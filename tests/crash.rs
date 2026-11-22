//! Crash recovery: torn writes, corrupt logs, interrupted flushes and corrupt tables.

mod common;

use common::{key, tiny, TempDir};
use lsm_kv::{wal, Db, Options};
use std::fs;

fn big_memtable() -> Options {
    Options {
        memtable_bytes: 64 << 20,
        ..tiny()
    } // never flushes: everything stays in the WAL
}

/// Cut the WAL at every possible byte: the reopened database must hold exactly the records
/// that were completely written, in order, and nothing else.
#[test]
fn every_torn_wal_prefix_recovers_exactly_the_complete_records() {
    let dir = TempDir::new("torn");
    let n = 40;
    {
        let mut db = Db::open(&dir.0, big_memtable()).unwrap();
        for i in 0..n {
            if i % 5 == 4 {
                db.delete(&key(i - 1)).unwrap();
            } else {
                db.put(&key(i), format!("value-{i}").as_bytes()).unwrap();
            }
        }
    }
    let full = fs::read(dir.0.join("wal.log")).unwrap();
    let (all, end) = wal::decode_all(&full);
    assert_eq!((all.len(), end), (n as usize, full.len()));
    for cut in 0..=full.len() {
        let case = TempDir::new("torncase");
        fs::write(case.0.join("wal.log"), &full[..cut]).unwrap();
        let mut db = Db::open(&case.0, big_memtable()).unwrap();
        let (complete, _) = wal::decode_all(&full[..cut]);
        let mut model = std::collections::BTreeMap::new();
        for rec in &complete {
            match rec {
                wal::Record::Put(k, v) => {
                    model.insert(k.clone(), v.clone());
                }
                wal::Record::Delete(k) => {
                    model.remove(k);
                }
            }
        }
        let got = db.scan(b"", b"\xff").unwrap();
        let expect: Vec<_> = model.into_iter().collect();
        assert_eq!(got, expect, "cut at byte {cut}");
        // The torn tail was truncated, so new writes after recovery are readable too.
        db.put(b"after", b"crash").unwrap();
        drop(db);
        let mut db = Db::open(&case.0, big_memtable()).unwrap();
        assert_eq!(
            db.get(b"after").unwrap().as_deref(),
            Some(&b"crash"[..]),
            "cut at byte {cut}"
        );
    }
}

#[test]
fn a_corrupt_wal_record_drops_it_and_everything_after() {
    let dir = TempDir::new("corruptwal");
    {
        let mut db = Db::open(&dir.0, big_memtable()).unwrap();
        for i in 0..10 {
            db.put(&key(i), b"v").unwrap();
        }
    }
    let mut data = fs::read(dir.0.join("wal.log")).unwrap();
    let rec_len = data.len() / 10;
    data[5 * rec_len + 10] ^= 0x40; // damage record 5
    fs::write(dir.0.join("wal.log"), &data).unwrap();
    let mut db = Db::open(&dir.0, big_memtable()).unwrap();
    for i in 0..10 {
        assert_eq!(db.get(&key(i)).unwrap().is_some(), i < 5, "key{i}");
    }
}

/// A flush that wrote its table but crashed before updating the manifest leaves an orphan
/// table and a full WAL. Recovery must delete the orphan and replay the log.
#[test]
fn an_interrupted_flush_loses_nothing() {
    let dir = TempDir::new("flush");
    {
        let mut db = Db::open(&dir.0, big_memtable()).unwrap();
        for i in 0..100 {
            db.put(&key(i), b"v").unwrap();
        }
    }
    fs::write(dir.0.join("000999.sst"), b"half a table").unwrap();
    fs::write(dir.0.join("MANIFEST.tmp"), b"table 0 999 garbage").unwrap();
    let mut db = Db::open(&dir.0, big_memtable()).unwrap();
    assert!(
        !dir.0.join("000999.sst").exists(),
        "orphan table not removed"
    );
    for i in 0..100 {
        assert!(db.get(&key(i)).unwrap().is_some());
    }
}

/// Damage inside a table must surface as an error, never as a wrong answer.
#[test]
fn a_corrupt_table_block_is_reported_not_misread() {
    let dir = TempDir::new("corrupttable");
    {
        let mut db = Db::open(&dir.0, tiny()).unwrap();
        for i in 0..200 {
            db.put(&key(i), &[b'v'; 30]).unwrap();
        }
        db.flush().unwrap();
    }
    let table = fs::read_dir(&dir.0)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "sst"))
        .unwrap();
    let mut data = fs::read(&table).unwrap();
    data[100] ^= 0xFF; // inside the first data block
    fs::write(&table, &data).unwrap();
    let mut db = Db::open(&dir.0, tiny()).unwrap();
    let mut errors = 0;
    for i in 0..200 {
        match db.get(&key(i)) {
            Ok(v) => assert!(
                v.is_none() || v.as_deref() == Some(&[b'v'; 30][..]),
                "wrong value for key{i}"
            ),
            Err(e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
                errors += 1;
            }
        }
    }
    assert!(errors > 0, "the damaged block was never detected");
}
