# Concurrent Agent Sandboxes

**Status:** `create` / `--from` implemented and tested. `stage` / `seal` are
design investigations (this doc).

## The model

Several agents work at once, each in a private working tree, sharing **one**
canonical graph. Two facts make it simple:

1. **The working tree is cloned copy-on-write; the graph is not.** A sandbox is
   a reflink clone of the working tree (APFS `clonefile`, Btrfs/XFS `FICLONE`,
   ReFS block-clone — one path via the `reflink-copy` crate, falling back to a
   plain copy). Five agents off a 200 MB base cost a few MB, not a gigabyte, and
   run at bare-metal filesystem speed — no FUSE, no virtio, no VM.
2. **There is exactly one pristine.** The sandbox carries a small pointer file
   (`.atomic-sandbox`) naming the canonical repo and the sandbox's view. Every
   `atomic` command run inside the sandbox resolves the graph from there, so
   `status`, `record`, `diff`, `vault query` all work as in a normal repo.

This is "views, not forks" taken to the filesystem: one graph, many cheap
working-tree projections.

### Why not the things we tried first

Recorded here so nobody re-walks them: a host-side FUSE overlay (macFUSE) is
impractical to deploy on Apple Silicon + macOS 26 (Recovery-Mode security
downgrade, kext staging, and a fuser/libfuse2 ABI break) and showed no speed
win; virtio-fs virtualizes the filesystem protocol and is death for the
metadata-heavy workloads (git, cargo, npm) we care about. Copy-on-write on the
real filesystem needs none of it.

## Implemented

### `Repository` (atomic-repository/src/repository/sandbox.rs)

- `provision_sandbox(dest, view)` — reflink-clone the working tree into `dest`,
  skipping `.atomic/` (shared) and nested sandboxes; write the `.atomic-sandbox`
  pointer. In-process in the binary — no shell.
- `open_sandbox(working_root, canonical, view)` — working tree in one place, the
  one canonical graph in another.
- `detect_sandbox()` — wired into `open` / `open_existing` / `open_readonly`, and
  the CLI's `find_repository_root`, so any command inside a sandbox routes to the
  canonical graph.

### CLI

```text
atomic sandbox create <NAME> [--dest <PATH>] [--view <VIEW>] [--from <VIEW>]
```

- `--from <view>` forks a per-agent **draft** view (named after the sandbox)
  from `<view>`, so the agent's records land in its own draft — isolating each
  agent's history as well as its files.
- `--view <view>` records into an existing view; default is the current view.

Covered by `tests/harness/20_sandbox.sh` (18 assertions): clone happens, graph
is not cloned, artifacts are per-agent, commands work inside the sandbox, records
land in the canonical graph / the per-agent draft, the canonical working tree is
untouched.

### Concurrency caveat

The harness is sequential. Multiple agent **processes** opening the canonical
pristine concurrently rely on redb's cross-process locking — records serialize
(the intended model), but simultaneous writers haven't been load-tested. Verify
before relying on true parallelism.

---

## Investigation: `atomic sandbox stage` and `atomic sandbox seal`

Both turn a sandbox into a shippable OCI artifact. They differ in *what* they
package and *why*.

| | `stage` | `seal` |
|--|--|--|
| Purpose | inner-loop CI / circuit-breaker runs | deployable runtime |
| Shape | **layered**: shared base + thin delta | **flattened**: one self-contained rootfs |
| Base sharing | yes — base keyed by the shared view's Merkle, pulled once and cached | no — fully self-contained |
| Ships each iteration | only the delta (small, fast) | the whole image (release artifact) |
| Lifecycle | ephemeral, per turn | versioned release |

### What each side already provides

**atomic** (graph → rootfs + dependency metadata):
- Materialize a view's visible state to a directory (the core `materialize_view`
  over a `FileSystem` working copy rooted at an arbitrary dir — the same path
  `Repository::materialize` uses, pointed elsewhere).
- The dependency closure of a view: `collect_visible_change_ids_with_deps`
  (already public) — the "dependency xxxx" the artifact must carry.
- Per-view change sets: `collect_visible_change_ids` — the symmetric difference
  between the draft and its base is the "delta yyyy".
- Merkle state per view — a stable content-address for the shared base.

**smolvm** (rootfs → OCI artifact, registry):
- `pack create` already accepts `--image`, `--from-vm`, and a hidden
  `--rootfs-dir` — i.e. packing a directory into a `.smolmachine` is mostly
  plumbed.
- `pack push` / `pull` move `.smolmachine` artifacts to/from an OCI registry.
- `BlobCache` + the registry are content-addressed (sha256) — the substrate for
  "base pulled once, cached, delta ships each time."
- `ContainerizationEXT4` (Apple) can build ext4 images natively on macOS (drops
  the e2fsprogs dependency) if we want block images instead of layer dirs.

### `atomic sandbox stage` — flow

1. Resolve base = the shared view (`--from`) at its current Merkle `M`.
2. If the base layer for `M` isn't in the registry, materialize the shared view
   to a dir, pack it (`pack --rootfs-dir`), push it tagged by `M`. Otherwise
   reuse it (this is the win — built once, shared across all stagings).
3. Materialize **only the delta** (changes in the sandbox's draft not in the
   base) to a dir → pack as a thin layer on top of base `M`.
4. Emit an OCI image = `[base@M] + [delta]` plus a reinflate manifest (which base
   to pull, how to stack). Push.
5. CI pulls base (cached) + delta (small) → smolvm reinflates → runs build/test
   and circuit-breaker checks against the agent's WIP. Cheap inner loop.

### `atomic sandbox seal` — flow

1. Compute the full dependency closure of the view (base deps + draft changes).
2. Materialize the **merged** state (base + delta flattened) to a single rootfs.
3. Add a runtime layer (toolchain/entrypoint needed to *run* the app).
4. Pack as a self-contained `.smolmachine` / OCI image, push as a versioned
   release. Runs "this exact version" as a runtime anywhere smolvm runs.

### Gaps to close (the actual work)

- **atomic:** a `materialize_view_to(view, dir)` helper (re-introduces the
  directory-targeted materialize), and a `delta-vs-base` change-set computation
  surfaced as a stable list for packing.
- **smolvm:** a supported (un-hidden) `pack --rootfs-dir` entry or a library API
  to pack a directory + a parent-layer reference into a layered `.smolmachine`;
  base-by-digest reuse on push.
- **shared:** the reinflate manifest schema (base digest + delta + how smolvm
  stacks them — in-guest overlayfs over two block layers, or a flattened rootfs).

### Open questions

- Layer boundary: ship base/delta as two ext4 block images (overlayfs in-guest)
  or as OCI tar layers (merged at unpack)? Block images keep native FS speed in
  CI; tar layers are more registry-standard.
- Does `stage` include build artifacts (`target`, `node_modules`) to skip a cold
  build in CI, or only source? Artifacts bloat the delta but cut CI time —
  probably a flag.
- Provenance: seal/stage artifacts should embed the view Merkle + change hashes
  so a shipped image is traceable back to exact graph state (ties into the
  existing attestation/provenance model).
