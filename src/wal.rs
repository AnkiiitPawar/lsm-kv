//! Write-ahead log: every write is appended here before it touches the memtable, so a crash
//! loses nothing that was acknowledged (with `sync`, nothing at all; without it, nothing the
//! operating system had already written out).
//!
//! Record: [crc32 of the rest: u32][payload length: u32][payload]
//! Payload: [kind: u8 (1 = put, 2 = delete)][key length: u32][key][value length: u32][value]
//!
//! Replay stops at the first record that is incomplete or fails its checksum: that is a write
//! torn by the crash. The file is truncated there so new records never follow garbage.

use crate::codec::{corrupt, crc32, get_u32, put_u32};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

pub struct Wal {
    file: File,
    sync: bool,
    pub bytes_written: u64,
}

impl Wal {
    /// Open (creating if needed), replay what's valid, and truncate any torn tail.
    pub fn open(path: &Path, sync: bool) -> io::Result<(Wal, Vec<Record>)> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        let (records, valid) = decode_all(&data);
        if valid < data.len() {
            file.set_len(valid as u64)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::End(0))?;
        Ok((
            Wal {
                file,
                sync,
                bytes_written: 0,
            },
            records,
        ))
    }

    pub fn append(&mut self, rec: &Record) -> io::Result<()> {
        let mut payload = Vec::new();
        match rec {
            Record::Put(k, v) => {
                payload.push(1);
                put_u32(&mut payload, k.len() as u32);
                payload.extend_from_slice(k);
                put_u32(&mut payload, v.len() as u32);
                payload.extend_from_slice(v);
            }
            Record::Delete(k) => {
                payload.push(2);
                put_u32(&mut payload, k.len() as u32);
                payload.extend_from_slice(k);
            }
        }
        let mut body = Vec::with_capacity(payload.len() + 4);
        put_u32(&mut body, payload.len() as u32);
        body.extend_from_slice(&payload);
        let mut frame = Vec::with_capacity(body.len() + 4);
        put_u32(&mut frame, crc32(&body));
        frame.extend_from_slice(&body);
        self.file.write_all(&frame)?;
        if self.sync {
            self.file.sync_data()?;
        }
        self.bytes_written += frame.len() as u64;
        Ok(())
    }

    /// Empty the log once its contents are safely in an SSTable.
    pub fn reset(&mut self) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_all()
    }
}

/// Decode records until the first torn or corrupt one. Returns them and the valid length.
pub fn decode_all(data: &[u8]) -> (Vec<Record>, usize) {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Ok(rec_len) = decode_one(&data[pos..]).map(|(rec, n)| {
        out.push(rec);
        n
    }) {
        pos += rec_len;
    }
    (out, pos)
}

fn decode_one(buf: &[u8]) -> io::Result<(Record, usize)> {
    let crc = get_u32(buf, 0)?;
    let len = get_u32(buf, 4)? as usize;
    let body = buf.get(4..8 + len).ok_or_else(|| corrupt("torn record"))?;
    if crc32(body) != crc {
        return Err(corrupt("bad record checksum"));
    }
    let p = &body[4..];
    let kind = *p.first().ok_or_else(|| corrupt("empty record"))?;
    let klen = get_u32(p, 1)? as usize;
    let key = p.get(5..5 + klen).ok_or_else(|| corrupt("key"))?.to_vec();
    let rec = match kind {
        1 => {
            let vlen = get_u32(p, 5 + klen)? as usize;
            let val = p
                .get(9 + klen..9 + klen + vlen)
                .ok_or_else(|| corrupt("value"))?
                .to_vec();
            Record::Put(key, val)
        }
        2 => Record::Delete(key),
        _ => return Err(corrupt("unknown record kind")),
    };
    Ok((rec, 8 + len))
}
