//! Linked IDs: union-find over interned IDs; each person is named by its byte-wise smallest ID.
use crate::parse::{for_lines, hash, split, B};
use hashbrown::HashTable;
use rayon::prelude::*;

pub struct LinkMap {
    table: HashTable<(u64, u32)>,
    ids: Vec<B>,
    hashes: Vec<u64>,
    name_of: Vec<u32>,
}

impl LinkMap {
    /// The person name (and its hash) of a user ID whose hash is h.
    #[inline]
    pub fn name(&self, user: B, h: u64) -> (B, u64) {
        match self.table.find(h, |&(eh, i)| eh == h && self.ids[i as usize] == user) {
            Some(&(_, i)) => {
                let n = self.name_of[i as usize] as usize;
                (self.ids[n], self.hashes[n])
            }
            None => (user, h),
        }
    }
    pub fn len(&self) -> usize {
        self.ids.len()
    }
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    let mut r = x;
    while parent[r as usize] != r {
        r = parent[r as usize];
    }
    while parent[x as usize] != r {
        let n = parent[x as usize];
        parent[x as usize] = r;
        x = n;
    }
    r
}

pub fn build(chunks: &[B]) -> LinkMap {
    // Parse in parallel: valid lines with both sides non-empty, with hashes.
    let parts: Vec<Vec<(B, u64, B, u64)>> = chunks
        .par_iter()
        .map(|&c| {
            let mut v = Vec::new();
            for_lines(c, |line| {
                if let Some([a, b]) = split::<2>(line) {
                    if !a.is_empty() && !b.is_empty() {
                        v.push((a, hash(a), b, hash(b)));
                    }
                }
            });
            v
        })
        .collect();
    let n: usize = parts.iter().map(|p| p.len()).sum();
    let mut table: HashTable<(u64, u32)> = HashTable::with_capacity(n + 16);
    let mut ids: Vec<B> = Vec::with_capacity(n);
    let mut hashes: Vec<u64> = Vec::with_capacity(n);
    let mut parent: Vec<u32> = Vec::with_capacity(n);
    let intern = |s: B, h: u64, table: &mut HashTable<(u64, u32)>, ids: &mut Vec<B>, hashes: &mut Vec<u64>, parent: &mut Vec<u32>| -> u32 {
        if let Some(&(_, i)) = table.find(h, |&(eh, i)| eh == h && ids[i as usize] == s) {
            return i;
        }
        let i = ids.len() as u32;
        ids.push(s);
        hashes.push(h);
        parent.push(i);
        table.insert_unique(h, (h, i), |e| e.0);
        i
    };
    for p in &parts {
        for &(a, ha, b, hb) in p {
            let x = intern(a, ha, &mut table, &mut ids, &mut hashes, &mut parent);
            let y = intern(b, hb, &mut table, &mut ids, &mut hashes, &mut parent);
            let rx = find(&mut parent, x);
            let ry = find(&mut parent, y);
            if rx != ry {
                // Union toward the root with the smaller ID name keeps no invariant we rely on;
                // just link by index to keep trees shallow enough with path compression.
                if rx < ry { parent[ry as usize] = rx } else { parent[rx as usize] = ry }
            }
        }
    }
    let m = ids.len();
    let mut min_of: Vec<u32> = (0..m as u32).collect();
    let mut roots = vec![0u32; m];
    for i in 0..m as u32 {
        let r = find(&mut parent, i);
        roots[i as usize] = r;
        if ids[i as usize] < ids[min_of[r as usize] as usize] {
            min_of[r as usize] = i;
        }
    }
    let name_of: Vec<u32> = roots.iter().map(|&r| min_of[r as usize]).collect();
    LinkMap { table, ids, hashes, name_of }
}
