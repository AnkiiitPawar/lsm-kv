//! K-way merge of sorted sources, where a lower source index means newer data.
//!
//! The heap holds the head of every source. When several sources hold the same key, the newest
//! version wins and the older ones are skipped, which is how overwrites and deletes shadow older
//! data in reads, scans and compactions.

use crate::sstable::Entry;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io;

pub type Source = Box<dyn Iterator<Item = io::Result<Entry>>>;

pub struct Merge {
    sources: Vec<Source>,
    heads: Vec<Option<Entry>>,
    heap: BinaryHeap<Reverse<(Vec<u8>, usize)>>,
    error: Option<io::Error>,
}

impl Merge {
    pub fn new(sources: Vec<Source>) -> Merge {
        let n = sources.len();
        let mut m = Merge {
            sources,
            heads: (0..n).map(|_| None).collect(),
            heap: BinaryHeap::new(),
            error: None,
        };
        for i in 0..n {
            m.advance(i);
        }
        m
    }

    fn advance(&mut self, i: usize) {
        match self.sources[i].next() {
            Some(Ok(e)) => {
                self.heap.push(Reverse((e.0.clone(), i)));
                self.heads[i] = Some(e);
            }
            Some(Err(e)) => {
                self.error.get_or_insert(e);
                self.heads[i] = None;
            }
            None => self.heads[i] = None,
        }
    }
}

impl Iterator for Merge {
    type Item = io::Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(e) = self.error.take() {
            return Some(Err(e));
        }
        let Reverse((key, src)) = self.heap.pop()?;
        let entry = self.heads[src].take().expect("heap and heads agree");
        self.advance(src);
        // Older sources holding the same key are shadowed: drop them.
        while let Some(Reverse((k, s))) = self.heap.peek() {
            if *k != key {
                break;
            }
            let s = *s;
            self.heap.pop();
            self.heads[s] = None;
            self.advance(s);
        }
        if let Some(e) = self.error.take() {
            return Some(Err(e));
        }
        Some(Ok(entry))
    }
}
