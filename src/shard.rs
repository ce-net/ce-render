//! Frame-range sharding — splitting one render job into independent units of work.
//!
//! A render job covers a contiguous, inclusive frame range (`start..=end`, e.g. a Blender
//! `--frames 1-500` or a batch of N screenshots indexed `0..=N-1`). We split it into shards of at
//! most `shard_size` frames each, every shard a self-contained unit placed on one mesh host. The
//! whole module is network-free and pure so it is exhaustively unit-testable without a cluster.
//!
//! The contract that makes reassembly correct: shards **partition** the range — they are contiguous,
//! non-overlapping, gap-free, and their union is exactly the original range. The unit tests assert
//! this invariant directly.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// A contiguous, inclusive frame range. `start <= end` always holds for a valid job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRange {
    pub start: u32,
    pub end: u32,
}

impl FrameRange {
    /// Construct a validated inclusive range. Errors if `end < start`.
    pub fn new(start: u32, end: u32) -> Result<Self> {
        if end < start {
            bail!("invalid frame range {start}-{end}: end is before start");
        }
        Ok(FrameRange { start, end })
    }

    /// Parse a `START-END` (inclusive) or single-frame `N` spec, e.g. `1-500` or `42`.
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        match spec.split_once('-') {
            Some((a, b)) => {
                let start = a.trim().parse().map_err(|_| anyhow::anyhow!("bad frame start in '{spec}'"))?;
                let end = b.trim().parse().map_err(|_| anyhow::anyhow!("bad frame end in '{spec}'"))?;
                Self::new(start, end)
            }
            None => {
                let n = spec.parse().map_err(|_| anyhow::anyhow!("bad frame number '{spec}'"))?;
                Self::new(n, n)
            }
        }
    }

    /// Total number of frames in the range (inclusive). Always >= 1 for a valid range.
    pub fn count(&self) -> u32 {
        self.end - self.start + 1
    }
}

impl std::fmt::Display for FrameRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}-{}", self.start, self.end)
        }
    }
}

/// One unit of render work: a contiguous sub-range placed on a single host. `index` is the shard's
/// position in the job (0-based, stable across runs) so results can be re-ordered deterministically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shard {
    pub index: u32,
    pub range: FrameRange,
}

impl Shard {
    /// Number of frames this shard renders.
    pub fn count(&self) -> u32 {
        self.range.count()
    }
}

/// Split `range` into shards of at most `shard_size` consecutive frames. The shards partition the
/// range exactly (contiguous, non-overlapping, gap-free, union == range). `shard_size == 0` is
/// treated as `1` (one frame per shard) rather than erroring, so a degenerate config still produces
/// valid work.
pub fn split(range: FrameRange, shard_size: u32) -> Vec<Shard> {
    let size = shard_size.max(1);
    let mut shards = Vec::new();
    let mut cursor = range.start;
    let mut index = 0u32;
    while cursor <= range.end {
        // `end` is inclusive; guard against overflow when cursor + size - 1 would exceed u32::MAX.
        let span = (size - 1).min(range.end - cursor);
        let shard_end = cursor + span;
        shards.push(Shard {
            index,
            range: FrameRange { start: cursor, end: shard_end },
        });
        index += 1;
        if shard_end == range.end {
            break;
        }
        cursor = shard_end + 1;
    }
    shards
}

/// How many shards `split` will produce for a range — without allocating them. Useful for sizing
/// progress bars / channel capacity up front.
pub fn shard_count(range: FrameRange, shard_size: u32) -> u32 {
    let size = shard_size.max(1);
    range.count().div_ceil(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_range_and_single() {
        assert_eq!(FrameRange::parse("1-500").unwrap(), FrameRange { start: 1, end: 500 });
        assert_eq!(FrameRange::parse(" 42 ").unwrap(), FrameRange { start: 42, end: 42 });
        assert_eq!(FrameRange::parse("0-0").unwrap(), FrameRange { start: 0, end: 0 });
    }

    #[test]
    fn parse_rejects_inverted_and_garbage() {
        assert!(FrameRange::parse("500-1").is_err());
        assert!(FrameRange::parse("a-b").is_err());
        assert!(FrameRange::parse("1-").is_err());
        assert!(FrameRange::parse("").is_err());
    }

    #[test]
    fn count_is_inclusive() {
        assert_eq!(FrameRange::new(1, 500).unwrap().count(), 500);
        assert_eq!(FrameRange::new(7, 7).unwrap().count(), 1);
    }

    #[test]
    fn display_collapses_single_frame() {
        assert_eq!(FrameRange { start: 5, end: 5 }.to_string(), "5");
        assert_eq!(FrameRange { start: 1, end: 10 }.to_string(), "1-10");
    }

    /// The load-bearing invariant: shards partition the range exactly.
    fn assert_partitions(range: FrameRange, shards: &[Shard]) {
        assert!(!shards.is_empty(), "a non-empty range must yield at least one shard");
        // indices are 0..n contiguous
        for (i, s) in shards.iter().enumerate() {
            assert_eq!(s.index as usize, i, "shard indices must be contiguous 0-based");
        }
        // first starts at range.start, last ends at range.end
        assert_eq!(shards.first().unwrap().range.start, range.start);
        assert_eq!(shards.last().unwrap().range.end, range.end);
        // contiguous, non-overlapping, gap-free
        for w in shards.windows(2) {
            assert_eq!(w[1].range.start, w[0].range.end + 1, "shards must be contiguous and gap-free");
        }
        // union frame count equals range count
        let total: u32 = shards.iter().map(|s| s.count()).sum();
        assert_eq!(total, range.count(), "shard frame counts must sum to the range count");
    }

    #[test]
    fn split_even_division() {
        let r = FrameRange::new(1, 100).unwrap();
        let shards = split(r, 10);
        assert_eq!(shards.len(), 10);
        assert_eq!(shards[0].range, FrameRange { start: 1, end: 10 });
        assert_eq!(shards[9].range, FrameRange { start: 91, end: 100 });
        assert_partitions(r, &shards);
    }

    #[test]
    fn split_uneven_last_shard_short() {
        let r = FrameRange::new(1, 25).unwrap();
        let shards = split(r, 10);
        assert_eq!(shards.len(), 3);
        assert_eq!(shards[2].range, FrameRange { start: 21, end: 25 }, "last shard holds the remainder");
        assert_partitions(r, &shards);
    }

    #[test]
    fn split_shard_larger_than_range() {
        let r = FrameRange::new(5, 9).unwrap();
        let shards = split(r, 1000);
        assert_eq!(shards.len(), 1, "one shard covers the whole range");
        assert_eq!(shards[0].range, r);
        assert_partitions(r, &shards);
    }

    #[test]
    fn split_single_frame() {
        let r = FrameRange::new(42, 42).unwrap();
        let shards = split(r, 8);
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0].count(), 1);
        assert_partitions(r, &shards);
    }

    #[test]
    fn split_size_one_is_one_frame_each() {
        let r = FrameRange::new(0, 4).unwrap();
        let shards = split(r, 1);
        assert_eq!(shards.len(), 5);
        assert!(shards.iter().all(|s| s.count() == 1));
        assert_partitions(r, &shards);
    }

    #[test]
    fn split_zero_size_treated_as_one() {
        let r = FrameRange::new(1, 3).unwrap();
        let shards = split(r, 0);
        assert_eq!(shards.len(), 3, "shard_size 0 degrades to 1 frame per shard");
        assert_partitions(r, &shards);
    }

    #[test]
    fn shard_count_matches_split_len() {
        for (s, e, sz) in [(1, 100, 10), (1, 25, 10), (0, 0, 5), (5, 9, 1000), (1, 7, 3)] {
            let r = FrameRange::new(s, e).unwrap();
            assert_eq!(shard_count(r, sz) as usize, split(r, sz).len(), "{s}-{e}/{sz}");
        }
    }

    #[test]
    fn split_handles_high_frame_numbers_without_overflow() {
        let r = FrameRange::new(u32::MAX - 3, u32::MAX).unwrap();
        let shards = split(r, 2);
        assert_eq!(shards.len(), 2);
        assert_partitions(r, &shards);
    }
}
