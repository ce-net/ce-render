//! `ce-render.toml` — the declarative render-job description.
//!
//! It pins everything a fan-out needs: the renderer kind + container image, the input asset CID, the
//! frame range, the shard size, the host count, the per-frame bid, an optional verify fraction, and
//! an optional capability chain. Money is **base units carried as decimal strings, never floats**
//! (the CE money rule): `bid_per_frame = "10000000000000000"` is 0.01 credit. The CLI may override
//! any field via flags.
//!
//! Example `ce-render.toml`:
//! ```toml
//! kind          = "blender"
//! image         = "ce-net/blender:latest"   # optional; defaults per kind
//! input_cid     = "b3f1c0...e7"             # scene.blend staged into the cell from the data layer
//! frames        = "1-500"
//! shard_size    = 10                         # frames per shard
//! hosts         = 8                          # max distinct hosts to fan out to
//! select        = "gpu"                      # optional atlas self-tag filter
//! bid_per_frame = "10000000000000000"        # base units (10^18 = 1 credit); decimal STRING
//! cpu_cores     = 4
//! mem_mb        = 8192
//! frame_secs    = 600                         # per-shard cell duration budget (seconds)
//! verify        = 0.05                        # re-render 5% of frames on a 2nd host (0 = off)
//! args          = ["--render-format", "PNG"] # extra container args, appended verbatim
//! cap           = ""                          # hex ce-cap chain granting render:frame (optional)
//! ```

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::proto::Kind;
use crate::shard::FrameRange;

/// A parsed `ce-render.toml`. Fields with defaults are optional in the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderConfig {
    pub kind: Kind,
    /// Container image; defaults to `kind.default_image()` when absent.
    #[serde(default)]
    pub image: Option<String>,
    /// CID of the input asset (scene / URL batch) to stage into each cell. May be empty for jobs
    /// whose container fetches its own input.
    #[serde(default)]
    pub input_cid: String,
    /// Inclusive frame range, e.g. `"1-500"` or `"42"`.
    pub frames: String,
    /// Frames per shard.
    #[serde(default = "default_shard_size")]
    pub shard_size: u32,
    /// Max distinct hosts to fan out to.
    #[serde(default = "default_hosts")]
    pub hosts: usize,
    /// Optional atlas self-tag filter (e.g. `gpu`).
    #[serde(default)]
    pub select: Option<String>,
    /// Per-frame bid, **base units as a decimal string** (never a float).
    #[serde(default = "default_bid")]
    pub bid_per_frame: String,
    #[serde(default = "default_cpu")]
    pub cpu_cores: u32,
    #[serde(default = "default_mem")]
    pub mem_mb: u64,
    /// Per-shard cell duration budget in seconds.
    #[serde(default = "default_frame_secs")]
    pub frame_secs: u64,
    /// Fraction of frames to re-render on a second host and verify (0.0 = off, 1.0 = all).
    #[serde(default)]
    pub verify: f32,
    /// Extra container args appended verbatim to the per-shard command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional hex `ce-cap` chain granting `render:frame`.
    #[serde(default)]
    pub cap: Option<String>,
}

fn default_shard_size() -> u32 {
    10
}
fn default_hosts() -> usize {
    4
}
fn default_bid() -> String {
    // 0.01 credit per frame in base units (10^18 per credit).
    "10000000000000000".to_string()
}
fn default_cpu() -> u32 {
    2
}
fn default_mem() -> u64 {
    4096
}
fn default_frame_secs() -> u64 {
    600
}

impl RenderConfig {
    /// Parse a config from TOML text, validating the cross-field invariants.
    pub fn parse(text: &str) -> Result<Self> {
        let cfg: RenderConfig = toml::from_str(text).context("parsing ce-render.toml")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load and parse `ce-render.toml` from `path`.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    /// The resolved frame range.
    pub fn range(&self) -> Result<FrameRange> {
        FrameRange::parse(&self.frames)
    }

    /// The image to use (explicit, else the kind default).
    pub fn image(&self) -> String {
        self.image.clone().unwrap_or_else(|| self.kind.default_image().to_string())
    }

    /// Validate cross-field constraints not expressible in the type. Pure → unit-tested.
    pub fn validate(&self) -> Result<()> {
        // frame range must parse and be ordered
        self.range()?;
        if self.hosts == 0 {
            bail!("hosts must be >= 1");
        }
        if !(0.0..=1.0).contains(&self.verify) {
            bail!("verify must be in [0.0, 1.0], got {}", self.verify);
        }
        if self.cpu_cores == 0 {
            bail!("cpu_cores must be >= 1");
        }
        if self.mem_mb == 0 {
            bail!("mem_mb must be >= 1");
        }
        // bid must be a non-negative integer base-unit amount (decimal string, never a float)
        if self.bid_per_frame.contains('.') {
            bail!("bid_per_frame must be integer base units (a decimal string), not a float");
        }
        self.bid_per_frame
            .parse::<u128>()
            .with_context(|| format!("bid_per_frame '{}' is not a base-unit integer", self.bid_per_frame))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        kind = "blender"
        frames = "1-100"
    "#;

    #[test]
    fn parses_minimal_with_defaults() {
        let cfg = RenderConfig::parse(MINIMAL).unwrap();
        assert_eq!(cfg.kind, Kind::Blender);
        assert_eq!(cfg.shard_size, 10);
        assert_eq!(cfg.hosts, 4);
        assert_eq!(cfg.bid_per_frame, "10000000000000000");
        assert_eq!(cfg.image(), "ce-net/blender:latest");
        assert_eq!(cfg.range().unwrap(), FrameRange::new(1, 100).unwrap());
        assert_eq!(cfg.verify, 0.0);
    }

    #[test]
    fn parses_full_config() {
        let text = r#"
            kind = "chrome"
            image = "myorg/chrome:1"
            input_cid = "abc123"
            frames = "0-49"
            shard_size = 5
            hosts = 6
            select = "gpu"
            bid_per_frame = "20000000000000000"
            cpu_cores = 4
            mem_mb = 8192
            frame_secs = 300
            verify = 0.1
            args = ["--window-size=1920,1080"]
            cap = "deadbeef"
        "#;
        let cfg = RenderConfig::parse(text).unwrap();
        assert_eq!(cfg.kind, Kind::Chrome);
        assert_eq!(cfg.image(), "myorg/chrome:1");
        assert_eq!(cfg.input_cid, "abc123");
        assert_eq!(cfg.select.as_deref(), Some("gpu"));
        assert_eq!(cfg.verify, 0.1);
        assert_eq!(cfg.args, vec!["--window-size=1920,1080"]);
        assert_eq!(cfg.cap.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn rejects_float_bid() {
        let text = "kind=\"blender\"\nframes=\"1-10\"\nbid_per_frame=\"0.01\"";
        assert!(RenderConfig::parse(text).is_err(), "floats are forbidden for money");
    }

    #[test]
    fn rejects_non_numeric_bid() {
        let text = "kind=\"blender\"\nframes=\"1-10\"\nbid_per_frame=\"lots\"";
        assert!(RenderConfig::parse(text).is_err());
    }

    #[test]
    fn rejects_bad_verify_fraction() {
        let text = "kind=\"blender\"\nframes=\"1-10\"\nverify=1.5";
        assert!(RenderConfig::parse(text).is_err());
    }

    #[test]
    fn rejects_zero_hosts() {
        let text = "kind=\"blender\"\nframes=\"1-10\"\nhosts=0";
        assert!(RenderConfig::parse(text).is_err());
    }

    #[test]
    fn rejects_inverted_range() {
        let text = "kind=\"blender\"\nframes=\"100-1\"";
        assert!(RenderConfig::parse(text).is_err());
    }
}
