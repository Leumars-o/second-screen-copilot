//! In-memory vector index, one flat block per profile.
//!
//! sqlite-vec's `vec_chunks` is the durable copy of every embedding. The 0002 virtual table has no
//! profile column, so a vec0 KNN query can't be filtered by profile without over-fetching. Instead the
//! vectors are loaded once at open and searched exactly (brute-force cosine on L2-normalised vectors):
//! 10k × 384 floats is ~15 MB and a few ms per query.

use std::collections::HashMap;

#[derive(Debug, Default)]
struct Block {
    rowids: Vec<i64>,
    /// `rowids.len() * dim` floats, row-major.
    vecs: Vec<f32>,
}

#[derive(Debug)]
pub(crate) struct VectorIndex {
    dim: usize,
    profiles: HashMap<String, Block>,
}

pub(crate) fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    // Four independent accumulators: lets the compiler vectorise and keeps debug builds tolerable.
    let mut acc = [0f32; 4];
    let (ca, ra) = a.split_at(a.len() - a.len() % 4);
    let (cb, rb) = b.split_at(ca.len());
    for (x, y) in ca.chunks_exact(4).zip(cb.chunks_exact(4)) {
        acc[0] += x[0] * y[0];
        acc[1] += x[1] * y[1];
        acc[2] += x[2] * y[2];
        acc[3] += x[3] * y[3];
    }
    let tail: f32 = ra.iter().zip(rb).map(|(x, y)| x * y).sum();
    acc[0] + acc[1] + acc[2] + acc[3] + tail
}

impl VectorIndex {
    pub fn new(dim: usize) -> Self {
        Self { dim, profiles: HashMap::new() }
    }

    /// `vec` must already be normalised and `dim` long.
    pub fn insert(&mut self, profile: &str, rowid: i64, vec: &[f32]) {
        debug_assert_eq!(vec.len(), self.dim);
        let b = self.profiles.entry(profile.to_string()).or_default();
        b.rowids.push(rowid);
        b.vecs.extend_from_slice(vec);
    }

    pub fn remove(&mut self, rowids: &[i64]) {
        if rowids.is_empty() {
            return;
        }
        let gone: std::collections::HashSet<i64> = rowids.iter().copied().collect();
        let dim = self.dim;
        for b in self.profiles.values_mut() {
            let mut i = 0;
            while i < b.rowids.len() {
                if gone.contains(&b.rowids[i]) {
                    let last = b.rowids.len() - 1;
                    b.rowids.swap_remove(i);
                    if i != last {
                        b.vecs.copy_within(last * dim..(last + 1) * dim, i * dim);
                    }
                    b.vecs.truncate(last * dim);
                } else {
                    i += 1;
                }
            }
        }
        self.profiles.retain(|_, b| !b.rowids.is_empty());
    }

    pub fn remove_profile(&mut self, profile: &str) {
        self.profiles.remove(profile);
    }

    pub fn len(&self) -> usize {
        self.profiles.values().map(|b| b.rowids.len()).sum()
    }

    /// Top `k` (rowid, cosine) with cosine > `min_score`, best first.
    pub fn search(&self, profile: &str, query: &[f32], k: usize, min_score: f32) -> Vec<(i64, f32)> {
        let Some(b) = self.profiles.get(profile) else { return vec![] };
        if k == 0 {
            return vec![];
        }
        // Keep a small sorted top-k list: k is ~5, so insertion beats a heap.
        let mut top: Vec<(i64, f32)> = Vec::with_capacity(k + 1);
        for (i, v) in b.vecs.chunks_exact(self.dim).enumerate() {
            let s = dot(query, v);
            if s <= min_score || (top.len() == k && s <= top[k - 1].1) {
                continue;
            }
            let pos = top.partition_point(|&(_, t)| t >= s);
            top.insert(pos, (b.rowids[i], s));
            top.truncate(k);
        }
        top
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[f32]) -> Vec<f32> {
        let mut v = xs.to_vec();
        normalize(&mut v);
        v
    }

    #[test]
    fn search_ranks_filters_and_removes() {
        let mut ix = VectorIndex::new(3);
        ix.insert("a", 1, &v(&[1.0, 0.0, 0.0]));
        ix.insert("a", 2, &v(&[0.7, 0.7, 0.0]));
        ix.insert("a", 3, &v(&[0.0, 0.0, 1.0]));
        ix.insert("b", 4, &v(&[1.0, 0.0, 0.0]));
        let q = v(&[1.0, 0.1, 0.0]);
        let hits = ix.search("a", &q, 5, 0.0);
        assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), [1, 2], "orthogonal chunk excluded, b excluded");
        assert_eq!(ix.search("a", &q, 1, 0.0)[0].0, 1);
        ix.remove(&[1, 4]);
        assert_eq!(ix.len(), 2);
        assert_eq!(ix.search("a", &q, 5, 0.0)[0].0, 2);
        assert!(ix.search("b", &q, 5, 0.0).is_empty());
        ix.remove_profile("a");
        assert_eq!(ix.len(), 0);
    }

    #[test]
    fn dot_handles_tail() {
        assert_eq!(dot(&[1.0, 2.0, 3.0, 4.0, 5.0], &[1.0, 1.0, 1.0, 1.0, 2.0]), 20.0);
    }
}
