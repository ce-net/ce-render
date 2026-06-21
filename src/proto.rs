//! ce-render protocol surface: the capability ability strings and the per-shard command builder.
//!
//! ce-render places work via the CE node's directed `mesh_deploy` primitive (a container cell on a
//! specific host), not a bespoke `request`/`reply` app loop — the host already authorizes the
//! deploy's `grant` chain against `render:frame` before launching the cell, so the cell command is
//! all ce-render needs to construct. This module keeps that command construction pure and tested:
//! given a shard's frame range, the input asset's CID, and the user's extra args, produce the
//! container argv that renders exactly that shard and emits its output as a content-addressed blob.
//!
//! Two render kinds are supported out of the box; both are deterministic given their inputs, which
//! is what makes the verify dial (re-render on a second host, compare) meaningful:
//!   - **Blender** (`blender -b <scene> -o <out> -s <start> -e <end> -a`) — a frame range;
//!   - **headless-chrome** screenshot/PDF — one URL or input per "frame".

use serde::{Deserialize, Serialize};

use crate::shard::FrameRange;

/// Ability a host requires before running a render shard for a requester. Opaque app-chosen string,
/// authorized by the host against a signed `ce-cap` chain rooted at the host or a configured org root.
pub const ABILITY_FRAME: &str = "render:frame";

/// The DHT service string a node advertises when it is willing to render (clients `find_service`
/// it to discover the render pool, then rank by atlas + history).
pub const SERVICE_HOST: &str = "render:host";

/// Which renderer a job uses. Selects the default image and the command template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Blender CLI batch render over a frame range (`blender -b ...`).
    Blender,
    /// Headless Chromium screenshot/PDF over a batch (one capture per "frame" index).
    Chrome,
    /// A custom image whose command template the user supplies verbatim (see [`render_argv`]).
    Custom,
}

impl Kind {
    /// The default container image for this kind (overridable in config).
    pub fn default_image(&self) -> &'static str {
        match self {
            Kind::Blender => "ce-net/blender:latest",
            Kind::Chrome => "ce-net/headless-chrome:latest",
            Kind::Custom => "alpine:latest",
        }
    }
}

/// Where a shard reads its input and writes its output **inside the container**. These are the staged
/// paths the host mounts: the input asset (staged from its CID before launch) and the output dir
/// whose contents the host captures and returns as an output CID.
pub const INPUT_PATH: &str = "/work/input";
pub const OUTPUT_DIR: &str = "/work/out";

/// Build the container argv that renders one `shard` of a job.
///
/// `extra` are user-supplied trailing args appended verbatim (e.g. Blender `--render-format PNG`,
/// or a Chrome `--window-size=1920,1080`). For [`Kind::Custom`], `extra` IS the full command and the
/// frame range is exposed to it via the placeholders `{start}`, `{end}`, `{input}`, `{out}` (each
/// occurrence substituted) — giving full control without ce-render knowing the tool.
///
/// Pure and deterministic → unit-tested. No shell is involved; argv is passed to the container
/// runtime directly, so there is no quoting/injection surface.
pub fn render_argv(kind: Kind, shard_range: FrameRange, extra: &[String]) -> Vec<String> {
    match kind {
        Kind::Blender => {
            // blender -b <scene> -o <out>/frame_#### -s START -e END -a
            let mut v = vec![
                "blender".into(),
                "-b".into(),
                INPUT_PATH.into(),
                "-o".into(),
                format!("{OUTPUT_DIR}/frame_"),
                "-s".into(),
                shard_range.start.to_string(),
                "-e".into(),
                shard_range.end.to_string(),
                "-a".into(),
            ];
            v.extend(extra.iter().cloned());
            v
        }
        Kind::Chrome => {
            // Headless capture; the wrapper image reads the input batch and writes per-index output.
            // We pass the index range so the wrapper renders exactly this shard.
            let mut v = vec![
                "ce-render-chrome".into(),
                "--input".into(),
                INPUT_PATH.into(),
                "--out".into(),
                OUTPUT_DIR.into(),
                "--from".into(),
                shard_range.start.to_string(),
                "--to".into(),
                shard_range.end.to_string(),
            ];
            v.extend(extra.iter().cloned());
            v
        }
        Kind::Custom => extra
            .iter()
            .map(|tok| {
                tok.replace("{start}", &shard_range.start.to_string())
                    .replace("{end}", &shard_range.end.to_string())
                    .replace("{input}", INPUT_PATH)
                    .replace("{out}", OUTPUT_DIR)
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(a: u32, b: u32) -> FrameRange {
        FrameRange::new(a, b).unwrap()
    }

    #[test]
    fn blender_argv_carries_frame_range() {
        let argv = render_argv(Kind::Blender, rng(10, 20), &[]);
        // -s 10 -e 20 must appear in order
        let s = argv.iter().position(|a| a == "-s").unwrap();
        assert_eq!(argv[s + 1], "10");
        let e = argv.iter().position(|a| a == "-e").unwrap();
        assert_eq!(argv[e + 1], "20");
        assert!(argv.contains(&"-a".to_string()), "animation flag renders the whole range");
        assert_eq!(argv[0], "blender");
    }

    #[test]
    fn blender_appends_extra_args() {
        let argv = render_argv(Kind::Blender, rng(1, 1), &["--render-format".into(), "PNG".into()]);
        assert!(argv.windows(2).any(|w| w == ["--render-format", "PNG"]));
    }

    #[test]
    fn chrome_argv_carries_index_range() {
        let argv = render_argv(Kind::Chrome, rng(0, 5), &[]);
        let from = argv.iter().position(|a| a == "--from").unwrap();
        assert_eq!(argv[from + 1], "0");
        let to = argv.iter().position(|a| a == "--to").unwrap();
        assert_eq!(argv[to + 1], "5");
    }

    #[test]
    fn custom_substitutes_placeholders() {
        let tmpl = vec!["mytool".into(), "--range={start}:{end}".into(), "{input}".into(), "-o".into(), "{out}".into()];
        let argv = render_argv(Kind::Custom, rng(7, 9), &tmpl);
        assert_eq!(argv, vec!["mytool", "--range=7:9", INPUT_PATH, "-o", OUTPUT_DIR]);
    }

    #[test]
    fn default_images_per_kind() {
        assert_eq!(Kind::Blender.default_image(), "ce-net/blender:latest");
        assert_eq!(Kind::Chrome.default_image(), "ce-net/headless-chrome:latest");
        assert_eq!(Kind::Custom.default_image(), "alpine:latest");
    }

    #[test]
    fn kind_json_is_kebab() {
        assert_eq!(serde_json::to_string(&Kind::Chrome).unwrap(), "\"chrome\"");
        let k: Kind = serde_json::from_str("\"blender\"").unwrap();
        assert_eq!(k, Kind::Blender);
    }
}
