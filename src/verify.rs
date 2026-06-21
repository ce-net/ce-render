//! The render verification dial — the trust crux for exec apps.
//!
//! A render frame is *reproducible*: rendering it again on a different host must yield byte-identical
//! (or perceptually identical) output. ce-render spot-checks a configurable fraction of frames by
//! re-rendering them on a second host and comparing the output CIDs. A divergence means at least one
//! host did not run the real workload (a planted black frame, a cached lie) — that frame is flagged
//! `verified = Some(false)`, withheld from payment, and re-assigned.
//!
//! This module is the **pure selection + comparison** logic (the portfolio's shared `ce-mesh-verify`
//! idea, inlined here for the MVP): which shards to canary, seeded by the public PoW `beacon` so the
//! choice is unpredictable to a host yet reproducible by any auditor. Network-free → unit-tested.

use sha2::{Digest, Sha256};

/// Choose which shard indices to re-render for verification. `pct` is the fraction in `[0,1]`;
/// `total` is the shard count; `beacon_hash` is the PoW tip hash (unpredictable, globally agreed) so
/// a malicious host cannot know in advance which of its shards will be audited. Returns a sorted,
/// deduplicated set of indices in `[0,total)`.
///
/// `pct <= 0` selects none; `pct >= 1` selects all. Otherwise the count is `ceil(pct*total)`,
/// clamped to at least 1 when `pct > 0` (so a non-zero dial always audits something).
pub fn canary_set(total: usize, pct: f32, beacon_hash: &str) -> Vec<usize> {
    if total == 0 || pct <= 0.0 {
        return Vec::new();
    }
    if pct >= 1.0 {
        return (0..total).collect();
    }
    let want = (((total as f32) * pct).ceil() as usize).clamp(1, total);
    // Deterministically shuffle indices by a beacon-seeded key, then take the first `want`.
    let mut keyed: Vec<(u64, usize)> = (0..total)
        .map(|i| (key(beacon_hash, i), i))
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut picked: Vec<usize> = keyed.into_iter().take(want).map(|(_, i)| i).collect();
    picked.sort_unstable();
    picked
}

/// A beacon-seeded sort key for shard index `i` — folds the public beacon hash with the index.
fn key(beacon_hash: &str, i: usize) -> u64 {
    let mut h = Sha256::new();
    h.update(beacon_hash.as_bytes());
    h.update(b"|");
    h.update((i as u64).to_be_bytes());
    let d = h.finalize();
    let mut n = [0u8; 8];
    n.copy_from_slice(&d[..8]);
    u64::from_be_bytes(n)
}

/// Verdict of comparing an original render against its re-render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Outputs agree — the host ran the real workload.
    Agree,
    /// Outputs differ — at least one host is suspect; withhold payment, re-assign.
    Diverge,
}

/// Compare an original frame's output CID against a re-render's. For deterministic renders the CID is
/// the content hash, so equality *is* agreement (content-addressing makes this trustless: a host
/// cannot fake a CID without producing the bytes). A perceptual-hash mode for lossy renders is a
/// documented follow-up.
pub fn agrees(original_cid: &str, rerun_cid: &str) -> Verdict {
    if original_cid == rerun_cid && !original_cid.is_empty() {
        Verdict::Agree
    } else {
        Verdict::Diverge
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_zero_selects_none() {
        assert!(canary_set(100, 0.0, "beacon").is_empty());
        assert!(canary_set(0, 0.5, "beacon").is_empty());
    }

    #[test]
    fn pct_one_selects_all() {
        assert_eq!(canary_set(5, 1.0, "beacon"), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn pct_fraction_rounds_up_and_is_in_range() {
        let set = canary_set(100, 0.05, "beacon-hash");
        assert_eq!(set.len(), 5, "5% of 100 = 5");
        assert!(set.iter().all(|&i| i < 100));
        // sorted + deduped
        let mut sorted = set.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(set, sorted);
    }

    #[test]
    fn nonzero_pct_audits_at_least_one() {
        // 1% of 10 frames rounds to ceil(0.1)=1
        assert_eq!(canary_set(10, 0.01, "b").len(), 1);
    }

    #[test]
    fn selection_is_deterministic_per_beacon() {
        let a = canary_set(50, 0.2, "beacon-X");
        let b = canary_set(50, 0.2, "beacon-X");
        assert_eq!(a, b, "same beacon → same canary set");
    }

    #[test]
    fn selection_varies_with_beacon() {
        let a = canary_set(50, 0.1, "beacon-0");
        let moved = (1..20).any(|i| canary_set(50, 0.1, &format!("beacon-{i}")) != a);
        assert!(moved, "varying the beacon should vary which frames are audited");
    }

    #[test]
    fn agrees_on_equal_cids() {
        assert_eq!(agrees("cidA", "cidA"), Verdict::Agree);
        assert_eq!(agrees("cidA", "cidB"), Verdict::Diverge);
        assert_eq!(agrees("", ""), Verdict::Diverge, "empty CIDs never agree");
    }
}
