# Database locking harness — issue #264

From the repository root:

```bash
bash tests/harness/46_database_locking.sh
```

This runs the snapshot tests, same-process and cross-process lock-exclusion
matrix, bounded lock-wait tests, and provenance checkpoint tests. It builds
the tests from the current checkout with the committed dependency resolution
(`cargo --locked`); it does not use an installed `atomic` binary.

To run the broader storage validation and service compile check:

```bash
bash tests/harness/46_database_locking.sh --full
```

The current full set contains 307 pristine unit tests and 35 integration tests,
plus `cargo check -p libatomic`. Counts can change as tests are added.

The numbered script is also discovered by `tests/harness/run_all.sh 46`, which
runs the default set. The existing CLI runner may first build or select a CLI
binary; direct invocation avoids that unnecessary step for this Rust suite.

Each invocation retains its environment, resolved core dependencies, complete
check output, and a machine-readable `summary.tsv` under a unique
`target/locking-harness/run.*` directory. Set `ATOMIC_LOCK_HARNESS_LOG_DIR` to
choose a different log parent. A failed check makes the harness exit nonzero;
remaining checks still run so their evidence is retained.

## Contracts exercised

- Wrapped repository writers exclude another wrapped writer, a native
  provenance-journal writer, and a read-only handle.
- Native journal writers and read-only holders exclude wrapped writers.
- The same combinations work within one process and across independent
  processes. Compatible read-only opens remain allowed.
- Refused opens and dropped backend clones do not release the live owner's
  lock; ending the owner releases it. Failed database initialization releases
  its own locks even when a backend clone remains alive.
- Snapshot copies preserve the source's exclusion and are independently
  readable. Serialized writes through wrapped and native handles survive
  reopening, including differently sized payloads.
- Lock waits are bounded, and provenance checkpoints continue to work.
- Full mode also covers database consolidation, legacy migration, schema
  guards, and the service's compatibility with the resolved redb version.

All test databases are temporary fixtures. The harness neither repairs nor
opens an existing user repository. A failure before the fix is expected when
the dependency resolves to redb 4.3/4.4: independent opens incorrectly succeed.
Passing these tests prevents the reproduced locking defect; it does not prove
the cause of a historical corruption incident or recover damaged data.
