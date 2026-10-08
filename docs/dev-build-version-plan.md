# Commit identity in development builds

Status: implemented on `feat/dev-build-identity`.

## Goal

Make development binaries identifiable from `atomic --version`, whether built
locally or distributed as development artifacts. Keep the existing package
versioning and release sequence.

## Version output

```text
Official release:  atomic 0.19.1
Development build: atomic 0.19.1 (dev 4b143dfbcad2)
```

The development identifier is the first 12 characters of the source Git commit.
It is display metadata, not a change to the Cargo package version.

## Build policy

- Default to a development build for local Cargo builds.
- Set `ATOMIC_BUILD_CHANNEL=release` in the existing release workflow to retain
  the plain release version.
- Future development publication uses `ATOMIC_BUILD_CHANNEL=dev` (the default)
  and therefore includes the commit identity.
- Select the channel explicitly at build time, rather than guessing from branch
  names or tags. Release builds currently happen before the version tag is
  created, and CI often checks out a detached commit.
- A locally built checkout of a release tag is still a source/development build
  by default. Building with the release channel explicitly selects plain output.

## Implementation: two small changes

1. **CLI build metadata**
   - Add `atomic-cli/build.rs` using Rust's standard library.
   - Read `CARGO_PKG_VERSION` and the build channel.
   - For development builds, resolve Git `HEAD` and embed its short hash into
     the CLI version string. Permit `ATOMIC_BUILD_COMMIT` to supply the source
     commit when a build pipeline uses a source archive without Git metadata.
   - Append `-dirty` when tracked source changes are present, so local edits
     are not presented as an exact clean commit. Do not count untracked files.
   - If source identity is unavailable, display `(dev unknown)` rather than
     failing an otherwise valid local build or pretending it is a release.
   - Ensure Cargo refreshes the metadata when the channel, supplied commit,
     source state, or Git HEAD/ref changes, including Git worktrees.
   - Wire the embedded string into Clap's version field in
     `atomic-cli/src/main.rs`, so both `--version` and `-V` show it.

2. **Release workflow**
   - Add `ATOMIC_BUILD_CHANNEL: release` to the build environment in
     `.github/workflows/release.yml`.
   - All current release entry points use that workflow, so they get the same
     plain version output.

The identity is compiled into the executable. Running a downloaded development
binary does not require Git or a checkout on the user's machine.

## Verification

- A clean local build shows the current commit; tracked edits add `-dirty`.
- Changing commits and rebuilding refreshes the displayed hash.
- Release-channel builds report only the existing package version.
- A development artifact built in CI retains its hash when run elsewhere.
- Archive builds accept an explicit commit or show `dev unknown`.
