# Depot CI + Release Builds — Design

Date: 2026-07-10
Status: approved

## Goal

Run CI on Depot-managed GitHub Actions runners, and add a tag-triggered
release workflow that publishes prebuilt `yg` binaries for Linux
(x86_64, arm64) and macOS (Apple Silicon) as GitHub Releases.

## Out of scope

- Docker image / GHCR publishing (no Dockerfile exists yet).
- Windows and Intel-macOS binaries.
- Automated release tooling (release-plz, cargo-dist); releases are cut by
  pushing a tag manually.

## 1. CI on Depot runners

`.github/workflows/ci.yml` changes only the runner:
`runs-on: ubuntu-latest` → `runs-on: depot-ubuntu-24.04`.
All steps (fmt, clippy `-D warnings`, doc, docker compose Postgres/MinIO,
tests) are unchanged; Depot Ubuntu runners ship Docker and are drop-in
compatible.

**One-time prerequisite (manual):** the Depot GitHub app must be installed
on `randax/yggdrasil` with GitHub Actions runners enabled in the Depot org.
Until then, jobs queue indefinitely.

## 2. Release workflow

New file `.github/workflows/release.yml`.

- **Trigger:** push of tags matching `v*`.
- **Permissions:** `contents: write` (creates the release). CI keeps
  `contents: read`.
- **Guard job:** fail fast if the tag (minus the `v` prefix) does not equal
  `workspace.package.version` in `Cargo.toml`.
- **Build job**, matrix of three targets, each built natively on its own
  Depot runner (no cross-compilation):

  | target | runner |
  |---|---|
  | `x86_64-unknown-linux-gnu` | `depot-ubuntu-24.04` |
  | `aarch64-unknown-linux-gnu` | `depot-ubuntu-24.04-arm` |
  | `aarch64-apple-darwin` | `depot-macos-latest` |

  Steps per target: checkout, toolchain from `rust-toolchain.toml`
  (`actions-rust-lang/setup-rust-toolchain`, `rustflags: ""` as in CI),
  `cargo build --release --locked -p yg-cli`, package
  `yg-<version>-<target>.tar.gz` containing the `yg` binary, write a
  matching `.sha256` file, upload both as a workflow artifact.
- **Release job:** after all builds, download artifacts and create the
  GitHub Release with `softprops/action-gh-release` (SHA-pinned, matching
  the repo's action-pinning style), attaching all tarballs and checksums,
  with auto-generated release notes.

## Decisions

- **gnu, not musl:** Linux binaries target glibc (require glibc ≥ 2.39 on
  the host). Fully static musl builds are a possible follow-up if broader
  host compatibility is needed.
- **Apple Silicon only for macOS:** Depot macOS runners are arm64-only;
  Intel macOS is deliberately skipped.
- **Hand-written matrix over cargo-dist / third-party build actions:** the
  workflow stays small, transparent, and fully controlled; only
  `action-gh-release` is added as a pinned dependency.

## Verification

- `actionlint` on both workflow files.
- Real end-to-end check: push a test tag (e.g. `v0.1.0`) once the Depot
  org prerequisite is confirmed, and watch the release appear with all six
  files (3 tarballs + 3 checksums).

## Error handling

- Tag/version mismatch → guard job fails with an explicit message; no
  partial release is created.
- Any target failing → release job is skipped entirely (default `needs`
  semantics); fix and re-tag.
