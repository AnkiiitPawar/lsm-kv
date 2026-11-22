//! Throughput, amplification and Bloom-filter accuracy.
//!
//! cargo run --release --example bench > docs/bench.json

use lsm_kv::bloom::Bloom;
use lsm_kv::{Db, Options};
use std::time::Instant;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn key(i: u64) -> Vec<u8> {
    format!("user{i:012}").into_bytes()
}

fn main() {
    let n: u64 = 500_000;
    let value = vec![b'x'; 100];
    let dir = std::env::temp_dir().join(format!("lsmkv-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = Db::open(&dir, Options::default()).unwrap();
    let mut rng = Rng(42);

    // Random-order inserts: the case where an LSM's sequential writes pay off most.
    let mut amp_curve = Vec::new();
    let t = Instant::now();
    for i in 0..n {
        db.put(&key(rng.next() % (4 * n)), &value).unwrap();
        if (i + 1) % 25_000 == 0 {
            amp_curve.push(format!(
                "[{}, {:.3}]",
                i + 1,
                db.stats().write_amplification()
            ));
        }
    }
    db.flush().unwrap();
    let write_s = t.elapsed().as_secs_f64();
    let s = db.stats().clone();
    let levels: Vec<String> = db
        .levels()
        .iter()
        .map(|(t, b)| format!("[{t}, {b}]"))
        .collect();

    // Point reads of keys that exist, then of keys that don't.
    let mut rng = Rng(42);
    let present: Vec<Vec<u8>> = (0..n).map(|_| key(rng.next() % (4 * n))).collect();
    let before = db.stats().clone();
    let t = Instant::now();
    for k in present.iter().take(200_000) {
        assert!(db.get(k).unwrap().is_some());
    }
    let hit_s = t.elapsed().as_secs_f64();
    let a = db.stats().clone();
    // Absent keys from inside the key range, so range checks can't reject them: only the
    // Bloom filters can.
    let inserted: std::collections::HashSet<&Vec<u8>> = present.iter().collect();
    let mut rng = Rng(7);
    let mut absent = Vec::new();
    while absent.len() < 200_000 {
        let k = key(rng.next() % (4 * n));
        if !inserted.contains(&k) {
            absent.push(k);
        }
    }
    let a = db.stats().clone();
    let t = Instant::now();
    for k in &absent {
        assert!(db.get(k).unwrap().is_none());
    }
    let miss_s = t.elapsed().as_secs_f64();
    let b = db.stats().clone();

    // Bloom accuracy against theory, independent of the database.
    let keys: Vec<Vec<u8>> = (0..100_000u64).map(key).collect();
    let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
    let mut bloom_rows = Vec::new();
    for bits in [4usize, 6, 8, 10, 12, 16] {
        let f = Bloom::build(&refs, bits);
        let fp = (0..1_000_000u64)
            .filter(|i| f.may_contain(format!("absent{i}").as_bytes()))
            .count();
        let k = ((bits as f64) * std::f64::consts::LN_2)
            .round()
            .clamp(1.0, 30.0);
        let theory = (1.0 - (-k / bits as f64).exp()).powf(k);
        bloom_rows.push(format!(
            "[{bits}, {}, {:.6}, {:.6}]",
            k as u32,
            fp as f64 / 1e6,
            theory
        ));
    }

    let per = |x: u64, y: u64| (x - y) as f64 / 200_000.0;
    println!("{{");
    println!("  \"keys_written\": {n}, \"value_bytes\": 100,");
    println!(
        "  \"write_seconds\": {write_s:.3}, \"writes_per_second\": {:.0},",
        n as f64 / write_s
    );
    println!("  \"user_mb\": {:.1}, \"wal_mb\": {:.1}, \"table_mb_written\": {:.1}, \"write_amplification\": {:.2},",
             s.user_bytes as f64 / 1e6, s.wal_bytes as f64 / 1e6, s.table_bytes_written as f64 / 1e6, s.write_amplification());
    println!(
        "  \"flushes\": {}, \"compactions\": {},",
        s.flushes, s.compactions
    );
    println!("  \"levels\": [{}],", levels.join(", "));
    println!("  \"amplification_curve\": [{}],", amp_curve.join(", "));
    println!(
        "  \"get_hit_per_second\": {:.0}, \"get_miss_per_second\": {:.0},",
        200_000.0 / hit_s,
        200_000.0 / miss_s
    );
    println!("  \"hit_tables_consulted\": {:.3}, \"hit_bloom_rejections\": {:.3}, \"hit_blocks_read\": {:.3},",
             per(a.tables_consulted, before.tables_consulted), per(a.bloom_rejections, before.bloom_rejections),
             per(a.blocks_read, before.blocks_read));
    println!("  \"miss_tables_consulted\": {:.3}, \"miss_bloom_rejections\": {:.3}, \"miss_blocks_read\": {:.3},",
             per(b.tables_consulted, a.tables_consulted), per(b.bloom_rejections, a.bloom_rejections),
             per(b.blocks_read, a.blocks_read));
    println!("  \"bloom\": [{}]", bloom_rows.join(", "));
    println!("}}");
    let _ = std::fs::remove_dir_all(&dir);
}
