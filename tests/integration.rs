//! Integration tests exercising ce-render's public library surface without a live CE node:
//! frame-range sharding partitions a job, the result manifest assembles output CIDs (with missing
//! and diverged frames), the verify dial selects a beacon-seeded canary set and catches a CID
//! divergence, and the exact capability authorization a render host performs (a `render:frame` chain
//! authorizes a deploy; an unrelated ability does not — attenuation is enforced).

use ce_iam_core::{Caveats, Resource, SignedCapability, authorize, decode_chain, encode_chain};
use ce_identity::Identity;
use ce_render::manifest::{FrameResult, ResultManifest};
use ce_render::proto::ABILITY_FRAME;
use ce_render::shard::{self, FrameRange};
use ce_render::verify::{self, Verdict};

/// A deterministic identity from a tmp dir seed, so chains are reproducible per test.
fn identity(tag: &str) -> Identity {
    let dir = std::env::temp_dir().join(format!("ce-render-it-{tag}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    Identity::load_or_generate(&dir).expect("identity")
}

fn never_revoked(_issuer: &[u8; 32], _nonce: u64) -> bool {
    false
}

// ----- sharding: a job partitions into shards that reassemble exactly -----

#[test]
fn frame_range_shards_partition_and_manifest_reassembles() {
    let range = FrameRange::new(1, 500).unwrap();
    let shards = shard::split(range, 10);
    assert_eq!(shards.len(), 50, "500 frames / 10 per shard = 50 shards");

    // Each shard "renders" to a CID; assemble the manifest from all frame results.
    let mut results = Vec::new();
    for s in &shards {
        let cid = format!("cid-shard-{}", s.index);
        for frame in s.range.start..=s.range.end {
            results.push(FrameResult { frame, host: format!("host-{}", s.index % 6), output_cid: cid.clone(), verified: None });
        }
    }
    let manifest = ResultManifest::assemble(range, &results);
    assert!(manifest.is_complete(), "every frame in 1-500 is covered");
    assert_eq!(manifest.collected(), 500);
    assert!(manifest.missing.is_empty());
    // frame 1 belongs to shard 0, frame 500 to shard 49
    assert_eq!(manifest.outputs.first().unwrap().output_cid, "cid-shard-0");
    assert_eq!(manifest.outputs.last().unwrap().output_cid, "cid-shard-49");
}

#[test]
fn a_dropped_shard_shows_as_missing_frames() {
    let range = FrameRange::new(1, 30).unwrap();
    let shards = shard::split(range, 10); // 3 shards: 1-10, 11-20, 21-30
    let mut results = Vec::new();
    for s in &shards {
        if s.index == 1 {
            continue; // shard 1 (frames 11-20) host died — produced nothing
        }
        for frame in s.range.start..=s.range.end {
            results.push(FrameResult { frame, host: "h".into(), output_cid: format!("c{}", s.index), verified: None });
        }
    }
    let manifest = ResultManifest::assemble(range, &results);
    assert!(!manifest.is_complete());
    assert_eq!(manifest.missing, (11..=20).collect::<Vec<_>>());
}

// ----- verify dial: beacon-seeded selection + divergence detection -----

#[test]
fn verify_dial_selects_and_catches_fraud() {
    // 100 shards, 5% verify → 5 canaried, reproducibly chosen from the beacon.
    let canary = verify::canary_set(100, 0.05, "pow-tip-hash-abc");
    assert_eq!(canary.len(), 5);
    assert_eq!(canary, verify::canary_set(100, 0.05, "pow-tip-hash-abc"), "deterministic per beacon");

    // An honest re-render agrees; a fraud (different output CID) diverges.
    assert_eq!(verify::agrees("frameCID", "frameCID"), Verdict::Agree);
    assert_eq!(verify::agrees("frameCID", "BLACK-FRAME-CID"), Verdict::Diverge);
}

#[test]
fn diverged_canary_flags_frame_in_manifest() {
    let range = FrameRange::new(1, 2).unwrap();
    // frame 1 verified OK, frame 2 caught diverging
    let good = FrameResult { frame: 1, host: "h1".into(), output_cid: "c1".into(), verified: Some(true) };
    let bad = FrameResult { frame: 2, host: "h2".into(), output_cid: "suspect".into(), verified: Some(false) };
    let manifest = ResultManifest::assemble(range, &[good, bad]);
    assert_eq!(manifest.diverged, vec![2]);
    assert!(!manifest.is_complete(), "a diverged frame is withheld → job not complete");
}

// ----- capability authorization the render host performs -----

/// A host self-issues a `render:frame` cap to an operator; the host authorizes the operator's deploy.
#[test]
fn host_authorizes_self_issued_render_frame() {
    let host = identity("host-a");
    let operator = identity("op-a");

    let cap = SignedCapability::issue(
        &host,
        operator.node_id(),
        vec![ABILITY_FRAME.to_string()],
        Resource::Any,
        Caveats::default(),
        1,
        None,
    );
    let token = encode_chain(&[cap]);
    let decoded: Vec<SignedCapability> = decode_chain(&token).expect("decode");
    let res = authorize(
        &host.node_id(),
        &[],
        &[],
        0,
        &operator.node_id(),
        ABILITY_FRAME,
        &decoded,
        &never_revoked,
    );
    assert!(res.is_ok(), "self-issued render:frame cap must authorize a deploy: {res:?}");
}

/// A cap granting a *different* ability must NOT authorize `render:frame` — abilities are opaque and
/// not interchangeable (attenuation is enforced).
#[test]
fn unrelated_ability_does_not_authorize_render_frame() {
    let host = identity("host-b");
    let operator = identity("op-b");

    let cap = SignedCapability::issue(
        &host,
        operator.node_id(),
        vec!["pin:read".to_string()], // not render:frame
        Resource::Any,
        Caveats::default(),
        2,
        None,
    );
    let res = authorize(
        &host.node_id(),
        &[],
        &[],
        0,
        &operator.node_id(),
        ABILITY_FRAME,
        &[cap],
        &never_revoked,
    );
    assert!(res.is_err(), "a non-render cap must not grant render:frame");
}

/// A chain rooted at a stranger (neither the host nor a configured root) is rejected.
#[test]
fn unrooted_chain_is_denied() {
    let host = identity("host-c");
    let stranger = identity("stranger-c");
    let operator = identity("op-c");

    let cap = SignedCapability::issue(
        &stranger,
        operator.node_id(),
        vec![ABILITY_FRAME.to_string()],
        Resource::Any,
        Caveats::default(),
        3,
        None,
    );
    let res = authorize(
        &host.node_id(),
        &[], // no accepted roots
        &[],
        0,
        &operator.node_id(),
        ABILITY_FRAME,
        &[cap],
        &never_revoked,
    );
    assert!(res.is_err(), "a chain rooted at a stranger must be denied");
}
