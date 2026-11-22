//! Bloom filter: a compact set that can say "definitely absent" or "maybe present".
//!
//! With m bits, n keys and k hash functions, the false-positive rate is about
//! (1 - e^(-kn/m))^k, minimised at k = (m/n) ln 2. At 10 bits per key that is k = 7 and
//! roughly 0.8 %. The k probe positions come from double hashing, gᵢ = h₁ + i·h₂
//! (Kirsch & Mitzenmacher, 2006), so each key is hashed only twice.

use crate::codec::{corrupt, get_u32, hash64, put_u32};
use std::io;

#[derive(Debug, Clone)]
pub struct Bloom {
    bits: Vec<u8>,
    k: u32,
}

impl Bloom {
    pub fn build(keys: &[&[u8]], bits_per_key: usize) -> Bloom {
        let m = (keys.len() * bits_per_key).max(64);
        let k = ((bits_per_key as f64) * std::f64::consts::LN_2)
            .round()
            .clamp(1.0, 30.0) as u32;
        let mut bloom = Bloom {
            bits: vec![0; m.div_ceil(8)],
            k,
        };
        for key in keys {
            bloom.insert(key);
        }
        bloom
    }

    fn positions(&self, key: &[u8]) -> impl Iterator<Item = usize> + '_ {
        let m = (self.bits.len() * 8) as u64;
        let h1 = hash64(key, 0);
        let h2 = hash64(key, 1) | 1;
        (0..self.k as u64).map(move |i| (h1.wrapping_add(i.wrapping_mul(h2)) % m) as usize)
    }

    fn insert(&mut self, key: &[u8]) {
        let pos: Vec<usize> = self.positions(key).collect();
        for p in pos {
            self.bits[p / 8] |= 1 << (p % 8);
        }
    }

    pub fn may_contain(&self, key: &[u8]) -> bool {
        self.positions(key)
            .all(|p| self.bits[p / 8] & (1 << (p % 8)) != 0)
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        put_u32(out, self.k);
        out.extend_from_slice(&self.bits);
    }

    pub fn decode(buf: &[u8]) -> io::Result<Bloom> {
        let k = get_u32(buf, 0)?;
        if k == 0 || k > 30 || buf.len() <= 4 {
            return Err(corrupt("bad bloom filter"));
        }
        Ok(Bloom {
            bits: buf[4..].to_vec(),
            k,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_false_negatives_and_the_expected_false_positive_rate() {
        let keys: Vec<Vec<u8>> = (0..10_000u32)
            .map(|i| format!("key{i}").into_bytes())
            .collect();
        let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        let bloom = Bloom::build(&refs, 10);
        assert!(keys.iter().all(|k| bloom.may_contain(k)));
        let fp = (0..100_000u32)
            .filter(|i| bloom.may_contain(format!("absent{i}").as_bytes()))
            .count();
        let rate = fp as f64 / 100_000.0;
        let theory = (1.0 - (-7.0f64 * 10_000.0 / 100_000.0).exp()).powi(7);
        assert!(
            rate < 2.0 * theory,
            "false-positive rate {rate} vs theory {theory}"
        );
    }

    #[test]
    fn round_trip() {
        let bloom = Bloom::build(&[b"a", b"b"], 10);
        let mut buf = Vec::new();
        bloom.encode(&mut buf);
        let back = Bloom::decode(&buf).unwrap();
        assert!(back.may_contain(b"a") && back.may_contain(b"b"));
    }
}
