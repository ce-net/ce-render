//! `ce-render` — fan a render job across the CE mesh, gather output blobs, reassemble.
//!
//! The client side of the render farm. It reads a `ce-render.toml` (or flags), discovers atlas-ranked
//! hosts, splits the frame range into shards, places each shard on a host via the node's directed
//! `mesh_deploy` primitive (capability-gated via the deploy `grant`), collects per-shard output CIDs,
//! re-renders a beacon-seeded subset to catch frauds, and writes a frame-ordered result manifest that
//! `fetch-all` pulls back from the data layer. CE provides the substrate (placement, sandboxed run,
//! billing, content-addressed blobs, the verify beacon); ce-render is the orchestration policy on top.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ce_render::config::RenderConfig;
use ce_render::farm::{self, DeployParams, MeshDeployer, Plan};
use ce_render::manifest::ResultManifest;
use ce_render::caps;
use ce_rs::{Amount, CeClient};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ce-render", about = "Headless-browser / Blender render farm over the CE mesh", version)]
struct Cli {
    /// CE node HTTP API URL.
    #[arg(long, default_value = "http://127.0.0.1:8844", global = true)]
    node: String,
    /// Capability token (hex `ce-cap` chain) granting `render:frame` on the target hosts. Overrides
    /// the config's `cap` field. For a fleet/org, one chain rooted at a key all hosts honor covers them.
    #[arg(long, global = true)]
    cap: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List candidate render hosts from the atlas (docker-capable, ranked by delivered work).
    Hosts {
        /// Only show hosts advertising this self-tag (e.g. gpu).
        #[arg(long)]
        select: Option<String>,
    },
    /// Show the placement plan for a job without deploying anything (dry run).
    Plan {
        /// Path to ce-render.toml.
        #[arg(long, default_value = "ce-render.toml")]
        config: PathBuf,
    },
    /// Split, scatter across hosts, gather output CIDs, verify, and write the result manifest.
    Submit {
        /// Path to ce-render.toml.
        #[arg(long, default_value = "ce-render.toml")]
        config: PathBuf,
        /// Where to write the result manifest JSON.
        #[arg(short = 'o', long, default_value = "ce-render.manifest.json")]
        out: PathBuf,
    },
    /// Inspect a written manifest: completeness, missing frames, diverged frames.
    Manifest {
        /// Path to a manifest JSON written by `submit`.
        manifest: PathBuf,
    },
    /// Fetch every output CID in a manifest from the data layer into a directory.
    FetchAll {
        /// Path to a manifest JSON written by `submit`.
        manifest: PathBuf,
        /// Output directory for the fetched frame artifacts.
        #[arg(short = 'o', long, default_value = "out")]
        dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let ce = CeClient::new(cli.node.clone());

    match cli.cmd {
        Cmd::Hosts { select } => hosts(&ce, select.as_deref()).await,
        Cmd::Plan { config } => plan_cmd(&ce, &config).await,
        Cmd::Submit { config, out } => submit(&ce, &config, &out, cli.cap.as_deref()).await,
        Cmd::Manifest { manifest } => show_manifest(&manifest),
        Cmd::FetchAll { manifest, dir } => fetch_all(&ce, &manifest, &dir).await,
    }
}

async fn hosts(ce: &CeClient, select: Option<&str>) -> Result<()> {
    let ranked = farm::discover(ce, select).await?;
    if ranked.is_empty() {
        println!("No candidate render hosts.");
        return Ok(());
    }
    println!("{} ranked render host(s) (most-proven first):", ranked.len());
    for (i, h) in ranked.iter().enumerate() {
        println!("  {:>2}. {}", i + 1, h);
    }
    Ok(())
}

/// Resolve the live placement plan for a job: load config, discover+rank hosts, seed the canary set
/// from the node's beacon, and build the [`Plan`].
async fn build_plan(ce: &CeClient, cfg: &RenderConfig) -> Result<Plan> {
    let range = cfg.range()?;
    // `discover` returns hosts already ranked best-first; take the top `cfg.hosts` to fan out to.
    let ranked = farm::discover(ce, cfg.select.as_deref()).await?;
    let chosen: Vec<String> = ranked.into_iter().take(cfg.hosts).collect();
    let beacon = ce.beacon().await.map(|b| b.hash).unwrap_or_default();
    Plan::build(
        range,
        cfg.shard_size,
        &chosen,
        cfg.verify,
        &beacon,
        cfg.kind,
        cfg.image(),
        cfg.args.clone(),
    )
}

async fn plan_cmd(ce: &CeClient, config: &Path) -> Result<()> {
    let cfg = RenderConfig::load(config)?;
    let plan = build_plan(ce, &cfg).await?;
    print_plan(&cfg, &plan);
    println!("\n(dry run — nothing deployed. Use `ce-render submit` to run it.)");
    Ok(())
}

fn print_plan(cfg: &RenderConfig, plan: &Plan) {
    println!("Render plan:");
    println!("  kind        {:?}", cfg.kind);
    println!("  image       {}", plan.image);
    println!("  frames      {} ({} frames)", plan.range, plan.range.count());
    println!("  shards      {} (size {})", plan.shards.len(), cfg.shard_size);
    println!("  hosts       {}", distinct(&plan.assignment));
    println!("  verify      {:.0}% ({} shard(s) canaried)", cfg.verify * 100.0, plan.canary.len());
    println!("  bid/frame   {} base units", cfg.bid_per_frame);
    println!("\n  shard  frames    host");
    for (i, s) in plan.shards.iter().enumerate() {
        let canaried = if plan.canary.contains(&i) { " [verify]" } else { "" };
        println!("  {:>5}  {:<8}  {}{}", s.index, s.range.to_string(), short(&plan.assignment[i]), canaried);
    }
}

async fn submit(ce: &CeClient, config: &Path, out: &Path, cap_flag: Option<&str>) -> Result<()> {
    let cfg = RenderConfig::load(config)?;
    let plan = build_plan(ce, &cfg).await?;
    print_plan(&cfg, &plan);

    let chain = caps::resolve(cap_flag, cfg.cap.as_deref());
    let grant = caps::grant(&chain).map(|s| s.to_string());
    if grant.is_none() {
        tracing::warn!("no capability chain configured — hosts that do not self-root your node will deny render:frame");
    }

    let bid_per_frame = Amount::from_base(
        cfg.bid_per_frame.parse::<i128>().context("bid_per_frame must be integer base units")?,
    );
    let params = DeployParams {
        cpu_cores: cfg.cpu_cores,
        mem_mb: cfg.mem_mb,
        duration_secs: cfg.frame_secs,
        bid_per_frame,
    };

    println!("\nScattering {} shard(s) across the mesh...\n", plan.shards.len());
    let deployer = MeshDeployer { client: ce };
    let manifest = farm::run(&deployer, &plan, &params, grant.as_deref()).await?;

    let json = serde_json::to_vec_pretty(&manifest)?;
    std::fs::write(out, &json).with_context(|| format!("writing manifest to {}", out.display()))?;

    println!(
        "\nCollected {}/{} frame(s). {} missing, {} diverged.",
        manifest.collected(),
        plan.range.count(),
        manifest.missing.len(),
        manifest.diverged.len(),
    );
    println!("Manifest written to {}.", out.display());
    if manifest.is_complete() {
        println!("Job complete. `ce-render fetch-all {} -o out/` to pull the frames.", out.display());
    } else {
        if !manifest.missing.is_empty() {
            println!("Missing frames: {}", brief(&manifest.missing));
        }
        if !manifest.diverged.is_empty() {
            println!("Diverged (fraud-suspect, unpaid) frames: {}", brief(&manifest.diverged));
        }
        bail!("render incomplete — see missing/diverged frames above");
    }
    Ok(())
}

fn show_manifest(path: &Path) -> Result<()> {
    let manifest = load_manifest(path)?;
    println!("Manifest {}:", path.display());
    println!("  range     {}", manifest.range);
    println!("  collected {} frame(s)", manifest.collected());
    println!("  missing   {}", if manifest.missing.is_empty() { "none".into() } else { brief(&manifest.missing) });
    println!("  diverged  {}", if manifest.diverged.is_empty() { "none".into() } else { brief(&manifest.diverged) });
    println!("  complete  {}", manifest.is_complete());
    println!("\n  frame  host          cid");
    for e in &manifest.outputs {
        let flag = match e.verified {
            Some(true) => " (verified)",
            Some(false) => " (DIVERGED)",
            None => "",
        };
        println!("  {:>5}  {}  {}{}", e.frame, short(&e.host), e.output_cid, flag);
    }
    Ok(())
}

async fn fetch_all(ce: &CeClient, manifest_path: &Path, dir: &Path) -> Result<()> {
    let manifest = load_manifest(manifest_path)?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut fetched = 0usize;
    let mut failed = 0usize;
    for entry in &manifest.outputs {
        if entry.output_cid.is_empty() {
            failed += 1;
            tracing::warn!(frame = entry.frame, "no output CID for frame — skipping");
            continue;
        }
        match ce.get_object(&entry.output_cid).await {
            Ok(bytes) => {
                let path = dir.join(format!("frame_{:05}_{}.bin", entry.frame, &entry.output_cid[..entry.output_cid.len().min(12)]));
                std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
                fetched += 1;
                tracing::info!(frame = entry.frame, bytes = bytes.len(), path = %path.display(), "fetched");
            }
            Err(e) => {
                failed += 1;
                tracing::warn!(frame = entry.frame, cid = %entry.output_cid, error = %e, "fetch failed");
            }
        }
    }
    println!("Fetched {fetched} frame(s) into {} ({failed} failed).", dir.display());
    if fetched == 0 && !manifest.outputs.is_empty() {
        bail!("no frames could be fetched");
    }
    Ok(())
}

fn load_manifest(path: &Path) -> Result<ResultManifest> {
    let text = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&text).with_context(|| format!("parsing manifest {}", path.display()))
}

/// Count distinct hosts in an assignment.
fn distinct(assignment: &[String]) -> usize {
    assignment.iter().collect::<std::collections::HashSet<_>>().len()
}

/// Compact a sorted frame list for display: `1, 2, 3` rather than dumping thousands.
fn brief(frames: &[u32]) -> String {
    const MAX: usize = 20;
    if frames.len() <= MAX {
        frames.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(", ")
    } else {
        let head: Vec<String> = frames.iter().take(MAX).map(|f| f.to_string()).collect();
        format!("{}, ... ({} more)", head.join(", "), frames.len() - MAX)
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}
