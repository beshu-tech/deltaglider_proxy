# Releasing DeltaGlider Proxy

How to cut a new release. The entire build/publish pipeline is automated — your job is to prepare the code, tag it, and verify.

## Prerequisites

- Push access to `main` branch
- All CI checks passing on `main` (fmt, clippy, tests, audit)
- DockerHub secrets configured in GitHub repo settings (`DOCKERHUB_USERNAME`, `DOCKERHUB_TOKEN`)

## Step-by-step

### 1. Prepare the release on `main`

Ensure all changes are merged and CI is green:

```bash
git checkout main
git pull origin main

# Verify CI passed on latest commit
gh run list --limit 1
```

### 2. Update documentation

Before tagging, update these files to reflect the new version's changes:

| File | What to update |
|------|----------------|
| `CHANGELOG.md` | Add a new `## vX.Y.Z` section at the top with all changes |
| `docs/METRICS.md` | Any new/changed Prometheus metrics |
| `docs/OPERATIONS.md` | New operational features, config options, observability |
| `README.md` | Feature highlights, architecture changes |
| `CLAUDE.md` | New types, modules, routing, key implementation details |
| `docs/AUTHENTICATION.md` | Auth changes (if any) |
| `docs/STORAGE_FORMAT.md` | Storage layout changes (if any) |

Commit the doc updates:

```bash
git add CHANGELOG.md docs/ README.md CLAUDE.md
git commit -m "Update docs for vX.Y.Z release"
git push origin main
```

### 3. Tag and push

The tag triggers the entire release pipeline. **Do not update `Cargo.toml` manually** — CI stamps the version from the git tag automatically.

```bash
git tag vX.Y.Z
git push origin vX.Y.Z
```

### 4. Monitor the pipeline

The release workflow runs 5 stages sequentially:

```
CI Gate (fmt + clippy + tests + audit)
  → Validate Release (stamp version, build UI, cargo check)
    → Build Binaries (4 targets in parallel) + Docker Build (2 platforms)
      → Create Release (checksums, SBOM, attestation, GitHub Release)
```

Monitor progress:

```bash
# Watch the run
gh run list --limit 1
gh run view <run-id>

# Check specific job
gh run view --job=<job-id>
```

**Expected timings:**
- CI Gate: ~5 min
- Validate: ~7 min
- Builds: ~6 min (parallel)
- Docker: ~30-55 min (multi-arch cross-compilation)
- Create Release: ~5 min (SBOM + attestation)
- **Total: ~50-70 min**

### 5. Verify the release

After the pipeline completes:

```bash
# GitHub Release exists with binaries + checksums + SBOM
gh release view vX.Y.Z

# Docker image is on DockerHub
docker manifest inspect beshultd/deltaglider_proxy:X.Y.Z

# Docker tags are correct
# vX.Y.Z → tags: X.Y.Z, X.Y, X, latest
docker pull beshultd/deltaglider_proxy:X.Y.Z
docker pull beshultd/deltaglider_proxy:latest
```

### 6. Update DockerHub description (manual)

The DockerHub repo description is not auto-updated. If `DOCKERHUB.md` changed:

1. Go to https://hub.docker.com/r/beshultd/deltaglider_proxy
2. Click "Manage Repository" → "Description"
3. Paste the contents of `DOCKERHUB.md`

## What the pipeline does automatically

You do NOT need to do any of this manually:

| Step | Automated by |
|------|--------------|
| Version stamping in `Cargo.toml` | `release.yml` — extracts from git tag, `sed` into Cargo.toml |
| UI build (`npm ci && npm run build`) | `release.yml` — runs in each build job |
| Binary compilation (4 targets) | `release.yml` — Linux x86/ARM, macOS Intel/ARM |
| Binary stripping | `release.yml` — `strip` on each binary |
| Docker multi-arch build + push | `release.yml` — buildx with QEMU for ARM64 |
| Docker tags (semver cascade) | `release.yml` — `X.Y.Z`, `X.Y`, `X`, `latest` |
| SHA256 checksums | `release.yml` — `sha256sum` on all archives |
| SBOM generation | `release.yml` — `cargo sbom` (SPDX JSON) |
| Build provenance attestation | `release.yml` — GitHub attestation action |
| GitHub Release creation | `release.yml` — with auto-generated release notes |

## Version numbering

We use [semver](https://semver.org/):

- **MAJOR** (`X.0.0`): Breaking S3 API changes, storage format changes requiring migration
- **MINOR** (`0.X.0`): New features, new metrics, new config options, significant bug fixes
- **PATCH** (`0.0.X`): Bug fixes, documentation, performance improvements

The version in `Cargo.toml` on `main` stays at the LAST released version. CI stamps the NEW version from the git tag at build time. This means `main` always reflects the latest release, and unreleased work doesn't carry a premature version.

## Troubleshooting

### CI Gate fails

Fix the issue on `main`, push, then delete and re-create the tag:

```bash
# Fix the issue
git push origin main

# Move the tag
git tag -d vX.Y.Z
git push origin :refs/tags/vX.Y.Z
git tag vX.Y.Z
git push origin vX.Y.Z
```

### Docker build times out

The multi-arch Docker build (especially ARM64 via QEMU) can take 30-55 minutes. This is normal. If it fails:

1. Check the job logs for OOM or disk space issues
2. Re-run the failed job from the GitHub Actions UI

### Attestation hangs

The `attest-build-provenance` step contacts Sigstore's infrastructure. If it hangs on a self-hosted runner, it may be a network issue. The Docker images and binaries are already published at this point — the release is usable even if attestation fails.

### Wrong version tagged

```bash
# Delete remote tag
git push origin :refs/tags/vX.Y.Z

# Delete local tag
git tag -d vX.Y.Z

# Create correct tag
git tag vX.Y.Z <correct-commit-sha>
git push origin vX.Y.Z
```
