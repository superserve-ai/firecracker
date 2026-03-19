// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

/// A compact bitset tracking which disk blocks have been written (dirtied).
///
/// Each bit corresponds to one block. Bit N is set when block N has been
/// written to the upper layer of the overlay engine. Internally stored as
/// a `Vec<u64>` — 64 blocks per word, cache-friendly for the common scan
/// operations (dirty block count, snapshot serialization).
#[derive(Debug, Clone)]
pub struct DirtyBitmap {
    words: Vec<u64>,
    num_blocks: usize,
}

impl DirtyBitmap {
    /// Create a new bitmap for a disk with `num_blocks` blocks. All bits start
    /// clear (no dirty blocks).
    pub fn new(num_blocks: usize) -> Self {
        let num_words = num_blocks.div_ceil(64);
        Self {
            words: vec![0u64; num_words],
            num_blocks,
        }
    }

    /// Mark block `block_idx` as dirty.
    ///
    /// # Panics
    /// Panics in debug builds if `block_idx >= self.num_blocks`.
    #[inline]
    pub fn set(&mut self, block_idx: usize) {
        debug_assert!(block_idx < self.num_blocks, "block index out of range");
        self.words[block_idx / 64] |= 1u64 << (block_idx % 64);
    }

    /// Clear the dirty flag for block `block_idx`.
    ///
    /// # Panics
    /// Panics in debug builds if `block_idx >= self.num_blocks`.
    #[inline]
    pub fn clear(&mut self, block_idx: usize) {
        debug_assert!(block_idx < self.num_blocks, "block index out of range");
        self.words[block_idx / 64] &= !(1u64 << (block_idx % 64));
    }

    /// Returns `true` if block `block_idx` is dirty.
    ///
    /// # Panics
    /// Panics in debug builds if `block_idx >= self.num_blocks`.
    #[inline]
    pub fn is_set(&self, block_idx: usize) -> bool {
        debug_assert!(block_idx < self.num_blocks, "block index out of range");
        (self.words[block_idx / 64] >> (block_idx % 64)) & 1 == 1
    }

    /// Mark all blocks as clean. O(num_blocks / 64).
    pub fn clear_all(&mut self) {
        for word in &mut self.words {
            *word = 0;
        }
    }

    /// Return the number of dirty blocks. Used to estimate snapshot size.
    pub fn dirty_block_count(&self) -> usize {
        self.words
            .iter()
            .map(|w| w.count_ones() as usize)
            .sum()
    }

    /// Total number of blocks this bitmap covers.
    pub fn num_blocks(&self) -> usize {
        self.num_blocks
    }

    /// Iterate over the indices of all dirty blocks in ascending order.
    pub fn iter_dirty(&self) -> impl Iterator<Item = usize> + '_ {
        let num_blocks = self.num_blocks;
        self.words
            .iter()
            .enumerate()
            .flat_map(move |(word_idx, &word)| {
                (0..64u32).filter_map(move |bit| {
                    if (word >> bit) & 1 == 1 {
                        let block = word_idx * 64 + bit as usize;
                        // Exclude padding bits beyond num_blocks.
                        if block < num_blocks { Some(block) } else { None }
                    } else {
                        None
                    }
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ────────────────────────────────────────────────────────

    #[test]
    fn test_new_all_clean() {
        let bm = DirtyBitmap::new(128);
        assert_eq!(bm.dirty_block_count(), 0);
        for i in 0..128 {
            assert!(!bm.is_set(i));
        }
    }

    #[test]
    fn test_new_zero_blocks() {
        let bm = DirtyBitmap::new(0);
        assert_eq!(bm.dirty_block_count(), 0);
        assert_eq!(bm.num_blocks(), 0);
    }

    #[test]
    fn test_num_blocks() {
        let bm = DirtyBitmap::new(100);
        assert_eq!(bm.num_blocks(), 100);
    }

    // ── set / is_set ────────────────────────────────────────────────────────

    #[test]
    fn test_set_and_is_set() {
        let mut bm = DirtyBitmap::new(64);
        bm.set(0);
        assert!(bm.is_set(0));
        bm.set(63);
        assert!(bm.is_set(63));
    }

    #[test]
    fn test_set_is_idempotent() {
        let mut bm = DirtyBitmap::new(64);
        bm.set(5);
        bm.set(5);
        assert!(bm.is_set(5));
        assert_eq!(bm.dirty_block_count(), 1);
    }

    #[test]
    fn test_set_does_not_affect_neighbours() {
        let mut bm = DirtyBitmap::new(128);
        bm.set(10);
        assert!(!bm.is_set(9));
        assert!(!bm.is_set(11));
    }

    // ── clear ───────────────────────────────────────────────────────────────

    #[test]
    fn test_clear_dirty_block() {
        let mut bm = DirtyBitmap::new(64);
        bm.set(7);
        bm.clear(7);
        assert!(!bm.is_set(7));
        assert_eq!(bm.dirty_block_count(), 0);
    }

    #[test]
    fn test_clear_already_clean_is_noop() {
        let mut bm = DirtyBitmap::new(64);
        bm.clear(20); // no-op, should not panic
        assert!(!bm.is_set(20));
    }

    // ── clear_all ───────────────────────────────────────────────────────────

    #[test]
    fn test_clear_all() {
        let mut bm = DirtyBitmap::new(200);
        for i in (0..200).step_by(3) {
            bm.set(i);
        }
        assert!(bm.dirty_block_count() > 0);
        bm.clear_all();
        assert_eq!(bm.dirty_block_count(), 0);
        for i in 0..200 {
            assert!(!bm.is_set(i));
        }
    }

    // ── dirty_block_count ───────────────────────────────────────────────────

    #[test]
    fn test_dirty_block_count_increments() {
        let mut bm = DirtyBitmap::new(128);
        for i in 0..10 {
            bm.set(i);
            assert_eq!(bm.dirty_block_count(), i + 1);
        }
    }

    #[test]
    fn test_dirty_block_count_full() {
        let mut bm = DirtyBitmap::new(64);
        for i in 0..64 {
            bm.set(i);
        }
        assert_eq!(bm.dirty_block_count(), 64);
    }

    // ── Word boundary edge cases ─────────────────────────────────────────────

    #[test]
    fn test_word_boundary_block_63() {
        let mut bm = DirtyBitmap::new(128);
        bm.set(63); // last bit of first word
        assert!(bm.is_set(63));
        assert!(!bm.is_set(62));
        assert!(!bm.is_set(64));
    }

    #[test]
    fn test_word_boundary_block_64() {
        let mut bm = DirtyBitmap::new(128);
        bm.set(64); // first bit of second word
        assert!(bm.is_set(64));
        assert!(!bm.is_set(63));
        assert!(!bm.is_set(65));
    }

    #[test]
    fn test_non_multiple_of_64_num_blocks() {
        // 65 blocks → 2 words, but word[1] has only bit 0 valid
        let mut bm = DirtyBitmap::new(65);
        bm.set(64);
        assert!(bm.is_set(64));
        assert_eq!(bm.dirty_block_count(), 1);
        bm.clear_all();
        assert_eq!(bm.dirty_block_count(), 0);
    }

    // ── iter_dirty ──────────────────────────────────────────────────────────

    #[test]
    fn test_iter_dirty_empty() {
        let bm = DirtyBitmap::new(64);
        assert_eq!(bm.iter_dirty().count(), 0);
    }

    #[test]
    fn test_iter_dirty_single() {
        let mut bm = DirtyBitmap::new(64);
        bm.set(42);
        let dirty: Vec<usize> = bm.iter_dirty().collect();
        assert_eq!(dirty, vec![42]);
    }

    #[test]
    fn test_iter_dirty_multiple() {
        let mut bm = DirtyBitmap::new(200);
        let expected = vec![0usize, 63, 64, 127, 128, 199];
        for &i in &expected {
            bm.set(i);
        }
        let dirty: Vec<usize> = bm.iter_dirty().collect();
        assert_eq!(dirty, expected);
    }

    #[test]
    fn test_iter_dirty_excludes_padding_bits() {
        // 65 blocks: word[1] has 63 padding bits that must not appear.
        let mut bm = DirtyBitmap::new(65);
        bm.set(64);
        let dirty: Vec<usize> = bm.iter_dirty().collect();
        assert_eq!(dirty, vec![64]);
    }

    #[test]
    fn test_iter_dirty_count_matches_dirty_block_count() {
        let mut bm = DirtyBitmap::new(256);
        for i in (0..256).step_by(7) {
            bm.set(i);
        }
        assert_eq!(bm.iter_dirty().count(), bm.dirty_block_count());
    }

    // ── Clone ───────────────────────────────────────────────────────────────

    #[test]
    fn test_clone_is_independent() {
        let mut bm = DirtyBitmap::new(64);
        bm.set(1);
        let mut clone = bm.clone();
        clone.set(2);
        // Original should not see the clone's write.
        assert!(!bm.is_set(2));
        assert_eq!(bm.dirty_block_count(), 1);
        assert_eq!(clone.dirty_block_count(), 2);
    }
}
