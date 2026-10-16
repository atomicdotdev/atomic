# libatomic

Atomic's Rust service-layer crate: canonical protobuf definitions, generated
Rust types, and the existing repository handlers. It remains a member of the
Atomic workspace; consumers do not need to become workspace members.

## Use from a separate local project

```toml
[dependencies]
libatomic = { path = "../atomic/libatomic", version = "=0.19.1" }
prost = "0.13"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

Cargo builds this crate and its local workspace dependencies directly. No source
copy, crate publication, socket, or service process is required. The consumer
can keep its own manifest, lockfile, target directory, and release lifecycle.
The version check above verifies the local crate; it is not a registry fallback.

## Embedded service API

`LocalService` provides a protobuf-byte boundary for language bindings. Reuse an
instance (or its clones) so calls share routing and repository gates:

```rust,no_run
use libatomic::{proto, LocalService};
use prost::Message;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let service = LocalService::new();
let request = proto::ResolveRepositoryRequest { path: "/work/project".into() };
let bytes = service.call(
    "/atomic.DaemonService/ResolveRepository", &request.encode_to_vec(),
).await?;
let resolved = proto::ResolveRepositoryResponse::decode(bytes.as_slice())?;
let request = proto::StatusRequest { repository: resolved.repository, ..Default::default() };
let bytes = service.call(
    "/atomic.RepositoryQueryService/Status", &request.encode_to_vec(),
).await?;
let status = proto::StatusResponse::decode(bytes.as_slice())?;
println!("{}", status.view);
# Ok(()) }
```

Supported local methods are ResolveRepository, Status, VaultService.GetVaultEntry,
ListVaultEntries, GetVaultContext, and AttestationService.VerifyAttestation.
Mutations and other methods return `Unimplemented`. Malformed messages and missing
repository references return `InvalidArgument`.
Repository references belong to the resolving service instance; its clones share
them. Methods delegate to the existing handlers, not a parallel implementation.
This API is for trusted local callers and does not run remote authentication
middleware. Existing redb locking and database-open recovery behavior remain.

`FILE_DESCRIPTOR_SET` contains the canonical protobuf descriptor compiled into
this crate. Bindings can generate matching codecs/types without vendoring proto
sources or requiring system protoc. This exports the full contract; it does not
claim that every contract method has a local-dispatch implementation.

## Vault and signed-revision reads

Vault reads delegate to the existing shared handlers. The current vault store is
repository-wide: explicit view requests and continuation cursors fail closed,
and responses do not claim a ViewSnapshot. View isolation remains separate work.

VerifyAttestation reads the stored signed JSON-LD and source under one read-only
repository open. It returns signature validity and stored-source freshness
separately, plus the exact signed document, signer DID and revision hash. The
compatibility `valid` flag requires both a valid signature and fresh source.
Unknown public keys leave signature validity absent; unsupported attestation-ID
verification returns Unimplemented rather than claiming an invalid signature.
Verification never loads a private key or creates an identity store. An explicit
32-byte public key may be supplied instead of looking up existing public identity
metadata. These results do not establish signer trust or editor-buffer freshness.
Existing repository upgrade/repair-on-open behavior is unchanged; healthy-read
non-mutation is covered by tests.

## Optional Node addon

All Rust code, including Node-API exports, belongs to this crate. Build them with:

```sh
cargo build --locked -p libatomic --features node
```

The `node` feature exports `call(method, protobufBytes)` and `descriptor()`.
There is no second Rust binding crate, socket, or service process. The feature
is off by default; ordinary Rust consumers do not need the Node-API dependencies.

### Distributed library names

The Rust crate and imports remain `libatomic`. Cargo automatically adds a `lib`
prefix on Unix, so the release packaging step removes the doubled prefix:

| Release target | Cargo build output | Distributed filename |
| --- | --- | --- |
| macOS ARM64 (`aarch64-apple-darwin`) | `liblibatomic.dylib` | `libatomic.dylib` |
| Linux x64 glibc (`x86_64-unknown-linux-gnu`) | `liblibatomic.so` | `libatomic.so` |
| Windows x64 (`x86_64-pc-windows-msvc`) | `libatomic.dll` | `libatomic.dll` |

For example, build and stage the release runtime on an Apple Silicon Mac:

```sh
cargo build --locked --release --target aarch64-apple-darwin -p libatomic --features node
node libatomic/scripts/package-runtime.mjs \
  --target aarch64-apple-darwin \
  --build-dir target/aarch64-apple-darwin/release \
  --output-dir target/runtime/aarch64-apple-darwin
```

Run these commands from the Atomic workspace root. Each output directory must be
new: packaging refuses to overwrite an already staged, possibly loaded library.
The script requires a matching host OS/CPU and Node-API 8+, loads the normalized
library with `process.dlopen`, and checks the Node exports and nonempty protobuf
descriptor. It then writes `libatomic-runtime.json` with the crate version, Git
revision, Rust target, OS/CPU, required Node-API version, filename, and SHA-256
checksums for the library and embedded descriptor. `--expected-version VERSION`
additionally rejects a mismatch with the workflow's release version. Cargo's
`--locked` build and the pinned workflow checkout provide the producer inputs;
the manifest is build metadata, not an independent signature or compatibility
guarantee.

These are **Node-enabled addons**, not a newly defined stable C ABI. Merely
renaming a library built without `--features node` does not make it usable by
nodeatomic. A Node loader must use `process.dlopen` for these platform suffixes;
ordinary `require()` expects `.node` for native addons. Consumers that currently
expect `libatomic.node` can still copy the distributed artifact under that name.

The TypeScript consumer lives separately at `~/Projects/nodeatomic`, with no
Rust source or Cargo manifest. Build this producer explicitly; the consumer never
invokes Cargo or discovers this checkout. For example, after the macOS build:

```sh
cd ~/Projects/nodeatomic
LIBATOMIC_ADDON=~/Projects/atomic/target/runtime/aarch64-apple-darwin/libatomic.dylib npm run install:addon
npm run build
```

The consumer can instead install an artifact supplied by CI, without Atomic's
source or a Rust toolchain. `install:addon` copies an explicit binary;
`npm run build` only generates protobuf codecs/types and compiles TypeScript.
Dispatch and repository access remain in this crate either way.

### CLI release archives

`.github/workflows/release.yml` builds the Node-enabled runtime separately from
the CLI for the three targets above. Both builds use the same pinned Git revision
and workspace release version. The matching `atomic-<target>` archive includes:

```text
atomic (or atomic.exe)
libatomic.dylib (or libatomic.so / libatomic.dll)
libatomic-runtime.json
```

Linux x64 uses the existing glibc 2.28 floor for **both** the CLI and library;
the workflow checks their ELF requirements. Node smoke tests run on the matching
runner before archiving. Release archive SHA-256 checksums cover the runtime too.
Other existing CLI targets remain CLI-only: no Intel Mac, Linux ARM64, or musl
Node addon is advertised by this matrix.

Install the entire archive into a versioned, application-private directory and
add that directory to PATH, rather than moving only the executable. Do not put
this `libatomic.so` into a global system library directory, where it could collide
with the compiler runtime of the same name. Install upgrades side by side and
restart editor hosts before removing old runtimes. External installers and
`atomic update` still need follow-up work to install/update the library; publishing
these archives alone does not change those installation paths.

The consumer must still verify runtime contract compatibility, not just OS/CPU.
Automatic installed-runtime discovery in nodeatomic and a universal Catalyst VSIX
are separate follow-ups; existing consumers continue using their explicitly
installed/bundled addon. CI's Node smoke does not establish VS Code/Electron
extension-host compatibility, which needs consumer testing on each platform.

## Test

```sh
cargo test --locked -p libatomic --test local_service_test
node --test libatomic/scripts/package-runtime.test.mjs
```

The tests exercise byte round trips, descriptor availability, malformed requests,
reference isolation, and release of database handles after a status call.

This crate is not published. Publishing will require versioned registry
references for Atomic's dependency crates and confirmed ownership of their
package names; none of those changes are part of this local test.
