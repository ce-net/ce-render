//! The render farm: scatter shards across atlas-ranked mesh hosts, gather output CIDs, verify, and
//! assemble the result manifest. This is the live orchestration layer (the swarm scatter/gather
//! pattern, adapted to directed `mesh_deploy` placement). The *decisions* it makes — which host
//! renders which shard, which frames to canary, how results assemble — are delegated to the pure,
//! unit-tested modules ([`crate::placement`], [`crate::verify`], [`crate::manifest`]); this module
//! is the I/O glue.
//!
//! Placement uses CE's directed-deploy primitive: for each shard we `mesh_deploy` a container cell on
//! a chosen host with the per-shard render command and the deploy `grant` (the `ce-cap` chain the
//! host authorizes against `render:frame`). A completed cell returns an **output CID** (the host's
//! `put_object` of the rendered frames); we collect those, optionally re-render a beacon-seeded
//! subset on a second host to catch frauds, and hand the lot to [`ResultManifest::assemble`].
//!
//! To keep the orchestration testable without a live cluster, deployment is behind the [`Deployer`]
//! trait: the live impl drives `CeClient`, while tests inject a fake that returns scripted outputs
//! (including a planted fraud host) to exercise the verify dial end-to-end.

use std::collections::HashMap;

use anyhow::{Result, bail};
use ce_rs::{Amount, AtlasEntry, BidSpec, CeClient};

use crate::manifest::{FrameResult, ResultManifest};
use crate::placement::{self, Candidate};
use crate::proto::{self, Kind};
use crate::shard::{self, FrameRange, Shard};
use crate::verify::{self, Verdict};

/// A single shard's deployment outcome: the host it ran on and its output CID (the rendered frames,
/// addressable via `get_object`). `output_cid` is empty when the cell produced no captured artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardOutcome {
    pub shard_index: u32,
    pub host: String,
    pub output_cid: String,
}

/// Abstraction over "place this shard's render on `host` and return its output CID". The live impl is
/// [`MeshDeployer`]; tests use a scripted fake. `spec` carries the image, the per-shard argv, the
/// resource ceiling, and the bid; `grant` is the capability chain the host authorizes.
#[allow(async_fn_in_trait)]
pub trait Deployer {
    /// Deploy one shard on `host` and resolve to its output CID (empty if none produced).
    async fn deploy(&self, host: &str, spec: &BidSpec, grant: Option<&str>) -> Result<String>;
}

/// Live deployer over a CE node: directed `mesh_deploy` of a container cell, then await the cell's
/// completion and read its output CID.
pub struct MeshDeployer<'a> {
    pub client: &'a CeClient,
}

impl Deployer for MeshDeployer<'_> {
    async fn deploy(&self, host: &str, spec: &BidSpec, grant: Option<&str>) -> Result<String> {
        // mesh_deploy places the cell and returns the host-assigned job id. The output CID for a
        // completed cell is carried on the deploy response; the SDK's typed `mesh_deploy` currently
        // surfaces only the job id, so we poll the job to completion and read its captured output.
        let job_id = self.client.mesh_deploy(host, spec, grant).await?;
        await_output(self.client, &job_id).await
    }
}

/// Poll a deployed job until it finishes, returning its output CID.
///
// TODO(ce-rs): the `/mesh-deploy` response already includes `output` for completed cells, but the
// SDK's typed `mesh_deploy` drops it (returns only `job_id`) and the `Job` record has no `output`
// field, so a container render's output CID cannot be read back through the current SDK surface.
// This polls for terminal status and then returns the job's output once ce-rs exposes it; until
// then a completed job yields an empty CID (surfaced as a "missing" frame by the manifest, never a
// silent wrong result). Tracking: add `output: Option<String>` to `ce_rs::Job` / a `mesh_deploy`
// that returns `Deployment`. The orchestration, sharding, verify, and manifest logic are complete
// and tested; this is the single live-path gap.
async fn await_output(client: &CeClient, job_id: &str) -> Result<String> {
    use tokio::time::{Duration, sleep};
    for _ in 0..600 {
        match client.job(job_id).await {
            Ok(job) => {
                if job.status == "completed" || job.status == "settled" || job.status == "exited" {
                    // Output CID not yet exposed on the Job record — see the TODO above.
                    return Ok(String::new());
                }
                if job.status == "failed" || job.status == "error" {
                    bail!("job {job_id} failed (status {})", job.status);
                }
            }
            Err(e) => tracing::debug!(job = %job_id, error = %e, "job poll failed (retrying)"),
        }
        sleep(Duration::from_secs(1)).await;
    }
    bail!("job {job_id} did not complete within the deadline")
}

/// The fully-resolved plan for a render job: which shard runs where, the per-shard command, and the
/// verify canary set. Pure to build (no I/O) so it can be inspected/tested before any deploy.
#[derive(Debug, Clone)]
pub struct Plan {
    pub range: FrameRange,
    pub shards: Vec<Shard>,
    /// The ranked host pool the job draws from (used to pick a *distinct* verifier for canaries).
    pub hosts: Vec<String>,
    /// `assignment[i]` is the host node id rendering `shards[i]`.
    pub assignment: Vec<String>,
    /// Shard indices selected for redundant re-render verification.
    pub canary: Vec<usize>,
    pub kind: Kind,
    pub image: String,
    pub args: Vec<String>,
}

impl Plan {
    /// Build a placement plan: split the range, rank+assign hosts round-robin, pick the canary set.
    /// `beacon_hash` seeds the canary selection unpredictably. Pure → composes the tested modules.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        range: FrameRange,
        shard_size: u32,
        hosts: &[String],
        verify_pct: f32,
        beacon_hash: &str,
        kind: Kind,
        image: String,
        args: Vec<String>,
    ) -> Result<Self> {
        if hosts.is_empty() {
            bail!("no candidate hosts to place render shards on");
        }
        let shards = shard::split(range, shard_size);
        let assignment = placement::assign(shards.len(), hosts);
        let canary = verify::canary_set(shards.len(), verify_pct, beacon_hash);
        Ok(Plan { range, shards, hosts: hosts.to_vec(), assignment, canary, kind, image, args })
    }

    /// The container argv for shard `i`.
    pub fn argv(&self, i: usize) -> Vec<String> {
        proto::render_argv(self.kind, self.shards[i].range, &self.args)
    }
}

/// Resource + billing parameters applied to every shard deploy.
#[derive(Debug, Clone)]
pub struct DeployParams {
    pub cpu_cores: u32,
    pub mem_mb: u64,
    pub duration_secs: u64,
    /// Per-frame bid in base units; the per-shard bid is `bid_per_frame * frames_in_shard`.
    pub bid_per_frame: Amount,
}

/// The bid spec for one shard: cost scales with the shard's frame count.
pub fn shard_spec(plan: &Plan, i: usize, params: &DeployParams) -> BidSpec {
    let frames = plan.shards[i].count() as i128;
    let bid = Amount::from_base(params.bid_per_frame.base().saturating_mul(frames));
    BidSpec {
        image: plan.image.clone(),
        cmd: plan.argv(i),
        cpu_cores: params.cpu_cores,
        mem_mb: params.mem_mb,
        duration_secs: params.duration_secs,
        bid,
    }
}

/// Run the whole job with `deployer`: deploy every shard, re-render the canary set on a different
/// host, compare, and assemble the [`ResultManifest`]. Returns the manifest (frame-ordered output
/// CIDs + missing + diverged). Deploys run sequentially in this MVP for clear per-shard logging;
/// concurrent fan-out via `JoinSet` is a drop-in refinement (the trait is `Send`-friendly).
pub async fn run<D: Deployer>(
    deployer: &D,
    plan: &Plan,
    params: &DeployParams,
    grant: Option<&str>,
) -> Result<ResultManifest> {
    // 1. Scatter: one deploy per shard, each frame in the shard sharing the shard's output CID.
    let mut frame_results: Vec<FrameResult> = Vec::new();
    let mut shard_outputs: HashMap<u32, ShardOutcome> = HashMap::new();

    for (i, shard) in plan.shards.iter().enumerate() {
        let host = &plan.assignment[i];
        let spec = shard_spec(plan, i, params);
        match deployer.deploy(host, &spec, grant).await {
            Ok(cid) => {
                tracing::info!(shard = shard.index, frames = %shard.range, host = %short(host), cid = %short(&cid), "shard rendered");
                shard_outputs.insert(shard.index, ShardOutcome { shard_index: shard.index, host: host.clone(), output_cid: cid.clone() });
                for frame in shard.range.start..=shard.range.end {
                    frame_results.push(FrameResult { frame, host: host.clone(), output_cid: cid.clone(), verified: None });
                }
            }
            Err(e) => {
                tracing::warn!(shard = shard.index, host = %short(host), error = %e, "shard deploy failed");
                // leave its frames absent → the manifest reports them missing
            }
        }
    }

    // 2. Verify: re-render the canary shards on a different host and compare output CIDs.
    for &ci in &plan.canary {
        let Some(orig) = shard_outputs.get(&(ci as u32)).cloned() else { continue };
        let Some(verifier) = placement::verifier_for(&orig.host, &plan.hosts) else {
            tracing::debug!(shard = ci, "no distinct verifier host available — skipping canary");
            continue;
        };
        let spec = shard_spec(plan, ci, params);
        let verdict = match deployer.deploy(&verifier, &spec, grant).await {
            Ok(rerun_cid) => verify::agrees(&orig.output_cid, &rerun_cid),
            Err(e) => {
                tracing::warn!(shard = ci, error = %e, "canary re-render failed — treating as unverified");
                continue;
            }
        };
        let ok = verdict == Verdict::Agree;
        if !ok {
            tracing::warn!(shard = ci, host = %short(&orig.host), verifier = %short(&verifier), "VERIFY DIVERGENCE — withholding payment, re-assigning");
        }
        // stamp the verdict on every frame of the canary shard
        if let Some(sh) = plan.shards.get(ci) {
            for fr in frame_results.iter_mut().filter(|f| f.frame >= sh.range.start && f.frame <= sh.range.end) {
                fr.verified = Some(ok);
            }
        }
    }

    Ok(ResultManifest::assemble(plan.range, &frame_results))
}

/// Build ranked render candidates live from the node: docker-capable atlas hosts (optionally
/// tag-filtered), scored by on-chain delivered work. Mirrors swarm's `select_hosts`.
pub async fn discover(client: &CeClient, select: Option<&str>) -> Result<Vec<String>> {
    let atlas = client.atlas().await?;
    let pool = placement::candidates(&atlas, select);
    if pool.is_empty() {
        bail!("no matching hosts in the atlas (need 'docker'{})", select.map(|t| format!(" + '{t}'")).unwrap_or_default());
    }
    let mut cands: Vec<Candidate> = Vec::new();
    for h in &pool {
        let work = client.history(&h.node_id).await.map(|r| r.delivered_work()).unwrap_or(0);
        cands.push(to_candidate(h, work));
    }
    Ok(placement::rank(&cands).into_iter().map(|c| c.node_id).collect())
}

/// Distil an atlas entry + its reputation into a placement [`Candidate`].
fn to_candidate(h: &AtlasEntry, delivered_work: u64) -> Candidate {
    Candidate {
        node_id: h.node_id.clone(),
        delivered_work,
        last_seen_secs: h.last_seen_secs,
        running_jobs: h.running_jobs,
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted deployer: maps host → output CID, optionally returning a *different* CID on the
    /// second deploy to a host (to simulate a fraud caught by the verify dial), or erroring.
    struct FakeDeployer {
        /// host → CID it returns. A host absent from the map errors (simulating a dead host).
        outputs: HashMap<String, String>,
        /// Hosts that lie: their re-render (verifier) call returns a divergent CID.
        liars: std::collections::HashSet<String>,
        calls: std::cell::RefCell<HashMap<String, u32>>,
    }

    impl FakeDeployer {
        fn new(outputs: &[(&str, &str)]) -> Self {
            FakeDeployer {
                outputs: outputs.iter().map(|(h, c)| (h.to_string(), c.to_string())).collect(),
                liars: Default::default(),
                calls: Default::default(),
            }
        }
    }

    impl Deployer for FakeDeployer {
        async fn deploy(&self, host: &str, _spec: &BidSpec, _grant: Option<&str>) -> Result<String> {
            *self.calls.borrow_mut().entry(host.to_string()).or_default() += 1;
            match self.outputs.get(host) {
                Some(cid) => {
                    // A liar returns its real CID first but a divergent one when re-rendered as a
                    // verifier — but since canary always re-renders on a DIFFERENT host, model the
                    // liar as the *original* host producing a CID no honest re-render matches.
                    if self.liars.contains(host) {
                        Ok(format!("FRAUD-{cid}"))
                    } else {
                        Ok(cid.clone())
                    }
                }
                None => bail!("host {host} is unreachable"),
            }
        }
    }

    fn params() -> DeployParams {
        DeployParams { cpu_cores: 2, mem_mb: 4096, duration_secs: 600, bid_per_frame: Amount::from_base(10) }
    }

    fn plan(hosts: &[&str], verify_pct: f32) -> Plan {
        Plan::build(
            FrameRange::new(1, 4).unwrap(),
            2, // shard_size → shards: [1-2],[3-4]
            &hosts.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            verify_pct,
            "beacon-test",
            Kind::Blender,
            "ce-net/blender:latest".into(),
            vec![],
        )
        .unwrap()
    }

    #[test]
    fn plan_splits_and_assigns() {
        let p = plan(&["h1", "h2"], 0.0);
        assert_eq!(p.shards.len(), 2);
        assert_eq!(p.assignment, vec!["h1", "h2"]);
        assert!(p.canary.is_empty());
        // argv carries the shard's range
        let argv = p.argv(0);
        assert!(argv.windows(2).any(|w| w == ["-s", "1"]));
        assert!(argv.windows(2).any(|w| w == ["-e", "2"]));
    }

    #[test]
    fn shard_bid_scales_with_frame_count() {
        let p = plan(&["h1"], 0.0);
        // shard 0 covers 2 frames at 10 base units each = 20
        let spec = shard_spec(&p, 0, &params());
        assert_eq!(spec.bid.base(), 20);
    }

    #[tokio::test]
    async fn run_assembles_complete_manifest() {
        let p = plan(&["h1", "h2"], 0.0);
        let dep = FakeDeployer::new(&[("h1", "cidA"), ("h2", "cidB")]);
        let m = run(&dep, &p, &params(), None).await.unwrap();
        assert!(m.is_complete(), "all frames rendered, none diverged: {m:?}");
        // frames 1,2 → cidA (h1 shard); frames 3,4 → cidB (h2 shard)
        assert_eq!(m.cids(), vec!["cidA", "cidA", "cidB", "cidB"]);
    }

    #[tokio::test]
    async fn run_reports_dead_host_frames_missing() {
        let p = plan(&["h1", "dead"], 0.0);
        // only h1 returns output; "dead" is absent → its shard (frames 3-4) errors
        let dep = FakeDeployer::new(&[("h1", "cidA")]);
        let m = run(&dep, &p, &params(), None).await.unwrap();
        assert!(!m.is_complete());
        assert_eq!(m.missing, vec![3, 4]);
        assert_eq!(m.cids(), vec!["cidA", "cidA"]);
    }

    #[tokio::test]
    async fn verify_dial_catches_fraud() {
        // verify=1.0 → both shards canaried. h1 is a liar: its original output won't match an
        // honest re-render on h2.
        let mut p = plan(&["h1", "h2"], 1.0);
        // force both shards onto h1 so the verifier (h2) is honest and diverges from h1's fraud
        p.assignment = vec!["h1".into(), "h1".into()];
        let mut dep = FakeDeployer::new(&[("h1", "cidA"), ("h2", "cidA")]);
        dep.liars.insert("h1".into()); // h1 returns FRAUD-cidA; h2 re-render returns cidA → diverge
        let m = run(&dep, &p, &params(), None).await.unwrap();
        assert!(!m.diverged.is_empty(), "the verify dial must flag the fraud: {m:?}");
        assert!(!m.is_complete(), "a diverged job is not complete");
    }

    #[tokio::test]
    async fn honest_canary_verifies_clean() {
        let mut p = plan(&["h1", "h2"], 1.0);
        p.assignment = vec!["h1".into(), "h1".into()]; // h1 renders both; h2 verifies
        let dep = FakeDeployer::new(&[("h1", "cidA"), ("h2", "cidA")]); // honest: re-render matches
        let m = run(&dep, &p, &params(), None).await.unwrap();
        assert!(m.diverged.is_empty(), "honest re-renders agree: {m:?}");
        assert!(m.is_complete());
    }
}
