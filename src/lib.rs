//! # ce-render — headless-browser / Blender render farm over CE
//!
//! ce-render is an **application built on CE primitives** (the SDK tier, like `swarm` / `rdev` /
//! `ce-pin`), not a node feature. It turns CE's directed `mesh_deploy` placement + content-addressed
//! blob layer into an embarrassingly-parallel render farm: split a render job into independent frame
//! shards, scatter each shard onto an atlas-ranked mesh host, collect the per-shard **output CIDs**,
//! optionally re-render a beacon-seeded subset on a second host to catch frauds (the verify dial),
//! and reassemble a frame-ordered manifest of output CIDs that `fetch-all` pulls back.
//!
//! ## Shape
//! - [`shard`]    — pure frame-range sharding (split a range into a partition of shards).
//! - [`manifest`] — pure result-manifest assembly (frame-ordered output CIDs + missing + diverged).
//! - [`proto`]    — render abilities + the per-shard container command builder (Blender / Chrome).
//! - [`placement`]— pure host ranking (atlas capacity + on-chain history) + round-robin assignment.
//! - [`verify`]   — pure beacon-seeded canary selection + output-CID comparison (the verify dial).
//! - [`config`]   — `ce-render.toml` loader/validator (money is base-unit strings, never floats).
//! - [`caps`]     — resolving the `ce-cap` chain the client presents as the deploy `grant`.
//! - [`farm`]     — live scatter/gather over `mesh_deploy`, gluing the pure modules together.
//!
//! ## Trust & money (honoring CE rules)
//! Authorization is the one CE primitive: each render host authorizes a signed, attenuating `ce-cap`
//! chain rooted at the host or a configured org root against the `render:frame` ability before
//! launching a cell — ce-render only *presents* the chain (the deploy `grant`), it never mints trust.
//! Money is integer base units (1 credit = 10^18 base units) carried as decimal strings — never
//! floats; the per-frame bid is priced in base units and per-frame billing rides CE payment channels.
//! Mesh-first: hosts are discovered via the atlas / `render:host` DHT service and addressed by NodeId,
//! never by stored ip:port.

pub mod caps;
pub mod config;
pub mod farm;
pub mod manifest;
pub mod placement;
pub mod proto;
pub mod shard;
pub mod verify;

/// Load accepted capability root keys for a render host: 64-hex NodeIds, one per line, `#` comments
/// allowed. Looked up at `$CE_RENDER_ROOTS`, else `$CE_DATA_DIR/roots`, else `~/.local/share/ce/roots`
/// — mirroring the node's and rdev's `<data_dir>/roots`. A host opts into an org/fleet by listing
/// that org's root key here; with no file, only self-issued chains are honored. (Used by a host-side
/// agent; the client side presents chains via [`caps`].)
pub fn load_roots() -> Vec<[u8; 32]> {
    use std::path::PathBuf;
    let path = std::env::var_os("CE_RENDER_ROOTS")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CE_DATA_DIR").map(|d| PathBuf::from(d).join("roots")))
        .or_else(|| directories::ProjectDirs::from("", "", "ce").map(|p| p.data_dir().join("roots")))
        .unwrap_or_else(|| PathBuf::from("roots"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .filter_map(|h| hex::decode(h).ok().and_then(|b| b.try_into().ok()))
        .collect()
}
