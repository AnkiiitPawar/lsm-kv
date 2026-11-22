//! Sorted string tables: immutable files of sorted key/value entries.
//!
//! ```text
//! [data block]...[data block][bloom block][index block][footer: 48 bytes]
//! data block:  entries [key len u32][value len u32, or u32::MAX for a tombstone][key][value], then crc32
//! bloom block: [k u32][bits], then crc32
//! index block: per data block [last key len u32][last key][offset u64][length u32], then crc32
//! footer:      [index offset u64][index length u64][bloom offset u64][bloom length u64][entries u64][magic u64]
//! ```
//! A lookup checks the Bloom filter, binary-searches the index for the one block that could hold
//! the key, reads that block and verifies its checksum. A corrupt block is an error, never a
//! wrong answer.

use crate::bloom::Bloom;
use crate::codec::{corrupt, crc32, get_u32, get_u64, put_u32, put_u64};
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAGIC: u64 = 0x4C53_4D4B_5654_4142; // "LSMKVTAB"
const TOMBSTONE: u32 = u32::MAX;
const FOOTER: usize = 48;

/// A value as stored: `None` is a tombstone (the key was deleted).
pub type Value = Option<Vec<u8>>;
pub type Entry = (Vec<u8>, Value);

#[derive(Debug, Clone)]
pub struct TableMeta {
    pub id: u64,
    pub smallest: Vec<u8>,
    pub largest: Vec<u8>,
    pub size: u64,
    pub entries: u64,
}

pub fn table_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id:06}.sst"))
}

pub struct TableBuilder {
    out: BufWriter<File>,
    block: Vec<u8>,
    block_bytes: usize,
    bits_per_key: usize,
    index: Vec<(Vec<u8>, u64, u32)>,
    keys: Vec<Vec<u8>>,
    offset: u64,
    smallest: Option<Vec<u8>>,
    last: Vec<u8>,
    id: u64,
    path: PathBuf,
}

impl TableBuilder {
    pub fn create(
        dir: &Path,
        id: u64,
        block_bytes: usize,
        bits_per_key: usize,
    ) -> io::Result<TableBuilder> {
        let path = table_path(dir, id);
        Ok(TableBuilder {
            out: BufWriter::new(File::create(&path)?),
            block: Vec::new(),
            block_bytes,
            bits_per_key,
            index: Vec::new(),
            keys: Vec::new(),
            offset: 0,
            smallest: None,
            last: Vec::new(),
            id,
            path,
        })
    }

    /// Keys must arrive in strictly increasing order.
    pub fn add(&mut self, key: &[u8], value: Option<&[u8]>) -> io::Result<()> {
        debug_assert!(
            self.smallest.is_none() || key > self.last.as_slice(),
            "keys out of order"
        );
        if self.smallest.is_none() {
            self.smallest = Some(key.to_vec());
        }
        put_u32(&mut self.block, key.len() as u32);
        put_u32(&mut self.block, value.map_or(TOMBSTONE, |v| v.len() as u32));
        self.block.extend_from_slice(key);
        if let Some(v) = value {
            self.block.extend_from_slice(v);
        }
        self.keys.push(key.to_vec());
        self.last = key.to_vec();
        if self.block.len() >= self.block_bytes {
            self.flush_block()?;
        }
        Ok(())
    }

    /// Bytes written so far plus the pending block: what the finished file will roughly be.
    pub fn approx_size(&self) -> u64 {
        self.offset + self.block.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.smallest.is_none()
    }

    fn flush_block(&mut self) -> io::Result<()> {
        if self.block.is_empty() {
            return Ok(());
        }
        let crc = crc32(&self.block);
        put_u32(&mut self.block, crc);
        self.out.write_all(&self.block)?;
        self.index
            .push((self.last.clone(), self.offset, self.block.len() as u32));
        self.offset += self.block.len() as u64;
        self.block.clear();
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<TableMeta> {
        self.flush_block()?;
        let refs: Vec<&[u8]> = self.keys.iter().map(|k| k.as_slice()).collect();
        let mut bloom = Vec::new();
        Bloom::build(&refs, self.bits_per_key).encode(&mut bloom);
        let bloom_crc = crc32(&bloom);
        put_u32(&mut bloom, bloom_crc);
        let bloom_off = self.offset;
        self.out.write_all(&bloom)?;
        let mut index = Vec::new();
        for (key, off, len) in &self.index {
            put_u32(&mut index, key.len() as u32);
            index.extend_from_slice(key);
            put_u64(&mut index, *off);
            put_u32(&mut index, *len);
        }
        let index_crc = crc32(&index);
        put_u32(&mut index, index_crc);
        let index_off = bloom_off + bloom.len() as u64;
        self.out.write_all(&index)?;
        let mut footer = Vec::with_capacity(FOOTER);
        for v in [
            index_off,
            index.len() as u64,
            bloom_off,
            bloom.len() as u64,
            self.keys.len() as u64,
            MAGIC,
        ] {
            put_u64(&mut footer, v);
        }
        self.out.write_all(&footer)?;
        let file = self.out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        let size = index_off + index.len() as u64 + FOOTER as u64;
        let _ = &self.path;
        Ok(TableMeta {
            id: self.id,
            smallest: self.smallest.unwrap_or_default(),
            largest: self.last,
            size,
            entries: self.keys.len() as u64,
        })
    }
}

pub struct Table {
    pub meta: TableMeta,
    file: Mutex<File>,
    index: Vec<(Vec<u8>, u64, u32)>,
    bloom: Bloom,
}

fn read_at(file: &Mutex<File>, off: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut f = file.lock().unwrap();
    f.seek(SeekFrom::Start(off))?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn checked(mut block: Vec<u8>) -> io::Result<Vec<u8>> {
    if block.len() < 4 {
        return Err(corrupt("short block"));
    }
    let n = block.len() - 4;
    if get_u32(&block, n)? != crc32(&block[..n]) {
        return Err(corrupt("block checksum mismatch"));
    }
    block.truncate(n);
    Ok(block)
}

impl Table {
    pub fn open(dir: &Path, meta: TableMeta) -> io::Result<Arc<Table>> {
        let mut file = File::open(table_path(dir, meta.id))?;
        let len = file.metadata()?.len();
        if len < FOOTER as u64 {
            return Err(corrupt("table shorter than its footer"));
        }
        file.seek(SeekFrom::Start(len - FOOTER as u64))?;
        let mut footer = vec![0u8; FOOTER];
        file.read_exact(&mut footer)?;
        if get_u64(&footer, 40)? != MAGIC {
            return Err(corrupt("bad table magic"));
        }
        let (index_off, index_len) = (get_u64(&footer, 0)?, get_u64(&footer, 8)? as usize);
        let (bloom_off, bloom_len) = (get_u64(&footer, 16)?, get_u64(&footer, 24)? as usize);
        let file = Mutex::new(file);
        let bloom = Bloom::decode(&checked(read_at(&file, bloom_off, bloom_len)?)?)?;
        let raw = checked(read_at(&file, index_off, index_len)?)?;
        let mut index = Vec::new();
        let mut p = 0;
        while p < raw.len() {
            let klen = get_u32(&raw, p)? as usize;
            let key = raw
                .get(p + 4..p + 4 + klen)
                .ok_or_else(|| corrupt("index key"))?
                .to_vec();
            let off = get_u64(&raw, p + 4 + klen)?;
            let blen = get_u32(&raw, p + 12 + klen)?;
            index.push((key, off, blen));
            p += 16 + klen;
        }
        Ok(Arc::new(Table {
            meta,
            file,
            index,
            bloom,
        }))
    }

    pub fn may_contain(&self, key: &[u8]) -> bool {
        self.bloom.may_contain(key)
    }

    fn read_block(&self, i: usize) -> io::Result<Vec<Entry>> {
        let (_, off, len) = &self.index[i];
        let block = checked(read_at(&self.file, *off, *len as usize)?)?;
        let mut out = Vec::new();
        let mut p = 0;
        while p < block.len() {
            let klen = get_u32(&block, p)? as usize;
            let vlen = get_u32(&block, p + 4)?;
            let key = block
                .get(p + 8..p + 8 + klen)
                .ok_or_else(|| corrupt("entry key"))?
                .to_vec();
            p += 8 + klen;
            let value = if vlen == TOMBSTONE {
                None
            } else {
                let v = block
                    .get(p..p + vlen as usize)
                    .ok_or_else(|| corrupt("entry value"))?
                    .to_vec();
                p += vlen as usize;
                Some(v)
            };
            out.push((key, value));
        }
        Ok(out)
    }

    /// The one block that could hold `key`: the first whose last key is >= key.
    fn block_for(&self, key: &[u8]) -> Option<usize> {
        let i = self
            .index
            .partition_point(|(last, _, _)| last.as_slice() < key);
        (i < self.index.len()).then_some(i)
    }

    /// `Ok(None)`: not in this table. `Ok(Some(None))`: deleted here (a tombstone).
    pub fn get(&self, key: &[u8]) -> io::Result<Option<Value>> {
        let Some(i) = self.block_for(key) else {
            return Ok(None);
        };
        let entries = self.read_block(i)?;
        Ok(entries
            .binary_search_by(|(k, _)| k.as_slice().cmp(key))
            .ok()
            .map(|j| entries[j].1.clone()))
    }

    /// Iterate entries with key >= start, in order.
    pub fn iter_from(self: &Arc<Self>, start: &[u8]) -> TableIter {
        let block = self.block_for(start).unwrap_or(self.index.len());
        TableIter {
            table: self.clone(),
            block,
            entries: Vec::new(),
            pos: 0,
            start: start.to_vec(),
            failed: false,
        }
    }

    pub fn blocks(&self) -> usize {
        self.index.len()
    }
}

pub struct TableIter {
    table: Arc<Table>,
    block: usize,
    entries: Vec<Entry>,
    pos: usize,
    start: Vec<u8>,
    failed: bool,
}

impl Iterator for TableIter {
    type Item = io::Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.failed {
                return None;
            }
            if self.pos < self.entries.len() {
                let e = std::mem::take(&mut self.entries[self.pos]);
                self.pos += 1;
                if e.0 < self.start {
                    continue;
                }
                return Some(Ok(e));
            }
            if self.block >= self.table.index.len() {
                return None;
            }
            match self.table.read_block(self.block) {
                Ok(entries) => {
                    self.entries = entries;
                    self.pos = 0;
                    self.block += 1;
                }
                Err(e) => {
                    self.failed = true;
                    return Some(Err(e));
                }
            }
        }
    }
}
