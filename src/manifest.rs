//! The render result manifest — assembling per-frame outputs back into one ordered, named artifact.
//!
//! Each rendered frame comes back from a host as a content-addressed **output CID** (the host stores
//! its frame via `put_object`; we fetch it by CID, so a frame's bytes are tamper-evident — a host
//! cannot return bytes whose hash differs from the CID it advertised). This module assembles those
//! per-frame results, in frame order, into a [`ResultManifest`]: the list of `(frame, CID)` an
//! operator can `fetch-all`, plus which frames are still missing and which failed verification.
//!
//! It is pure (no network, no I/O): the farm hands it `FrameResult`s and a `ResultManifest` is
//! produced deterministically, so the assembly logic is exhaustively unit-tested without a cluster.

use serde::{Deserialize, Serialize};

use crate::shard::FrameRange;

/// One rendered frame's result. `output_cid` is the CID of the frame artifact in the CE data layer
/// (fetch with `get_object`). `verified` records whether a redundant re-render on a second host
/// agreed (the verify dial); `None` means the frame was not spot-checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameResult {
    pub frame: u32,
    /// The host (64-hex NodeId) that produced this frame.
    pub host: String,
    /// Content id of the output artifact (manifest hash from the host's `put_object`).
    pub output_cid: String,
    /// `Some(true)` verified by re-render, `Some(false)` divergence detected, `None` not checked.
    #[serde(default)]
    pub verified: Option<bool>,
}

/// One entry in the assembled, frame-ordered output manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputEntry {
    pub frame: u32,
    pub host: String,
    pub output_cid: String,
    #[serde(default)]
    pub verified: Option<bool>,
}

/// The assembled result of a whole render job: every covered frame's output CID in frame order,
/// the frames still missing, and the verification verdict. This is what `ce-render`'s manifest
/// command serializes and what `fetch-all` walks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultManifest {
    /// The full frame range the job covered (inclusive).
    pub range: FrameRange,
    /// One entry per successfully-collected frame, sorted ascending by frame number.
    pub outputs: Vec<OutputEntry>,
    /// Frames in `range` with no collected result, sorted ascending. Empty == complete.
    pub missing: Vec<u32>,
    /// Frames whose redundant re-render diverged (`verified == Some(false)`), sorted ascending.
    /// These are present in `outputs` but must not be trusted / paid for as-is.
    pub diverged: Vec<u32>,
}

impl ResultManifest {
    /// Assemble a manifest from collected per-frame results over the job's `range`.
    ///
    /// Rules (all deterministic, all unit-tested):
    /// - results are de-duplicated by frame, keeping the **last** result for a frame (a re-render /
    ///   re-assignment supersedes the earlier attempt);
    /// - outputs are sorted ascending by frame;
    /// - any frame in `range` without a result is reported in `missing`;
    /// - any present frame with `verified == Some(false)` is reported in `diverged`;
    /// - results for frames outside `range` are ignored (defensive; the farm never produces them).
    pub fn assemble(range: FrameRange, results: &[FrameResult]) -> Self {
        use std::collections::BTreeMap;
        let mut by_frame: BTreeMap<u32, &FrameResult> = BTreeMap::new();
        for r in results {
            if r.frame < range.start || r.frame > range.end {
                continue; // out-of-range result — ignore
            }
            by_frame.insert(r.frame, r); // last write wins
        }

        let outputs: Vec<OutputEntry> = by_frame
            .values()
            .map(|r| OutputEntry {
                frame: r.frame,
                host: r.host.clone(),
                output_cid: r.output_cid.clone(),
                verified: r.verified,
            })
            .collect();

        let missing: Vec<u32> = (range.start..=range.end).filter(|f| !by_frame.contains_key(f)).collect();
        let diverged: Vec<u32> =
            by_frame.values().filter(|r| r.verified == Some(false)).map(|r| r.frame).collect();

        ResultManifest { range, outputs, missing, diverged }
    }

    /// Every covered frame produced an output and none diverged.
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty() && self.diverged.is_empty()
    }

    /// The output CIDs in frame order — the exact fetch plan for `fetch-all`.
    pub fn cids(&self) -> Vec<String> {
        self.outputs.iter().map(|e| e.output_cid.clone()).collect()
    }

    /// Count of frames successfully collected (regardless of verification).
    pub fn collected(&self) -> usize {
        self.outputs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fr(frame: u32, cid: &str) -> FrameResult {
        FrameResult { frame, host: format!("host-{frame}"), output_cid: cid.into(), verified: None }
    }

    fn range(a: u32, b: u32) -> FrameRange {
        FrameRange::new(a, b).unwrap()
    }

    #[test]
    fn assembles_complete_job_in_frame_order() {
        // arrive out of order
        let results = vec![fr(3, "c3"), fr(1, "c1"), fr(2, "c2")];
        let m = ResultManifest::assemble(range(1, 3), &results);
        assert!(m.is_complete());
        assert_eq!(m.missing, Vec::<u32>::new());
        assert_eq!(m.cids(), vec!["c1", "c2", "c3"], "outputs sorted ascending by frame");
        assert_eq!(m.collected(), 3);
    }

    #[test]
    fn reports_missing_frames() {
        let results = vec![fr(1, "c1"), fr(3, "c3")];
        let m = ResultManifest::assemble(range(1, 4), &results);
        assert!(!m.is_complete());
        assert_eq!(m.missing, vec![2, 4]);
        assert_eq!(m.cids(), vec!["c1", "c3"]);
    }

    #[test]
    fn last_result_wins_on_reassignment() {
        // frame 2 rendered twice (original then a re-assignment) — the re-render supersedes.
        let results = vec![fr(2, "stale"), fr(1, "c1"), fr(2, "fresh")];
        let m = ResultManifest::assemble(range(1, 2), &results);
        assert_eq!(m.cids(), vec!["c1", "fresh"]);
        assert_eq!(m.collected(), 2);
    }

    #[test]
    fn diverged_frames_are_flagged_but_present() {
        let mut bad = fr(2, "suspect");
        bad.verified = Some(false);
        let mut good = fr(1, "c1");
        good.verified = Some(true);
        let m = ResultManifest::assemble(range(1, 2), &[good, bad]);
        assert!(!m.is_complete(), "a diverged frame makes the job incomplete");
        assert_eq!(m.diverged, vec![2]);
        assert!(m.missing.is_empty());
        // still listed in outputs so an operator can inspect it
        assert_eq!(m.cids(), vec!["c1", "suspect"]);
    }

    #[test]
    fn out_of_range_results_are_ignored() {
        let results = vec![fr(1, "c1"), fr(99, "rogue")];
        let m = ResultManifest::assemble(range(1, 2), &results);
        assert_eq!(m.cids(), vec!["c1"]);
        assert_eq!(m.missing, vec![2]);
    }

    #[test]
    fn empty_results_all_missing() {
        let m = ResultManifest::assemble(range(1, 3), &[]);
        assert_eq!(m.missing, vec![1, 2, 3]);
        assert!(m.cids().is_empty());
        assert!(!m.is_complete());
    }

    #[test]
    fn manifest_json_roundtrips() {
        let m = ResultManifest::assemble(range(1, 2), &[fr(1, "c1"), fr(2, "c2")]);
        let bytes = serde_json::to_vec(&m).unwrap();
        let back: ResultManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(m, back);
    }
}
