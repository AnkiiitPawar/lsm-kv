//! Random operations checked against a BTreeMap, with reopens in between.

mod common;

use common::{key, tiny, Rng, TempDir};
use lsm_kv::Db;
use std::collections::BTreeMap;

fn check_all(db: &mut Db, model: &BTreeMap<Vec<u8>, Vec<u8>>, n_keys: u64) {
    for i in 0..n_keys {
        assert_eq!(
            db.get(&key(i)).unwrap(),
            model.get(&key(i)).cloned(),
            "get(key{i})"
        );
    }
    let scan = db.scan(b"", b"\xff").unwrap();
    let expect: Vec<(Vec<u8>, Vec<u8>)> =
        model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(scan, expect, "full scan");
}

fn run(seed: u64, ops: usize, n_keys: u64, min_levels: usize) {
    let dir = TempDir::new("model");
    let mut db = Db::open(&dir.0, tiny()).unwrap();
    let mut model = BTreeMap::new();
    let mut rng = Rng(seed);
    for op in 0..ops {
        let k = key(rng.below(n_keys));
        match rng.below(100) {
            0..=59 => {
                let v = format!("v{op}-{}", "x".repeat(rng.below(64) as usize)).into_bytes();
                db.put(&k, &v).unwrap();
                model.insert(k, v);
            }
            60..=79 => {
                db.delete(&k).unwrap();
                model.remove(&k);
            }
            80..=96 => assert_eq!(db.get(&k).unwrap(), model.get(&k).cloned()),
            97 => {
                let lo = key(rng.below(n_keys));
                let hi = key(rng.below(n_keys));
                let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
                let got = db.scan(&lo, &hi).unwrap();
                let expect: Vec<(Vec<u8>, Vec<u8>)> = model
                    .range(lo..hi)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                assert_eq!(got, expect);
            }
            _ => {
                drop(db); // close and reopen: everything must survive
                db = Db::open(&dir.0, tiny()).unwrap();
            }
        }
    }
    check_all(&mut db, &model, n_keys);
    let levels = db.levels();
    assert!(
        levels.len() >= min_levels,
        "expected data in at least {min_levels} levels, got {levels:?}"
    );
    drop(db);
    let mut db = Db::open(&dir.0, tiny()).unwrap();
    check_all(&mut db, &model, n_keys);
}

#[test]
fn random_operations_match_a_btreemap_seed_1() {
    run(1, 30_000, 2_000, 3);
}

#[test]
fn random_operations_match_a_btreemap_seed_2() {
    run(2, 30_000, 500, 2); // fewer keys: many overwrites and deletes, and less live data
}

#[test]
fn deleted_keys_stay_deleted_after_compaction_to_the_bottom() {
    let dir = TempDir::new("tomb");
    let mut db = Db::open(&dir.0, tiny()).unwrap();
    for i in 0..3000 {
        db.put(&key(i), &[b'v'; 40]).unwrap();
    }
    for i in (0..3000).step_by(2) {
        db.delete(&key(i)).unwrap();
    }
    for i in 3000..6000 {
        db.put(&key(i), &[b'w'; 40]).unwrap(); // push the tombstones down through the levels
    }
    db.flush().unwrap();
    drop(db);
    let mut db = Db::open(&dir.0, tiny()).unwrap();
    for i in 0..3000 {
        assert_eq!(db.get(&key(i)).unwrap().is_some(), i % 2 == 1, "key{i}");
    }
    assert_eq!(db.scan(&key(0), &key(3000)).unwrap().len(), 1500);
}

#[test]
fn bloom_filters_spare_most_lookups_of_missing_keys() {
    let dir = TempDir::new("bloom");
    let mut db = Db::open(&dir.0, tiny()).unwrap();
    for i in 0..5000 {
        db.put(&key(2 * i), b"value").unwrap(); // even keys only
    }
    db.flush().unwrap();
    let before = db.stats().clone();
    for i in 0..5000 {
        assert_eq!(db.get(&key(2 * i + 1)).unwrap(), None); // odd keys: absent
    }
    let s = db.stats();
    let consulted = s.tables_consulted - before.tables_consulted;
    let rejected = s.bloom_rejections - before.bloom_rejections;
    assert!(consulted > 0);
    assert!(
        rejected as f64 / consulted as f64 > 0.97,
        "bloom rejected {rejected} of {consulted}"
    );
}
