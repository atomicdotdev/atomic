# Investigation: a stable, language-neutral C ABI for libatomic

## Conclusion

Feasible without a service process, a database change, or a separate Atomic
engine build for each language. Keep the implementation in `libatomic`; ship
one shared library per supported OS/architecture. Node, Java, Go, and Python can
consume that same library through their FFI facilities.

The protobuf contract is already a good payload boundary. The difficult work is
not moving bytes into Rust: it is defining and testing ownership, runtime and
shutdown behavior, panic containment, errors, versioning, and binary distribution.

**Planning estimate for one experienced Rust developer:**

- Local two-method proof of concept: **3–5 engineer-days**.
- Reviewed, releaseable ABI v1 for ResolveRepository/Status, with Node integration
  and an initial platform matrix: **10–15 engineer-days total**, including the
  proof of concept rather than adding it again.
- Minimal Java and Go interoperability checks: **another 2–4 engineer-days total**.
- Wider unary dispatch and coverage: **another 5–10 engineer-days** for adapters
  and representative tests, not completion of missing domain features.
- Streaming support for already-implemented streaming operations: **another
  5–10 engineer-days**.
- Full multi-platform SDK coverage with cancellation, recovery, and broad service
  semantics is a **4–8 engineer-week planning range**, with substantial uncertainty.
  Completion of missing Git/Reactor/domain functionality must be estimated separately.

These are design/implementation/testing estimates, not measured development time
or commitments. They assume reuse of existing handlers and CI, a trusted local
caller model, and no simultaneous storage redesign.

## What was investigated

- `libatomic/src/local.rs`, the Node adapter, build script, manifests and descriptor.
- Service trait implementations and streaming implementations under `daemon/`.
- `proto/CONTRACT.md` and the canonical request/response/error definitions.
- redb-open and per-repository gate behavior in `daemon/state.rs`.
- Existing CI/release workflows, including the Linux glibc compatibility floor.
- Rust's official FFI/unwinding documentation, Go cgo pointer rules, Java FFM
  documentation, and Koffi's synchronous/asynchronous FFI interface.
- A disposable C ABI probe, built outside the repositories, linked to the existing
  local libatomic crate without its Node feature.

No production implementation was changed for this investigation. The probe lives
under `/tmp/atomic-abi-investigation/`; its compiled artifact was produced in the
existing Cargo build cache. It is not safe to treat the probe as a stable API.

## Evidence from the disposable probe

On the available macOS ARM64 host:

1. A plain C executable loaded `libatomic_abi_probe.dylib` with `dlopen`, retrieved
   its descriptor, resolved a repository and performed **64 status calls from
   four C threads**. It checked error returns, released output buffers, destroyed
   its client, and closed the library.
2. Node used **Koffi 3.3.3** to load that same ordinary dylib, perform repository
   resolution and **32 asynchronous status calls**, decode a result produced by
   the C executable, and exercise malformed/unknown-method errors and buffer release.
   No Atomic `.node` addon was loaded. Koffi itself has its own native FFI component.
3. Python `ctypes` loaded the same dylib and exercised ABI version/create/destroy.
   This was a lifecycle smoke test, not a Python status integration.
4. Export inspection showed the probe's six C functions and a dependency-exported
   Blake3 symbol. There were **no unresolved Node-API symbols** in the probe library.
5. `otool -L` showed macOS system dependencies and an absolute debug-build install
   name. Release packaging must fix that install name for linked consumers.

The temporary bridge is only 49 lines of Rust, plus a 58-line C test and a 34-line
Node test. That demonstrates feasibility, not the size of the production work.
It lacks a stable error model, complete input limits, poisoned-client behavior,
close/call race handling, ABI compatibility tests, and a release build policy.

Go and Java runtimes were not installed on this host. Their integration paths
were checked against official documentation but **not executed**. Linux, Windows,
release optimization, sanitizer coverage and mutation/stream cancellation were
not tested by this probe.

## Current implementation and contract inventory

`libatomic` already produces `rlib` and `cdylib` artifacts. Its optional `node`
feature exposes Node-API functions, not a general C API. `LocalService::call` already
provides the reusable protobuf-byte dispatch layer, currently supporting only:

- `/atomic.DaemonService/ResolveRepository`
- `/atomic.RepositoryQueryService/Status`

`FILE_DESCRIPTOR_SET` is generated from this crate's canonical proto tree. Rust
codecs, request/response types and the existing handlers can be reused.

The **current source and freshly built C probe** contain **101 methods across
15 services: 95 unary and 6 streaming**. A source audit found service trait method
implementations for 93 declarations, including 90 unary methods and 3 streams.
Presence of a trait method is not proof of functional or contract-complete behavior.

| Service | Declared methods |
|---|---:|
| Daemon | 6 |
| RepositoryQuery | 8 |
| RepositoryMutation | 12 |
| View | 6 |
| Tag | 4 |
| Vault | 11 |
| Knowledge | 2 |
| Attestation | 4 |
| Provenance | 18 |
| Sync | 5 |
| Sandbox | 11 |
| Maintenance | 4 |
| Triage | 2 |
| GitInterop | 3 |
| Reactor | 5 |

No corresponding service trait implementations were found for the 3 GitInterop
or 5 Reactor declarations. `GetProvenance` and `ReindexWorkspace` explicitly return
`Unimplemented`; some vault variants also remain unsupported. Those are existing
service gaps, not functions a C bridge can implement by forwarding bytes.

The six streaming declarations are Diff, DiffWorkingCopy, Materialize,
PublishStream, Subscribe and Replay. Only the first three have handlers here.

### Artifact drift was observed

The previously built Node addon embedded a 99-method descriptor. Current source
and the newly built probe contain 101, including AppendHookLog and LoadHookLog.
Older prose also refers to 94 or 97 methods. This makes a concrete case for
reporting a build ID/schema fingerprint and checking supported methods, rather
than treating an artifact filename or package version as the complete contract.

## Recommended ABI v1 scope

Start with a small synchronous C interface for trusted local callers. The function
blocks on the **calling thread**, while Rust owns its internal async runtime.
Node schedules it through Koffi's async worker interface; Java through its executor;
Go through cgo. These internal worker threads are not a service process.

Use an optional `c-abi` feature in libatomic. C ABI builds should not enable the
Node-API feature. Keep ordinary Rust APIs available without either feature.

Approximately **6–8 exports**, not one export per protobuf method:

- ABI/build/protocol information and supported-method discovery.
- Create client context.
- Call a supported unary method with method-name bytes and protobuf request bytes.
- Retrieve protobuf descriptor bytes.
- Release returned buffers/results.
- Close client context.

Exact signatures and names require review before committing to ABI v1. Use
`extern "C"`, explicit export names, opaque handles, fixed-width status codes,
C `size_t` for memory lengths, and `#[repr(C)]` only for deliberately frozen C
structs. Use output parameters rather than returning complex structs by value.
Do not expose Vec, String, Rust enums, Result, trait objects, futures, Tokio,
redb transactions, or generated protobuf structs through the ABI.

The message bodies continue to be the existing protobufs. A method identifier
that resembles a gRPC path does not imply an RPC transport. Unsupported methods
must fail explicitly and cannot be advertised as implemented merely because they
appear in the full descriptor.

## Decisions and work that make it stable

### 1. Ownership and lifetimes

Recommended initial rules:

- Input bytes are borrowed only for the duration of the C call. Decode/copy what
  is needed before any work that could outlive that call.
- Returned bytes are owned by libatomic. Callers copy or decode them, then invoke
  libatomic's release function. Never call the host's `free()` on Rust allocations.
- Allocate response/error state once per operation; do not implement a two-call
  "measure then execute" output-size protocol that could repeat a mutation.
- Document empty buffers, null pointers, length bounds, UTF-8 requirements,
  output initialization, and exact ownership on success and failure.
- Define close-versus-in-flight-call behavior. Do not reclaim a context while an
  FFI worker still uses it. Invalid/stale handles can be detected with a registry,
  or opaque pointer lifetimes can be an explicit caller obligation; choose before
  freezing the interface.
- Do not promise protection from arbitrary dangling/invalid C pointers. Null and
  length checks are useful but cannot establish that a supplied address is valid.

Opaque result handles are an alternative to pointer/length output structs and
can make stale-handle/double-release detection easier. They add accessor exports.
A plain owned buffer is simpler, provided its non-copyable ownership is documented
and wrapped correctly in each language.

### 2. Async runtime, threads and shutdown

The existing handlers use Tokio, `spawn_blocking` and sometimes `tokio::spawn`.
Rust futures cannot be returned directly over a C ABI.

The probe used one runtime per client and Runtime::block_on on foreign caller
threads; concurrent calls worked. Production should decide whether to use a shared
library-owned runtime and shared repository gates instead, avoiding one thread
pool and independent gate registry per client.

A robust alternative is scheduling onto the library runtime and synchronously
waiting on a result channel. This avoids accidentally nesting block_on within a
host's Tokio context. The call must not depend on the host driving an executor.

Define initialization failure, admission limits, number of workers, concurrent
calls, repeated create/close, and behavior after shutdown. Client closure and safe
library unloading are separate concerns. V1 can explicitly forbid unloading while
any call/client/buffer exists, or require process-lifetime loading, rather than
promising arbitrary hot unloading immediately.

### 3. Panic policy is a release blocker

The Atomic workspace release profile sets **`panic = "abort"`**. A Rust panic
in such a build can terminate the embedding application. catch_unwind cannot
catch aborting panics, and cannot recover arbitrary foreign faults or OOM aborts.

A stable embedding library should use a dedicated release profile with
**`panic = "unwind"`**, catch unwinding panics at every exported entry point and
inside async tasks as needed, and never let them unwind across the C boundary.
This can leave the CLI's release profile unchanged.

Several current handlers call `request.repository.as_ref().unwrap()` or contain
other assumptions. Protobuf decoding accepts missing optional fields, so dispatch
must validate required fields before invoking those handlers. Panic handling is
last-resort containment, not a substitute for validation. A contained panic may
also leave state poisoned: define whether to invalidate the client and refuse
further mutation rather than claiming it can safely continue.

The disposable tests used an unwinding development build. They do not prove the
workspace's current aborting release build is suitable for embedding.

### 4. Errors must remain machine-readable

Do not use thread-local "last error" or parse human message prose. Return error
information as part of the call result, with stable categories and owned bytes.
Preserve tonic status codes and structured ErrorInfo details where present.

`daemon/state.rs::domain_status` constructs and encodes ErrorInfo but currently
discards the bytes and uses an `atomic:CODE:` text prefix. The compaction handler,
in contrast, attaches ErrorInfo with Status::with_details. Normalize this behavior
before claiming uniform structured domain errors across all methods.

Also distinguish ABI misuse/version/unsupported-method failures from a legitimate
domain refusal. Responses containing refusal oneofs must remain successful
protobuf responses, not be converted into a generic FFI exception.

### 5. Binary ABI, message contract and DB schema are separate versions

- ABI version: signatures, layouts, constants, ownership and threading rules.
- Protobuf/behavioral contract: currently revision 2; reserved fields and behavioral
  changes still require negotiation even if decoding succeeds.
- Library package/build revision and embedded schema fingerprint.
- Repository schema/file format: governed by the existing storage compatibility.

All four should be identifiable. A header alone cannot establish protobuf or
storage compatibility. Support additions without changing old struct layouts:
version/size fields for options, reserved fields, explicit flags, and refusal of
unknown behavior-changing options. Test older headers/clients against new binaries.

Generate or validate the public C header with cbindgen and compile C and C++ smoke
clients against it. Keep an export allowlist: the probe also exported a Blake3
symbol that is not part of Atomic's API. Platform linker controls should prevent
dependency/internal symbols from becoming accidental ABI commitments.

### 6. Local trust and contract enforcement

The initial local API is for trusted callers with the application's filesystem
rights. It is not an isolation boundary against malicious code in the same
address space. Do not add a network listener or silently treat restricted sandbox
credentials as unrestricted local access.

The RPC contract also specifies request-ID replay, effective-view fences,
authorization, read effects, workspace leases and cancellation. Several handlers
currently return snapshots/meta as None or rely on middleware. A C ABI wrapper
cannot make those semantics complete merely by preserving wire types.

There is also an existing read-effect discrepancy: `Pristine::open_readonly`
can briefly open a writable database for UpgradeRequired/RepairAborted recovery
(`atomic-core/src/pristine/txn/pristine.rs`). The RPC contract prohibits lazy
repair in declared reads. An embedded client needs an explicit documented
open/recovery policy; a C wrapper must not advertise stricter read purity than
the implementation actually provides.

Document the supported local semantics and validate advertised methods. Completion
of remote/middleware/domain requirements is a separate workstream from freezing
six C function signatures.

### 7. Streaming and cancellation are not unary dispatch

A single response buffer is sufficient for status, but not an unbounded stream.
Later expose stream handles with explicit next/read, end-of-stream, error, cancel
and release behavior; retain canonical protobuf chunk types. Pull-based streams
avoid calling arbitrary host callbacks from Rust-owned threads and fit Java/Go/Node
more naturally. Do not silently aggregate unbounded streams into one buffer.

Diff currently creates a spawned producer. Materialize collects frames under a
repository handle, then releases it before returning its stream. Stream handles
must keep the runtime alive, clean up producers, enforce backpressure/size limits,
and not let a slow host retain a database writer indefinitely.

Some handlers hold an async repository guard outside spawn_blocking. Cancellation
can drop the guard while the blocking task continues. Compaction already uses an
owned guard moved into the blocking task. Audit this pattern before adding native
cancellation or timeouts; a timed-out call must not be interpreted as proof that
no mutation committed. Keep existing request-ID/recovery semantics rather than
inventing transparent retries.

V1 can omit cancellation and streaming explicitly. Language-side promise timeouts
must not free native state or input/output storage while a C call still runs.

## Distribution and CI work

Current release automation packages the CLI only. It already has useful targets:
Linux x64/ARM64 glibc and musl, macOS x64/ARM64, Windows x64, with a glibc 2.28 floor.
This is a starting point, not proof the shared libraries load on those platforms.

Required additions:

- Build C ABI artifacts without Node-API, using the embedding-safe release profile.
- Package the same native artifact plus C header, ABI/build/schema information,
  checksums, licenses and release notes. No crates.io publication is required to
  distribute a compiled C library.
- macOS: portable install name (`@rpath/...`), architecture validation and signing
  policy. The probe's debug install name is an absolute development path.
- Linux: inspect .so dependencies, loader paths and glibc symbol versions. The
  Linux libatomic dependency graph includes native-tls/OpenSSL, as well as rustls;
  choose and verify vendoring/system/TLS dependency policy. Do not assume a static
  musl CLI implies a shared library usable in every glibc or musl host.
- Windows: exports/import library and runtime dependency policy; test actual DLL
  loading, not only cross-compilation.
- Test a small C client and at least the Node wrapper on native runners. Gate new
  releases on older-client compatibility and symbol/header consistency.

One **Atomic** library is built per platform, reused by all language packages.
Node's Koffi and older Java JNA may have their own generic native components; those
are not separately compiled Atomic implementations. Go cgo still requires a C
compiler when building Go applications unless another FFI route is chosen.

## Language binding effort

### Node

Retain the TypeScript API and generated protobuf codecs. Replace the current
Node-API loading with Koffi calls to the common C library, use .async so long
calls don't block the event loop, copy returned bytes and release them in finally,
and explicitly retain library/context lifetime. This was proven in the temporary
probe, not installed into nodeatomic.

### Java

JDK 22's Foreign Function & Memory API can invoke the common C ABI without a custom
JNI Atomic library. Use explicit memory layouts and arenas, copy result bytes
before release, configure native access, and run blocking calls on an executor.
Older JDK support through JNA is a separate compatibility choice. Not executed here.

### Go

Use cgo against the same header/library and generated Go protobuf codecs. Do not
retain Go pointers after a call unless correctly pinned; copying/decoding request
bytes before work outlives a call avoids that requirement. Copy results into Go
memory and call libatomic's release function. Dynamic linker configuration and
cgo cross-compilation need tests. Not executed here.

## Required acceptance tests

1. ABI/header/version/layout and symbol allowlist on each target.
2. C/C++/Node round trips; minimum Java/Go smoke tests before claiming those SDKs.
3. Empty/null/invalid UTF-8/malformed/oversized protobuf requests and unsupported methods.
4. Missing required fields without host termination; stable error details.
5. Large outputs, partial failures, and exact allocation/free ownership.
6. Repeated open/call/close, call/close races, concurrent clients and threads.
7. Host event loop continues while a call blocks; library runtime needs no host executor.
8. Injected panics in boundary/async/blocking tasks in the **release ABI profile**.
9. redb contention with CLI and another embedding process; bounded failures and no corruption.
10. Database handles released after calls; ignored artifacts excluded from status.
11. Older ABI clients against a newer library, schema drift and unsupported capabilities.
12. Stream/cancellation and commit/replay tests before adding those capabilities.

C sanitizer tests and Rust-side ownership tests should supplement functional
checks. The disposable probe did not provide leak, fuzz, sanitizer or malicious
pointer safety coverage.

## Suggested work plan

| Stable two-method v1 workstream | Engineer-days |
|---|---:|
| Freeze ownership/lifecycle/version/error decisions and header | 1–2 |
| C exports, runtime/handle management, panic/validation/error boundary | 3–4 |
| Ownership, contention, concurrency, invalid-input, panic and compatibility tests | 3–4 |
| Initial shared-library packaging and CI/loader verification | 2–3 |
| Convert Node consumer to the common C ABI and rerun its tests | 1–2 |
| **Total, including the first local prototype** | **10–15** |

A narrow functional local build can precede most hardening in 3–5 days. Do not
call that intermediate result stable. Decide a bounded initial platform set
before interpreting this estimate; all seven existing CLI targets may stretch it.

For broader service coverage, generate only adapters for explicitly supported
handlers and preserve their existing request/response types. A descriptor-driven
registry reduces repetitive code, but each newly exposed domain needs validation
and coverage. Missing/stubbed methods and protocol guarantees should not be hidden
inside an optimistic FFI estimate.

## Recommendation

Proceed with a **trusted-local, unary-only ABI v1**, initially ResolveRepository
and Status, and one Node consumer using the common library. Keep redb and the
single repository database unchanged. Establish ownership and a panic-safe release
profile before committing to signatures. Validate with C plus Java/Go smoke tests,
then add unary methods incrementally. Add streaming/cancellation only when their
lifecycle contracts and existing handler behavior have been audited.

This is meaningfully less work than writing three independent bindings to all of
Atomic's Rust types. It is more work than a 49-line forwarding shim because
"stable" commits us to memory, error, concurrency and compatibility behavior.

## External references

- Rust Nomicon, FFI and unwinding: https://doc.rust-lang.org/nomicon/ffi.html
- Rust reference, function ABIs: https://doc.rust-lang.org/reference/items/functions.html
- Koffi loading/calling and async calls: https://koffi.dev/load
- Koffi output ownership: https://koffi.dev/output
- Go cgo and pointer restrictions: https://pkg.go.dev/cmd/cgo
- Java FFM: https://docs.oracle.com/en/java/javase/22/core/foreign-function-and-memory-api.html
