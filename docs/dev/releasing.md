# Releasing

*Release process, tagging, and Docker builds*

How to cut a new release. Three workflows handle the full lifecycle so the
operator's job is "click Run, review the PR, merge."

```text
┌──────────────────────────┐  workflow_dispatch
│ prepare-release.yml      │ ───────────────────► PR titled
│   bump Cargo.toml        │                      `chore(release): vX.Y.Z`
│   roll CHANGELOG         │
│   open PR                │
└──────────────────────────┘
                                 reviewer merges PR
                                 ▼
┌──────────────────────────┐
│ tag-on-merge.yml         │
│   read PR title          │ ───────────────────► tag `vX.Y.Z` pushed
│   tag merge commit       │
│   push tag               │
└──────────────────────────┘
                                 tag push fires
                                 ▼
┌──────────────────────────┐
│ release.yml              │
│   CI gate                │
│   stamp Cargo.toml       │
│   4-target binaries      │
│   multi-arch Docker      │
│   GitHub Release         │ ───────────────────► artifacts on
│   SBOM + attestation     │                      DockerHub & GH Releases
│   sync DOCKERHUB.md      │
└──────────────────────────┘
```

## Prerequisites

- Push access to `main` branch (only required to merge the prepare-release PR).
- All CI checks passing on `main` before kicking off a release.
- DockerHub secrets configured in GitHub repo settings (`DOCKERHUB_USERNAME`,
  `DOCKERHUB_TOKEN`). The token needs Read & Write scope on the repo so the
  description-sync step can PATCH `DOCKERHUB.md` into the hub UI.

## Quick path (recommended)

### 1. Run `Prepare Release`

GitHub → Actions → **Prepare Release** → Run workflow.

Pick a `bump`:
- `patch` — bug fixes only
- `minor` — new features, new metrics, new config options
- `major` — breaking S3 API or storage-format changes
- explicit `X.Y.Z` — when you want a specific version (e.g. `1.0.0`)

The workflow runs [`scripts/release-prep.sh`](../../scripts/release-prep.sh)
which:
- Bumps `Cargo.toml` and refreshes `Cargo.lock`.
- Renames `## Unreleased` in `CHANGELOG.md` to `## vX.Y.Z — YYYY-MM-DD` and
  inserts a fresh empty `## Unreleased` block above it.
- Opens a PR titled `chore(release): vX.Y.Z`.

### 2. Review the PR

This is the human checkpoint. Read the diff, edit the CHANGELOG section under
`## vX.Y.Z` if the auto-rolled content needs polish — e.g. promote a buried
fix into the headline, drop noise, add a "Breaking changes" section.

If you need to update other doc files for this release (`docs/product/...`,
`README.md`, `CLAUDE.md`), push the edits onto the same `release/vX.Y.Z`
branch — they ship as part of the release commit.

CI runs against the PR. Wait for green before merging.

### 3. Merge

Standard merge into `main`. `tag-on-merge.yml` picks it up:
- Parses `vX.Y.Z` from the PR title.
- Cross-checks against `Cargo.toml` (refuses if they disagree — defends
  against a hand-edited PR that drifted).
- Tags the merge commit with `vX.Y.Z`.
- Pushes the tag.

### 4. `release.yml` runs

The tag push fires the existing release pipeline (5 stages, ~50–70 min
total). Monitor with `gh run list --limit 1` and `gh run watch`.

When done:
- GitHub Release exists with binaries + checksums + SBOM.
- DockerHub has `:X.Y.Z`, `:X.Y`, `:X`, `:latest`.
- DockerHub repo description matches `DOCKERHUB.md` in the tagged commit.
- Build provenance attestation published.

### 5. Verify

```bash
gh release view vX.Y.Z
docker manifest inspect beshultd/deltaglider_proxy:X.Y.Z
docker pull beshultd/deltaglider_proxy:X.Y.Z
```

Smoke-test the new image (`docker run --rm -p 9000:9000 ...`, hit `/_/health`).

## Manual fallback

If for any reason the automated flow can't run (workflow disabled, CI broken
in a way that blocks `prepare-release` itself), you can do everything by hand:

1. On `main`, run the script locally:

   ```bash
   scripts/release-prep.sh patch   # or minor / major / X.Y.Z
   git checkout -b release/vX.Y.Z
   git add Cargo.toml Cargo.lock CHANGELOG.md
   git commit -m "chore(release): vX.Y.Z"
   git push origin release/vX.Y.Z
   gh pr create --base main --title "chore(release): vX.Y.Z" \
       --body "manual fallback prepare"
   ```

2. Merge the PR. `tag-on-merge.yml` still runs, so the rest is automated
   from there.

3. If `tag-on-merge.yml` is also broken, tag the merge commit by hand:

   ```bash
   git checkout main
   git pull
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

   The tag push fires `release.yml` exactly the same way.

## What the pipeline does automatically

You do NOT need to do any of this manually:

| Step | Automated by |
|------|--------------|
| Version bump in `Cargo.toml`/`Cargo.lock` on `main` | [`.github/workflows/prepare-release.yml`](../../.github/workflows/prepare-release.yml) via [`scripts/release-prep.sh`](../../scripts/release-prep.sh) |
| `## Unreleased` → `## vX.Y.Z — date` rewrite in CHANGELOG | [`scripts/release-prep.sh`](../../scripts/release-prep.sh) |
| Open release PR with diff for review | [`.github/workflows/prepare-release.yml`](../../.github/workflows/prepare-release.yml) |
| Tag the merge commit and push the tag | [`.github/workflows/tag-on-merge.yml`](../../.github/workflows/tag-on-merge.yml) |
| Version stamping in `Cargo.toml` for release artifacts | [`.github/workflows/release.yml`](../../.github/workflows/release.yml) — extracts from git tag, `sed` into Cargo.toml inside CI |
| UI build (`npm ci && npm run build`) | [`.github/workflows/release.yml`](../../.github/workflows/release.yml) — runs in each build job |
| Binary compilation (4 targets) | [`.github/workflows/release.yml`](../../.github/workflows/release.yml) — Linux x86/ARM, macOS Intel/ARM |
| Binary stripping | [`.github/workflows/release.yml`](../../.github/workflows/release.yml) — `strip` on each binary |
| Docker multi-arch build + push | [`.github/workflows/release.yml`](../../.github/workflows/release.yml) — native `amd64`/`arm64` builds plus manifest merge |
| Docker tags (semver cascade) | [`.github/workflows/release.yml`](../../.github/workflows/release.yml) — `X.Y.Z`, `X.Y`, `X`, `latest` |
| SHA256 checksums | [`.github/workflows/release.yml`](../../.github/workflows/release.yml) — `sha256sum` on all archives |
| SBOM generation | [`.github/workflows/release.yml`](../../.github/workflows/release.yml) — `cargo sbom` (SPDX JSON) |
| Build provenance attestation | [`.github/workflows/release.yml`](../../.github/workflows/release.yml) — GitHub attestation action |
| GitHub Release creation | [`.github/workflows/release.yml`](../../.github/workflows/release.yml) — with auto-generated release notes |
| DockerHub repo description | [`.github/workflows/release.yml`](../../.github/workflows/release.yml) — PATCH `DOCKERHUB.md` via the v2 API |

## Version numbering

We use [semver](https://semver.org/):

- **MAJOR** (`X.0.0`): Breaking S3 API changes, storage format changes
  requiring migration.
- **MINOR** (`0.X.0`): New features, new metrics, new config options,
  significant bug fixes.
- **PATCH** (`0.0.X`): Bug fixes, documentation, performance improvements.

`Cargo.toml` on `main` and the latest tag stay in sync because the bump lands
in the prepare-release PR BEFORE the tag is created. That eliminates the old
"sync version after release" bookkeeping commit.

## Troubleshooting

### Prepare-release PR opens but CHANGELOG section is empty / wrong

You forgot to land any `## Unreleased` entries between releases. The script
moves whatever is under `## Unreleased` into the dated section verbatim, so an
empty Unreleased gives an empty release section. Fix it on the PR branch
(edit CHANGELOG, push) before merging — CI re-runs and the PR keeps working.

### `tag-on-merge.yml` skipped after merge

The PR title has to match `chore(release): vX.Y.Z` exactly. If you renamed
the PR mid-review, the regex check fails and tagging is skipped (silently —
intentional, to avoid auto-tagging unrelated PRs). Tag manually:

```bash
git checkout main && git pull
git tag vX.Y.Z
git push origin vX.Y.Z
```

### `tag-on-merge.yml` refuses with "Cargo.toml says X but PR title says Y"

Someone edited `Cargo.toml` on the PR branch without keeping the title in
sync, OR rebased the branch onto a `Cargo.toml` change that hadn't accounted
for the bump. Fix the inconsistency on the merged commit:

```bash
git checkout main && git pull
# Hand-edit Cargo.toml to the right version, regenerate Cargo.lock
git commit -am "chore(release): align Cargo.toml to vX.Y.Z"
git tag vX.Y.Z && git push origin vX.Y.Z
```

### CI Gate (inside `release.yml`) fails

Fix the issue on `main`, push, then move the tag forward:

```bash
git push origin main
git tag -d vX.Y.Z
git push origin :refs/tags/vX.Y.Z
git tag vX.Y.Z
git push origin vX.Y.Z
```

### Docker build times out

The multi-arch Docker build can take 30–55 minutes. This is normal. If it
fails:

1. Check the job logs for OOM or disk space issues.
2. Re-run the failed job from the GitHub Actions UI.

### Attestation hangs

The `attest-build-provenance` step contacts Sigstore's infrastructure. If it
hangs on a self-hosted runner, it may be a network issue. The Docker images
and binaries are already published at this point — the release is usable even
if attestation fails.

### DockerHub description sync fails

Doesn't fail the release (`continue-on-error: true`). Causes:

- `DOCKERHUB_TOKEN` doesn't have Read & Write scope on the repo. Rotate the
  token in the secrets store.
- DockerHub auth API is down. The release is fine; sync the description by
  hand: hub.docker.com → repo → "Manage repository" → paste `DOCKERHUB.md`.

### Wrong version tagged

```bash
# Delete remote tag
git push origin :refs/tags/vX.Y.Z
# Delete local tag
git tag -d vX.Y.Z
# Re-trigger via prepare-release.yml with the correct version,
# OR tag manually at the right commit:
git tag vX.Y.Z <correct-commit-sha>
git push origin vX.Y.Z
```
