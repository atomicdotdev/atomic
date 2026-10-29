# Native delegation evaluation

Run four native process-level evaluations and retain the original output,
test population, test executable, source commit and SHA-256 artifact manifest:

```sh
python3 tools/native-evaluation/run.py --output /tmp/atomic-delegation-run
```

The output directory must be new. The runner uses Python's standard library and
the repository's locked Rust dependencies. Set `CARGO_TARGET_DIR` as usual to
reuse a build cache. Each case runs the actual `atomic-canonical` and
`atomic-identity` public APIs; grant lookup happens in a fresh OS process with a
persisted identity store. It never touches the operator's installed identities.

The declared cases cover:

- Repeating an exact certificate write and restarting the reader preserves one
  certificate and its narrow permission, project, server and view scope.
- Local revocation remains effective after restart, while a distinct renewed
  certificate remains selectable.
- Expired, altered and ambiguous certificates cannot become active; adding a
  valid certificate restores a positive control without hiding corrupt files.
- Local self-contained selection accepts an internally valid certificate, but
  verification against an unrelated externally supplied issuer key fails. A
  grant for another subject cannot replace the selected agent's grant.

These are **local certificate selection and persistence** results. They do not
establish server permission enforcement, remote revocation, crash consistency
during a write, model behavior or external effect custody. Separate processes
still share one operator's clock, keys and filesystem. `report.json` retains
these limitations alongside results; an exit code alone is insufficient evidence.

The tests also run in the existing `cargo test --workspace` CI matrix. Ubuntu CI
retains a complete evaluation bundle using the same runner. A build failure or
timeout produces a failed report and preserved logs; it cannot count as a passed
native run. The helper test is ignored by the normal test runner and invoked only
by process-level cases.

## Installed offline consumer

The separate `Native delegation consumer` workflow produces a fresh packet from
the actual Atomic checkout, builds Observer's wheel from immutable reviewed
source `16d31f6ae1874389a6d5dc56e54997b88a7e2397`, and installs it in a clean
environment. The host selector checks the checkout commit, exact source/lockfile
bytes and pinned installed reader before accepting the candidate selection.
Selection policy and receipts remain outside the producer packet. The reader
does not execute the retained binary or rerun Atomic.

`check_consumer.py --help` lists explicit packet, source, wheel, installed-reader
and output arguments. `consumer-policy.json` pins the reviewed reader outside
the packet. The selector invokes the reader's documented `describe` and `verify`
commands, including a separately pinned selection digest. It retains positive
verification and refusal controls for changed raw logs, a newly pinned wider
selection and an incorrect selection digest.

This is a runnable proposed Atomic-host consumer integration. A local success
does not establish maintainer acceptance, completed upstream CI, recurring
outside adoption or independent operation. The source test population and local
key/clock/store limitations above continue to apply.
