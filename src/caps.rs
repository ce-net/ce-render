//! Capability resolution for the client side.
//!
//! ce-render does not mint trust — it presents a signed, attenuating `ce-cap` chain (the deploy
//! `grant`) that each render host authorizes against `render:frame` before launching a cell. The
//! chain is produced out-of-band (`ce grant <holder> --can render:frame ...` on the host or an org
//! root, then handed to the operator's wallet). The client resolves the hex chain from, in order:
//!   1. the explicit `--cap <hex>` flag (highest precedence);
//!   2. the `cap` field in `ce-render.toml`;
//!   3. the `$CE_RENDER_CAPS` environment variable;
//!   4. `<config dir>/ce-render/caps` (a file containing the hex chain).
//!
//! An empty result is allowed only when the target host roots at itself and self-issues; otherwise
//! the host's `authorize` rejects it, surfacing a clear "denied" rather than ce-render guessing.

use std::path::PathBuf;

/// Resolve the capability chain hex the client presents. `explicit` is the `--cap` flag; `from_config`
/// is the `cap` field of `ce-render.toml` (already trimmed by the loader, `None` if absent).
pub fn resolve(explicit: Option<&str>, from_config: Option<&str>) -> String {
    if let Some(c) = explicit.map(str::trim).filter(|c| !c.is_empty()) {
        return c.to_string();
    }
    if let Some(c) = from_config.map(str::trim).filter(|c| !c.is_empty()) {
        return c.to_string();
    }
    if let Ok(c) = std::env::var("CE_RENDER_CAPS") {
        let c = c.trim().to_string();
        if !c.is_empty() {
            return c;
        }
    }
    if let Some(p) = caps_file()
        && let Ok(c) = std::fs::read_to_string(&p)
    {
        let c = c.trim().to_string();
        if !c.is_empty() {
            return c;
        }
    }
    String::new()
}

fn caps_file() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CE_RENDER_DIR") {
        return Some(PathBuf::from(d).join("caps"));
    }
    directories::ProjectDirs::from("", "", "ce-render").map(|p| p.config_dir().join("caps"))
}

/// `mesh_deploy`'s `grant` argument: `None` when no chain is configured (lets the node/host produce a
/// clean "no capability" denial), else `Some(hex)`.
pub fn grant(chain: &str) -> Option<&str> {
    let c = chain.trim();
    if c.is_empty() { None } else { Some(c) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_takes_precedence_over_config() {
        assert_eq!(resolve(Some("deadbeef"), Some("c0ffee")), "deadbeef");
        assert_eq!(resolve(Some("  abc  "), None), "abc");
    }

    #[test]
    fn config_used_when_no_flag() {
        assert_eq!(resolve(None, Some("c0ffee")), "c0ffee");
        assert_eq!(resolve(Some("  "), Some("c0ffee")), "c0ffee");
    }

    #[test]
    fn empty_everywhere_yields_empty() {
        unsafe {
            std::env::remove_var("CE_RENDER_CAPS");
            std::env::set_var("CE_RENDER_DIR", "/nonexistent-ce-render-dir-xyz");
        }
        assert_eq!(resolve(Some("  "), Some("  ")), "");
        unsafe {
            std::env::remove_var("CE_RENDER_DIR");
        }
    }

    #[test]
    fn grant_maps_empty_to_none() {
        assert_eq!(grant(""), None);
        assert_eq!(grant("  "), None);
        assert_eq!(grant("abc"), Some("abc"));
    }
}
