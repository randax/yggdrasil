# Depot CI + Release Builds Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move CI onto Depot-managed GitHub Actions runners and add a tag-triggered release workflow that publishes `yg` binaries for Linux x86_64/arm64 and macOS Apple Silicon.

**Architecture:** Two GitHub Actions workflows. `ci.yml` changes only its runner label. A new `release.yml` runs on `v*` tags: a guard job checks the tag against the workspace version, a three-target matrix builds natively on Depot runners (no cross-compilation), and a final job attaches tarballs + checksums to a GitHub Release.

**Tech Stack:** GitHub Actions, Depot runners (`depot-ubuntu-24.04`, `depot-ubuntu-24.04-arm`, `depot-macos-latest`), `actions-rust-lang/setup-rust-toolchain`, `softprops/action-gh-release`.

**Spec:** `docs/superpowers/specs/2026-07-10-depot-ci-release-builds-design.md`

## Global Constraints

- All third-party actions are pinned to a full commit SHA with a `# vX.Y.Z` comment, matching the existing style in `.github/workflows/ci.yml`.
- The release binary is `yg`, built from package `yg-cli` (`crates/yg-cli/Cargo.toml` sets `[[bin]] name = "yg"`).
- Builds are native per runner — never add `--target` or cross-compilation.
- `ci.yml` keeps `permissions: contents: read`; only `release.yml` gets `contents: write`.
- Workflow branch: `ci-depot-release-builds` (already exists, contains the spec commit).
- Commit messages: Conventional Commits with a scope, e.g. `feat(ci): ...` (project rule; no unscoped subjects).
- **External prerequisite (manual, user-side):** the Depot GitHub app must be installed on `randax/yggdrasil` with GitHub Actions runners enabled in the Depot org. If jobs sit queued > ~2 minutes, this is missing — stop and report to the user rather than debugging YAML.

---

### Task 1: Open the draft PR

Project rule: a draft PR whose description contains the plan must exist before implementation work begins.

**Files:**
- None modified (git/gh operations only).

**Interfaces:**
- Produces: an open draft PR from `ci-depot-release-builds` to `main`; later tasks push commits to it and Task 4 finalizes it.

- [ ] **Step 1: Commit the plan document**

```bash
cd /Users/oyr/projects/yggdrasil
git add docs/superpowers/plans/2026-07-10-depot-ci-release-builds.md
git commit -m "docs(ci): add implementation plan for depot runners and release builds"
```

- [ ] **Step 2: Push the branch**

```bash
git push -u origin ci-depot-release-builds
```

- [ ] **Step 3: Open the draft PR with the plan as its body**

Write the body to a temp file first so it contains real newlines (project rule: never escaped `\n`):

```bash
cat > /private/tmp/claude-501/-Users-oyr-projects-yggdrasil/30b935d6-9883-49b2-9e0b-a0b46c8560a5/scratchpad/pr-body.md <<'EOF'
## Plan

Move CI onto Depot GitHub Actions runners and add a tag-triggered release workflow.

1. `ci.yml`: swap `runs-on: ubuntu-latest` → `depot-ubuntu-24.04`; nothing else changes.
2. New `release.yml` on `v*` tags:
   - guard job fails if the tag ≠ `workspace.package.version` in `Cargo.toml`
   - matrix builds `yg` natively for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin` on Depot runners
   - final job attaches `yg-<version>-<target>.tar.gz` + `.sha256` files to a GitHub Release with generated notes

Design: `docs/superpowers/specs/2026-07-10-depot-ci-release-builds-design.md`

**Prerequisite:** Depot GitHub app installed on this repo with Actions runners enabled.
EOF
gh pr create --draft \
  --title "feat(ci): depot runners and tag-triggered release builds" \
  --body-file /private/tmp/claude-501/-Users-oyr-projects-yggdrasil/30b935d6-9883-49b2-9e0b-a0b46c8560a5/scratchpad/pr-body.md
```

- [ ] **Step 4: Verify the rendered body**

```bash
gh pr view --json title,body,isDraft
```

Expected: `isDraft: true`, body shows real Markdown (numbered list, no literal `\n`).

---

### Task 2: Switch CI to Depot runners

**Files:**
- Modify: `.github/workflows/ci.yml` (the `runs-on` line of the single `ci` job)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: CI job running on `depot-ubuntu-24.04`; Task 4 verifies it goes green.

- [ ] **Step 1: Edit the runner label**

In `.github/workflows/ci.yml`, change:

```yaml
    runs-on: ubuntu-latest
```

to:

```yaml
    runs-on: depot-ubuntu-24.04
```

No other changes — the docker compose steps and pinned actions work unchanged on Depot Ubuntu runners.

- [ ] **Step 2: Lint the workflow**

```bash
command -v actionlint >/dev/null || brew install actionlint
actionlint /Users/oyr/projects/yggdrasil/.github/workflows/ci.yml
```

Expected: no output (exit 0). Note: actionlint may warn it doesn't recognize the `depot-ubuntu-24.04` label; that specific warning is expected and acceptable — any other finding is a real error.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "feat(ci): run ci on depot runners"
```

---

### Task 3: Add the release workflow

**Files:**
- Create: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: a workflow triggered by `v*` tags; Task 4 pushes it and verifies syntax via CI (full end-to-end run happens when the user pushes a real tag).

- [ ] **Step 1: Create `.github/workflows/release.yml` with exactly this content**

```yaml
name: Release

on:
  push:
    tags: ["v*"]

# Creates the GitHub Release and uploads assets.
permissions:
  contents: write

env:
  CARGO_TERM_COLOR: always

jobs:
  guard:
    name: tag matches workspace version
    runs-on: depot-ubuntu-24.04
    steps:
      - uses: actions/checkout@93cb6efe18208431cddfb8368fd83d5badbf9bfd # v5
      - name: Check tag against Cargo.toml
        run: |
          tag_version="${GITHUB_REF_NAME#v}"
          cargo_version="$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)"
          if [ "$tag_version" != "$cargo_version" ]; then
            echo "::error::Tag ${GITHUB_REF_NAME} does not match workspace version ${cargo_version}"
            exit 1
          fi

  build:
    name: build ${{ matrix.target }}
    needs: guard
    strategy:
      matrix:
        include:
          - target: x86_64-unknown-linux-gnu
            runner: depot-ubuntu-24.04
          - target: aarch64-unknown-linux-gnu
            runner: depot-ubuntu-24.04-arm
          - target: aarch64-apple-darwin
            runner: depot-macos-latest
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@93cb6efe18208431cddfb8368fd83d5badbf9bfd # v5
      # Reads rust-toolchain.toml; rustflags kept empty to match CI.
      - uses: actions-rust-lang/setup-rust-toolchain@46268bd060767258de96ed93c1251119784f2ab6 # v1.16.1
        with:
          rustflags: ""
      # Each matrix leg runs on hardware native to its target; no --target needed.
      - run: cargo build --release --locked -p yg-cli
      - name: Package tarball and checksum
        run: |
          version="${GITHUB_REF_NAME#v}"
          archive="yg-${version}-${{ matrix.target }}.tar.gz"
          tar -czf "${archive}" -C target/release yg
          shasum -a 256 "${archive}" > "${archive}.sha256"
      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1
        with:
          name: yg-${{ matrix.target }}
          path: |
            yg-*.tar.gz
            yg-*.tar.gz.sha256
          if-no-files-found: error

  release:
    name: create github release
    needs: build
    runs-on: depot-ubuntu-24.04
    steps:
      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1
        with:
          path: dist
          merge-multiple: true
      - uses: softprops/action-gh-release@718ea10b132b3b2eba29c1007bb80653f286566b # v3.0.1
        with:
          files: dist/*
          generate_release_notes: true
```

- [ ] **Step 2: Lint the workflow**

```bash
actionlint /Users/oyr/projects/yggdrasil/.github/workflows/release.yml
```

Expected: exit 0 (same caveat as Task 2 about unknown Depot runner labels).

- [ ] **Step 3: Sanity-check the version-guard sed expression locally**

```bash
cd /Users/oyr/projects/yggdrasil
sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml
```

Expected output: `0.1.0` (exactly one line). If empty or multiple lines, fix the expression before committing.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "feat(ci): add tag-triggered release builds for linux and macos"
```

---

### Task 4: Push, finalize PR, and watch CI

**Files:**
- None modified (git/gh operations only).

**Interfaces:**
- Consumes: the draft PR from Task 1 and the commits from Tasks 2–3.

- [ ] **Step 1: Push**

```bash
git push
```

- [ ] **Step 2: Watch CI on the PR**

```bash
gh pr checks --watch
```

Expected: the `fmt + clippy + test` check passes **on a Depot runner**. Confirm the runner with:

```bash
gh run view --job "$(gh run list --branch ci-depot-release-builds --limit 1 --json databaseId --jq '.[0].databaseId')" 2>/dev/null || gh run list --branch ci-depot-release-builds --limit 1
```

If the job sits **queued** for more than ~2 minutes, the Depot GitHub app / runner enablement is missing — stop and report to the user (external prerequisite; not a YAML bug). If it fails, fix and repeat; the job doesn't stop until checks are green.

- [ ] **Step 3: Update the PR to describe completed work and remove draft status**

Rewrite the body (real newlines, via `--body-file` as in Task 1) so it describes the finished change: CI on Depot runners, the new release workflow's shape, the manual prerequisite, and how to cut a release (`git tag v0.1.0 && git push origin v0.1.0`). Then:

```bash
gh pr ready
gh pr view --json title,body,isDraft
```

Expected: `isDraft: false`, body renders correctly.

---

## Verification notes

- The release workflow can only be exercised end-to-end by pushing a real `v*` tag; that is deliberately left to the user after merge (spec: "push a test tag once the Depot org prerequisite is confirmed").
- Per project rules, after implementation run adversarial review personas over the diff and fix anything real before finalizing the PR.
