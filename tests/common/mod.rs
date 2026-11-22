#![allow(dead_code)]

use lsm_kv::Options;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// A directory under the system temp dir, removed when dropped.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "lsmkv-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Tiny sizes, so a few thousand writes exercise flushes and compactions through several levels.
pub fn tiny() -> Options {
    Options {
        memtable_bytes: 4 << 10,
        table_bytes: 8 << 10,
        block_bytes: 512,
        bloom_bits_per_key: 10,
        l0_trigger: 2,
        level1_bytes: 32 << 10,
        level_multiplier: 4,
        sync: false,
    }
}

/// xorshift64*: a small deterministic generator, so the tests need no dependencies.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

pub fn key(i: u64) -> Vec<u8> {
    format!("key{i:06}").into_bytes()
}
