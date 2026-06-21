//! Host placement: choose which mesh hosts render the shards.
//!
//! Mesh-first and reputation-aware, the swarm pattern: candidates are docker-capable atlas entries
//! (optionally GPU-tagged for Blender), ranked by proven delivered work (`history().delivered_work()`)
//! and liveness, then assigned shards round-robin. The ranking is pure so it is unit-testable without
//! a live mesh; the live discovery (`atlas` + `history`) lives in the farm.

use ce_rs::AtlasEntry;

/// A candidate render host distilled to the signals we rank on — kept as an owned, network-free
/// struct so the ranking is testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub node_id: String,
    /// Proven delivered work (settled jobs + heartbeats hosted). Higher = more trusted.
    pub delivered_work: u64,
    /// Seconds since last seen advertising capacity. Lower = fresher.
    pub last_seen_secs: u64,
    /// Currently-running jobs — prefer less-loaded hosts.
    pub running_jobs: u32,
}

/// Docker-capable atlas entries, optionally filtered by a self-tag (e.g. `gpu`). Mirrors swarm's
/// `candidates`: a host must advertise `docker` to run a cell.
pub fn candidates(atlas: &[AtlasEntry], select: Option<&str>) -> Vec<AtlasEntry> {
    atlas
        .iter()
        .filter(|h| h.has_tag("docker"))
        .filter(|h| select.is_none_or(|t| h.has_tag(t)))
        .cloned()
        .collect()
}

/// Rank candidates best-first:
///   1. more delivered work (descending) — trust proven hosts;
///   2. fewer running jobs (ascending) — spread load;
///   3. seen more recently (ascending) — prefer live hosts;
///   4. node_id (ascending) — stable deterministic tie-break.
pub fn rank(candidates: &[Candidate]) -> Vec<Candidate> {
    let mut v = candidates.to_vec();
    v.sort_by(|a, b| {
        b.delivered_work
            .cmp(&a.delivered_work)
            .then(a.running_jobs.cmp(&b.running_jobs))
            .then(a.last_seen_secs.cmp(&b.last_seen_secs))
            .then(a.node_id.cmp(&b.node_id))
    });
    v
}

/// Pick up to `n` distinct best hosts to fan out to.
pub fn select(candidates: &[Candidate], n: usize) -> Vec<String> {
    rank(candidates).into_iter().map(|c| c.node_id).take(n).collect()
}

/// Assign `shard_count` shards across `hosts` round-robin, returning, for each shard index, the host
/// that renders it. Pure → unit-tested. Empty `hosts` yields an empty assignment (caller errors).
pub fn assign(shard_count: usize, hosts: &[String]) -> Vec<String> {
    if hosts.is_empty() {
        return Vec::new();
    }
    (0..shard_count).map(|i| hosts[i % hosts.len()].clone()).collect()
}

/// Pick a verifier host for a shard already rendered by `original` — any ranked host that is not the
/// original (so a re-render lands on a *different* machine). Returns `None` if no distinct host exists.
pub fn verifier_for(original: &str, hosts: &[String]) -> Option<String> {
    hosts.iter().find(|h| h.as_str() != original).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, work: u64, seen: u64, jobs: u32) -> Candidate {
        Candidate { node_id: id.into(), delivered_work: work, last_seen_secs: seen, running_jobs: jobs }
    }

    fn atlas_entry(id: &str, tags: &[&str]) -> AtlasEntry {
        AtlasEntry {
            node_id: id.into(),
            cpu_cores: 4,
            mem_mb: 8192,
            running_jobs: 0,
            last_seen_secs: 0,
            tags: tags.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn candidates_require_docker_and_match_select() {
        let atlas = vec![
            atlas_entry("a", &["docker", "gpu"]),
            atlas_entry("b", &["gpu"]),         // no docker → excluded
            atlas_entry("c", &["docker"]),       // docker, no gpu
        ];
        let all: Vec<_> = candidates(&atlas, None).iter().map(|h| h.node_id.clone()).collect();
        assert_eq!(all, vec!["a", "c"]);
        let gpu: Vec<_> = candidates(&atlas, Some("gpu")).iter().map(|h| h.node_id.clone()).collect();
        assert_eq!(gpu, vec!["a"]);
    }

    #[test]
    fn ranks_proven_then_least_loaded() {
        let cs = vec![cand("a", 0, 1, 0), cand("b", 5, 1, 3), cand("c", 5, 1, 1)];
        let ranked: Vec<_> = rank(&cs).into_iter().map(|c| c.node_id).collect();
        // b,c tie on work; c has fewer jobs → c first; a (work 0) last
        assert_eq!(ranked, vec!["c", "b", "a"]);
    }

    #[test]
    fn select_caps_to_n() {
        let cs = vec![cand("a", 9, 1, 0), cand("b", 8, 1, 0), cand("c", 7, 1, 0)];
        assert_eq!(select(&cs, 2), vec!["a", "b"]);
    }

    #[test]
    fn assign_round_robin() {
        let hosts = vec!["h1".to_string(), "h2".to_string()];
        assert_eq!(assign(5, &hosts), vec!["h1", "h2", "h1", "h2", "h1"]);
        assert!(assign(3, &[]).is_empty());
    }

    #[test]
    fn verifier_is_a_different_host() {
        let hosts = vec!["h1".to_string(), "h2".to_string()];
        assert_eq!(verifier_for("h1", &hosts).as_deref(), Some("h2"));
        assert_eq!(verifier_for("h1", &["h1".to_string()]), None);
    }
}
