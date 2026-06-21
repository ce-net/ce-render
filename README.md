# ce-render — headless-browser / Blender render farm over CE

Screenshot/PDF/scrape and 3D render at scale, billed per frame, across borrowed browsers and GPUs on
the CE mesh. Embarrassingly parallel → mesh scaling is near-linear; output is blob-addressed (fetched
once, cached everywhere, tamper-evident by CID).

ce-render is an **application built on CE primitives** (the SDK tier, like `swarm` / `rdev` /
`ce-pin`), not a node feature. It uses only existing primitives:

- **directed placement** — `mesh_deploy` a container cell (Blender / headless-Chromium) on a chosen host;
- **content-addressed blobs** — each shard's output is a CID, fetched with `get_object` (CID = integrity proof);
- **atlas + history** — discover and rank hosts (proven delivered work first, GPU-tag filterable);
- **capabilities** — every host authorizes a signed `ce-cap` chain against `render:frame` before launching;
- **payment channels** — per-frame billing in integer base units (never floats);
- **the beacon** — public PoW randomness seeds an unpredictable verification spot-check.

## How it works

```
ce-render submit --config ce-render.toml -o out.manifest.json
```

1. **Split** the frame range (`1-500`) into shards of `shard_size` frames each — a clean partition
   (contiguous, gap-free, union == range).
2. **Rank + place** — discover docker-capable hosts from the atlas, rank by on-chain delivered work,
   `mesh_deploy` each shard on a host with the per-shard render command and the deploy `grant`.
3. **Gather** — collect each shard's output CID; a shard whose host died shows as *missing* frames.
4. **Verify** — re-render a beacon-seeded `verify` fraction of shards on a *different* host and compare
   output CIDs. A divergence (a planted black frame, a cached lie) is flagged, withheld from payment,
   and the frame re-assigned.
5. **Reassemble** — write a frame-ordered manifest of `(frame, host, output_cid)`; `fetch-all` pulls
   every CID back from the data layer.

## CLI

```
ce-render hosts [--select gpu]                       # ranked candidate render hosts
ce-render plan   --config ce-render.toml             # dry-run: show the placement plan
ce-render submit --config ce-render.toml -o m.json   # split, scatter, gather, verify, write manifest
ce-render manifest m.json                            # inspect completeness / missing / diverged
ce-render fetch-all m.json -o out/                   # pull every output CID into out/
```

Global flags: `--node <url>` (default `http://127.0.0.1:8844`), `--cap <hex>` (the `render:frame`
capability chain; also read from the config `cap` field, `$CE_RENDER_CAPS`, or
`<config dir>/ce-render/caps`).

## ce-render.toml

See [`examples/ce-render.toml`](examples/ce-render.toml). Key fields:

| field | meaning |
|---|---|
| `kind` | `blender` \| `chrome` \| `custom` |
| `image` | container image (defaults per kind) |
| `input_cid` | CID of the input asset (scene / URL batch) staged into each cell |
| `frames` | inclusive range `"1-500"` or single `"42"` |
| `shard_size` | frames per shard |
| `hosts` | max distinct hosts to fan out to |
| `select` | atlas self-tag filter (e.g. `gpu`) |
| `bid_per_frame` | **base units as a decimal string** (10^18 = 1 credit) — never a float |
| `verify` | fraction `[0,1]` of frames re-rendered on a 2nd host to catch frauds |
| `args` | extra container args appended to the per-shard command |
| `cap` | hex `ce-cap` chain granting `render:frame` |

For `kind = "custom"` the `args` are the full command; `{start}`, `{end}`, `{input}`, `{out}` are
substituted per shard.

## Trust & money

Authorization is the one CE primitive: a host authorizes a signed, attenuating `ce-cap` chain rooted
at itself or a configured org root against `render:frame` before launching a cell. ce-render only
*presents* the chain (the deploy `grant`) — it never mints trust. Money is integer base units carried
as decimal strings, never floats; the per-shard bid is `bid_per_frame * frames_in_shard`. Mesh-first:
hosts are addressed by NodeId, never a stored ip:port.

The verify dial is the trust crux for exec work: re-running a frame on a different host catches a host
that did not run the real workload. As the portfolio spec notes, a colluding worker + canary peer can
still defeat it — the dial raises the cost of cheating, it is not a hard guarantee. Non-reproducible
work (e.g. LLM inference) is out of scope (needs TEE attestation).

## Module map

| module | role |
|---|---|
| `shard` | pure frame-range sharding (unit-tested partition invariant) |
| `manifest` | pure result-manifest assembly (output CIDs + missing + diverged) |
| `proto` | render abilities + per-shard container command builder |
| `placement` | pure host ranking + round-robin assignment |
| `verify` | pure beacon-seeded canary selection + output-CID comparison |
| `config` | `ce-render.toml` loader/validator |
| `caps` | resolve the `ce-cap` chain presented to hosts |
| `farm` | live scatter/gather over `mesh_deploy`, gluing the pure modules |

## Tests

```
cargo test          # 54 unit + 7 integration, no live cluster needed
```

The unit tests cover the load-bearing pure logic — frame-range sharding (partition invariant),
result-manifest assembly (frame order, missing, diverged, last-write-wins re-assignment), the
beacon-seeded verify selection, and the per-shard command builder. The farm's scatter/gather/verify
flow is tested end-to-end against a scripted fake deployer (including a planted fraud host that the
verify dial catches). The integration tests additionally exercise the exact `ce-cap` authorization a
render host performs (a `render:frame` chain authorizes; an unrelated ability or a stranger-rooted
chain is denied).

## Status

The sharding, placement, verify dial, manifest assembly, config, and capability handling are complete
and tested. The one live-path gap is reading a completed container cell's **output CID** back through
ce-rs: the `/mesh-deploy` response carries it, but the SDK's typed `mesh_deploy` returns only the job
id and `ce_rs::Job` has no `output` field, so the farm currently records an empty CID for completed
container renders (surfaced as a *missing* frame — never a silent wrong result). Closing it is a small
ce-rs change (expose `output` on the deploy response / `Job`); see the `// TODO(ce-rs)` in
`src/farm.rs`. The container images (`images/blender.Dockerfile`, `images/chrome.Dockerfile`) are the
runtime side and are provided as buildable starting points.
