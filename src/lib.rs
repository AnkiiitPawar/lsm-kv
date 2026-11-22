//! A log-structured merge-tree key-value store.
//!
//! Writes go to a write-ahead log and an in-memory sorted map (the memtable). A full memtable is
//! written out as an immutable sorted table in level 0. Too many level-0 tables are merged into
//! level 1, and a level that outgrows its size budget pushes a table's worth of data into the
//! next one (leveled compaction, as in LevelDB). Reads check the memtable, then level 0 newest
//! first, then at most one table per deeper level, skipping tables whose Bloom filter rules the
//! key out. The set of live tables is recorded in a manifest that is replaced atomically, so a
//! crash at any point leaves a consistent database.

pub mod bloom;
pub mod codec;
pub mod merge;
pub mod sstable;
pub mod wal;

use merge::{Merge, Source};
use sstable::{table_path, Entry, Table, TableBuilder, TableMeta, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wal::{Record, Wal};

#[derive(Debug, Clone)]
pub struct Options {
    /// Flush the memtable once it holds about this many bytes.
    pub memtable_bytes: usize,
    /// Target size of each table written by compaction.
    pub table_bytes: u64,
    pub block_bytes: usize,
    pub bloom_bits_per_key: usize,
    /// Compact level 0 into level 1 once it has more tables than this.
    pub l0_trigger: usize,
    /// Size budget of level 1; each deeper level gets `level_multiplier` times more.
    pub level1_bytes: u64,
    pub level_multiplier: u64,
    /// fsync the WAL after every write.
    pub sync: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            memtable_bytes: 4 << 20,
            table_bytes: 2 << 20,
            block_bytes: 4096,
            bloom_bits_per_key: 10,
            l0_trigger: 4,
            level1_bytes: 10 << 20,
            level_multiplier: 10,
            sync: false,
        }
    }
}

/// Counters for reasoning about amplification.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub user_bytes: u64,
    pub wal_bytes: u64,
    pub table_bytes_written: u64,
    pub flushes: u64,
    pub compactions: u64,
    pub gets: u64,
    pub tables_consulted: u64,
    pub bloom_rejections: u64,
    pub blocks_read: u64,
}

impl Stats {
    /// Bytes written to disk per byte of user data.
    pub fn write_amplification(&self) -> f64 {
        (self.wal_bytes + self.table_bytes_written) as f64 / self.user_bytes.max(1) as f64
    }
}

pub struct Db {
    dir: PathBuf,
    opts: Options,
    wal: Wal,
    mem: BTreeMap<Vec<u8>, Value>,
    mem_bytes: usize,
    /// levels[0] is newest first and may overlap; deeper levels are sorted and disjoint.
    levels: Vec<Vec<Arc<Table>>>,
    next_id: u64,
    compact_ptr: Vec<Vec<u8>>,
    stats: Stats,
}

const MAX_LEVELS: usize = 7;

impl Db {
    pub fn open(dir: impl AsRef<Path>, opts: Options) -> io::Result<Db> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let (metas, next_id) = read_manifest(&dir)?;
        let mut levels: Vec<Vec<Arc<Table>>> = vec![Vec::new(); MAX_LEVELS];
        let mut live = std::collections::HashSet::new();
        for (level, meta) in metas {
            live.insert(meta.id);
            levels[level].push(Table::open(&dir, meta)?);
        }
        for level in levels.iter_mut().skip(1) {
            level.sort_by(|a, b| a.meta.smallest.cmp(&b.meta.smallest));
        }
        // Tables written by a flush or compaction that crashed before the manifest named them.
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "sst") {
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<u64>().ok());
                if id.is_none_or(|id| !live.contains(&id)) {
                    fs::remove_file(&path)?;
                }
            }
        }
        let (wal, records) = Wal::open(&dir.join("wal.log"), opts.sync)?;
        let mut db = Db {
            dir,
            opts,
            wal,
            mem: BTreeMap::new(),
            mem_bytes: 0,
            levels,
            next_id,
            compact_ptr: vec![Vec::new(); MAX_LEVELS],
            stats: Stats::default(),
        };
        for rec in records {
            match rec {
                Record::Put(k, v) => db.mem_insert(k, Some(v)),
                Record::Delete(k) => db.mem_insert(k, None),
            }
        }
        Ok(db)
    }

    fn mem_insert(&mut self, key: Vec<u8>, value: Value) {
        self.mem_bytes += key.len() + value.as_ref().map_or(0, |v| v.len()) + 32;
        self.mem.insert(key, value);
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        self.write(key, Some(value))
    }

    pub fn delete(&mut self, key: &[u8]) -> io::Result<()> {
        self.write(key, None)
    }

    fn write(&mut self, key: &[u8], value: Option<&[u8]>) -> io::Result<()> {
        let rec = match value {
            Some(v) => Record::Put(key.to_vec(), v.to_vec()),
            None => Record::Delete(key.to_vec()),
        };
        let before = self.wal.bytes_written;
        self.wal.append(&rec)?;
        self.stats.wal_bytes += self.wal.bytes_written - before;
        self.stats.user_bytes += (key.len() + value.map_or(0, |v| v.len())) as u64;
        self.mem_insert(key.to_vec(), value.map(|v| v.to_vec()));
        if self.mem_bytes >= self.opts.memtable_bytes {
            self.flush()?;
        }
        Ok(())
    }

    pub fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        self.stats.gets += 1;
        if let Some(v) = self.mem.get(key) {
            return Ok(v.clone());
        }
        for level in 0..self.levels.len() {
            let candidates: Vec<Arc<Table>> = if level == 0 {
                self.levels[0]
                    .iter()
                    .filter(|t| covers(t, key))
                    .cloned()
                    .collect()
            } else {
                let tables = &self.levels[level];
                let i = tables.partition_point(|t| t.meta.largest.as_slice() < key);
                tables
                    .get(i)
                    .filter(|t| covers(t, key))
                    .cloned()
                    .into_iter()
                    .collect()
            };
            for t in candidates {
                self.stats.tables_consulted += 1;
                if !t.may_contain(key) {
                    self.stats.bloom_rejections += 1;
                    continue;
                }
                self.stats.blocks_read += 1;
                if let Some(v) = t.get(key)? {
                    return Ok(v);
                }
            }
        }
        Ok(None)
    }

    /// All live entries with start <= key < end, in key order.
    pub fn scan(&mut self, start: &[u8], end: &[u8]) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mem: Vec<io::Result<Entry>> = self
            .mem
            .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end)))
            .map(|(k, v)| Ok((k.clone(), v.clone())))
            .collect();
        let mut sources: Vec<Source> = vec![Box::new(mem.into_iter())];
        for t in &self.levels[0] {
            sources.push(Box::new(t.iter_from(start)));
        }
        for level in self.levels.iter().skip(1) {
            let tables: Vec<Arc<Table>> = level
                .iter()
                .filter(|t| t.meta.largest.as_slice() >= start && t.meta.smallest.as_slice() < end)
                .cloned()
                .collect();
            let s = start.to_vec();
            sources.push(Box::new(
                tables.into_iter().flat_map(move |t| t.iter_from(&s)),
            ));
        }
        let mut out = Vec::new();
        for item in Merge::new(sources) {
            let (k, v) = item?;
            if k.as_slice() >= end {
                break;
            }
            if let Some(v) = v {
                out.push((k, v));
            }
        }
        Ok(out)
    }

    /// Write the memtable to a level-0 table, then compact if needed.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.mem.is_empty() {
            return Ok(());
        }
        let id = self.alloc_id();
        let mut b = TableBuilder::create(
            &self.dir,
            id,
            self.opts.block_bytes,
            self.opts.bloom_bits_per_key,
        )?;
        for (k, v) in &self.mem {
            b.add(k, v.as_deref())?;
        }
        let meta = b.finish()?;
        self.stats.table_bytes_written += meta.size;
        self.stats.flushes += 1;
        self.levels[0].insert(0, Table::open(&self.dir, meta)?);
        self.write_manifest()?; // the table is durable and named: now the log can go
        self.wal.reset()?;
        self.mem.clear();
        self.mem_bytes = 0;
        self.maybe_compact()
    }

    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn budget(&self, level: usize) -> u64 {
        self.opts.level1_bytes * self.opts.level_multiplier.pow(level as u32 - 1)
    }

    fn level_bytes(&self, level: usize) -> u64 {
        self.levels[level].iter().map(|t| t.meta.size).sum()
    }

    fn maybe_compact(&mut self) -> io::Result<()> {
        loop {
            if self.levels[0].len() > self.opts.l0_trigger {
                let inputs: Vec<Arc<Table>> = self.levels[0].clone();
                self.compact(0, inputs)?;
                continue;
            }
            let over = (1..MAX_LEVELS - 1).find(|&l| self.level_bytes(l) > self.budget(l));
            let Some(level) = over else { return Ok(()) };
            // Round-robin through the level so every key range gets pushed down in turn.
            let tables = &self.levels[level];
            let ptr = &self.compact_ptr[level];
            let pick = tables
                .iter()
                .position(|t| t.meta.smallest.as_slice() > ptr.as_slice())
                .unwrap_or(0);
            let t = tables[pick].clone();
            self.compact_ptr[level] = t.meta.largest.clone();
            self.compact(level, vec![t])?;
        }
    }

    /// Merge `inputs` (from `level`) with the overlapping tables of `level + 1` into new tables
    /// in `level + 1`.
    fn compact(&mut self, level: usize, inputs: Vec<Arc<Table>>) -> io::Result<()> {
        let out_level = level + 1;
        let lo = inputs
            .iter()
            .map(|t| t.meta.smallest.clone())
            .min()
            .unwrap();
        let hi = inputs.iter().map(|t| t.meta.largest.clone()).max().unwrap();
        let overlapping: Vec<Arc<Table>> = self.levels[out_level]
            .iter()
            .filter(|t| t.meta.largest >= lo && t.meta.smallest <= hi)
            .cloned()
            .collect();
        // A tombstone can be dropped once nothing older could exist below the output level.
        let bottom = self.levels[out_level + 1..].iter().all(|l| l.is_empty());
        let mut sources: Vec<Source> = inputs
            .iter()
            .map(|t| Box::new(t.iter_from(b"")) as Source)
            .collect();
        let older: Vec<Arc<Table>> = overlapping.clone();
        sources.push(Box::new(older.into_iter().flat_map(|t| t.iter_from(b""))));

        let mut outputs = Vec::new();
        let mut builder: Option<TableBuilder> = None;
        for item in Merge::new(sources) {
            let (k, v) = item?;
            if v.is_none() && bottom {
                continue;
            }
            if builder.is_none() {
                let id = self.alloc_id();
                builder = Some(TableBuilder::create(
                    &self.dir,
                    id,
                    self.opts.block_bytes,
                    self.opts.bloom_bits_per_key,
                )?);
            }
            let b = builder.as_mut().unwrap();
            b.add(&k, v.as_deref())?;
            if b.approx_size() >= self.opts.table_bytes {
                outputs.push(builder.take().unwrap().finish()?);
            }
        }
        if let Some(b) = builder {
            if !b.is_empty() {
                outputs.push(b.finish()?);
            }
        }
        let mut new_tables = Vec::new();
        for meta in outputs {
            self.stats.table_bytes_written += meta.size;
            new_tables.push(Table::open(&self.dir, meta)?);
        }
        let dead: std::collections::HashSet<u64> = inputs
            .iter()
            .chain(&overlapping)
            .map(|t| t.meta.id)
            .collect();
        for l in [level, out_level] {
            self.levels[l].retain(|t| !dead.contains(&t.meta.id));
        }
        self.levels[out_level].extend(new_tables);
        self.levels[out_level].sort_by(|a, b| a.meta.smallest.cmp(&b.meta.smallest));
        self.write_manifest()?; // switch over atomically, then delete the inputs
        for id in dead {
            let _ = fs::remove_file(table_path(&self.dir, id));
        }
        self.stats.compactions += 1;
        Ok(())
    }

    fn write_manifest(&self) -> io::Result<()> {
        let mut text = format!("next_id {}\n", self.next_id);
        for (level, tables) in self.levels.iter().enumerate() {
            for t in tables {
                let m = &t.meta;
                text += &format!(
                    "table {level} {} {} {} {} {}\n",
                    m.id,
                    m.size,
                    m.entries,
                    hex(&m.smallest),
                    hex(&m.largest)
                );
            }
        }
        let tmp = self.dir.join("MANIFEST.tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, self.dir.join("MANIFEST"))?; // atomic replacement on POSIX file systems
        if let Ok(d) = fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// (number of tables, bytes) per non-empty level.
    pub fn levels(&self) -> Vec<(usize, u64)> {
        let deepest = self
            .levels
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(0, |d| d + 1);
        (0..deepest)
            .map(|l| (self.levels[l].len(), self.level_bytes(l)))
            .collect()
    }
}

fn covers(t: &Table, key: &[u8]) -> bool {
    t.meta.smallest.as_slice() <= key && key <= t.meta.largest.as_slice()
}

fn hex(b: &[u8]) -> String {
    if b.is_empty() {
        return "-".into();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> io::Result<Vec<u8>> {
    if s == "-" {
        return Ok(Vec::new());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(s.get(i..i + 2).unwrap_or("zz"), 16)
                .map_err(|_| codec::corrupt("manifest hex"))
        })
        .collect()
}

fn read_manifest(dir: &Path) -> io::Result<(Vec<(usize, TableMeta)>, u64)> {
    let text = match fs::read_to_string(dir.join("MANIFEST")) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e),
    };
    let mut next_id = 0;
    let mut tables = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            ["next_id", n] => {
                next_id = n.parse().map_err(|_| codec::corrupt("manifest next_id"))?
            }
            ["table", level, id, size, entries, lo, hi] => {
                let p = |s: &str| {
                    s.parse::<u64>()
                        .map_err(|_| codec::corrupt("manifest number"))
                };
                let meta = TableMeta {
                    id: p(id)?,
                    size: p(size)?,
                    entries: p(entries)?,
                    smallest: unhex(lo)?,
                    largest: unhex(hi)?,
                };
                tables.push((p(level)? as usize, meta));
            }
            [] => {}
            _ => return Err(codec::corrupt("manifest line")),
        }
    }
    Ok((tables, next_id))
}
