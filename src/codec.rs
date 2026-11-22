//! Checksums, hashing and little-endian helpers shared by the on-disk formats.

use std::io;

/// CRC-32 (IEEE 802.3, reflected, polynomial 0xEDB88320), the checksum used by zlib and
/// Ethernet. Every WAL record and every SSTable block carries one.
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        t
    });
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// 64-bit FNV-1a followed by the splitmix64 finaliser. Written out here (rather than using
/// std's hasher) because Bloom filters live on disk and need a hash that never changes.
pub fn hash64(data: &[u8], seed: u64) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

pub fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn get_u32(buf: &[u8], at: usize) -> io::Result<u32> {
    buf.get(at..at + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| corrupt("truncated u32"))
}

pub fn get_u64(buf: &[u8], at: usize) -> io::Result<u64> {
    buf.get(at..at + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| corrupt("truncated u64"))
}

pub fn corrupt(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("corruption: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_values() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926); // the standard check value
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
    }

    #[test]
    fn hash_is_stable_and_seeded() {
        assert_eq!(hash64(b"key", 0), hash64(b"key", 0));
        assert_ne!(hash64(b"key", 0), hash64(b"key", 1));
        assert_ne!(hash64(b"key1", 0), hash64(b"key2", 0));
    }
}
