#!/usr/bin/env bash
# Prepare a release by bumping the version in Cargo.toml + Cargo.lock and
# stamping the CHANGELOG.
#
# Usage:
#   scripts/release-prep.sh patch
#   scripts/release-prep.sh minor
#   scripts/release-prep.sh major
#   scripts/release-prep.sh 1.2.3        # explicit
#
# Outputs (to stdout, last line only):
#   NEW_VERSION=X.Y.Z
#
# Side effects:
#   - Cargo.toml: package.version bumped to NEW_VERSION
#   - Cargo.lock: refreshed via `cargo update -p deltaglider_proxy --offline`
#                 (falls back to `cargo update -p deltaglider_proxy` if needed)
#   - operator, Helm chart and the docs that name an image version: stamped
#     (see "Stamp the deployment pins" below)
#   - CHANGELOG.md: `## Unreleased` block renamed to
#                   `## vX.Y.Z — YYYY-MM-DD` and a fresh `## Unreleased`
#                   inserted above it.
#
# Pre-conditions enforced:
#   - Working tree must be clean (no uncommitted changes).
#   - CHANGELOG.md must contain a `## Unreleased` heading.
#   - The chosen NEW_VERSION must be strictly greater than the current
#     Cargo.toml version (semver compare).
#
# Failure modes return non-zero with a descriptive `error:` line on stderr.
# All shell-meta inputs are validated against a tight regex before they
# touch sed/awk — no untrusted strings reach a shell expansion.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

CARGO_TOML="$ROOT/Cargo.toml"
CARGO_LOCK="$ROOT/Cargo.lock"
CHANGELOG="$ROOT/CHANGELOG.md"

err() { echo "error: $*" >&2; exit 1; }

# ── Argument parsing ────────────────────────────────────────────────
[ $# -eq 1 ] || err "usage: $0 <patch|minor|major|X.Y.Z>"
BUMP="$1"

# Reject anything that's not a strict bump-keyword or a clean semver.
if [[ ! "$BUMP" =~ ^(patch|minor|major|[0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
    err "bump must be 'patch', 'minor', 'major', or a semver string like '1.2.3' (got: $BUMP)"
fi

# ── Pre-flight: clean tree ──────────────────────────────────────────
if ! git diff --quiet || ! git diff --cached --quiet; then
    err "working tree is dirty — commit or stash before running release-prep"
fi

# ── Read current version ────────────────────────────────────────────
CURRENT_VERSION="$(grep -E '^version = ' "$CARGO_TOML" | head -1 | sed -E 's/^version = "([^"]+)".*/\1/')"
if [[ ! "$CURRENT_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    err "Cargo.toml has unparseable version '$CURRENT_VERSION'"
fi

IFS='.' read -r MAJ MIN PATCH <<< "$CURRENT_VERSION"

# ── Compute new version ─────────────────────────────────────────────
case "$BUMP" in
    patch) NEW_VERSION="${MAJ}.${MIN}.$((PATCH + 1))" ;;
    minor) NEW_VERSION="${MAJ}.$((MIN + 1)).0" ;;
    major) NEW_VERSION="$((MAJ + 1)).0.0" ;;
    *)     NEW_VERSION="$BUMP" ;;
esac

# Strict greater-than check to prevent accidental backwards bumps.
# `sort -V` puts versions in ascending order; the smaller of the two
# must be the current one.
if [ "$NEW_VERSION" = "$CURRENT_VERSION" ]; then
    err "new version $NEW_VERSION equals current $CURRENT_VERSION"
fi
SORTED="$(printf '%s\n%s\n' "$CURRENT_VERSION" "$NEW_VERSION" | sort -V | head -1)"
if [ "$SORTED" != "$CURRENT_VERSION" ]; then
    err "new version $NEW_VERSION is not greater than current $CURRENT_VERSION"
fi

echo "current: $CURRENT_VERSION → new: $NEW_VERSION" >&2

# ── Stamp Cargo.toml ────────────────────────────────────────────────
# Replace ONLY the first `version = "..."` line. The package metadata
# lives near the top of Cargo.toml, before any [dependencies] could
# introduce a same-named field.
#
# Use a temp file + atomic mv so a partial write can't corrupt the
# file. `sed -i` semantics differ between BSD and GNU; the temp-file
# approach is portable.
tmp="$(mktemp)"
awk -v new="$NEW_VERSION" '
    /^version = "/ && !done {
        print "version = \"" new "\""
        done = 1
        next
    }
    { print }
' "$CARGO_TOML" > "$tmp"
mv "$tmp" "$CARGO_TOML"

# Verify the stamp took.
STAMPED="$(grep -E '^version = ' "$CARGO_TOML" | head -1 | sed -E 's/^version = "([^"]+)".*/\1/')"
if [ "$STAMPED" != "$NEW_VERSION" ]; then
    err "failed to stamp Cargo.toml (got: $STAMPED)"
fi

# ── Refresh Cargo.lock ──────────────────────────────────────────────
# Try offline first (fast, no network); fall back to online if the
# package metadata isn't cached.
if cargo update --workspace --offline >/dev/null 2>&1; then
    :
else
    cargo update --workspace >/dev/null 2>&1 || err "cargo update failed — Cargo.lock won't be in sync"
fi

# Sanity: the package's version in Cargo.lock matches.
LOCK_VERSION="$(awk '
    /^name = "deltaglider_proxy"/ { in_pkg = 1; next }
    in_pkg && /^version = "/ {
        gsub(/^version = "|"$/, "")
        print
        exit
    }
' "$CARGO_LOCK")"
if [ "$LOCK_VERSION" != "$NEW_VERSION" ]; then
    err "Cargo.lock didn't pick up new version (got: $LOCK_VERSION, expected: $NEW_VERSION)"
fi

# ── Stamp CHANGELOG.md ──────────────────────────────────────────────
# Replace `## Unreleased` (case-insensitive on the word) with
# `## vX.Y.Z — YYYY-MM-DD`, and insert a fresh `## Unreleased\n\n`
# block above it. The em dash matches the existing CHANGELOG style.
#
# Refuse if the file doesn't already have an `## Unreleased` heading
# — that means the previous release didn't reset it, and continuing
# would silently lose history.
if ! grep -qE '^## Unreleased$' "$CHANGELOG"; then
    err "CHANGELOG.md has no '## Unreleased' heading — won't continue"
fi

DATE="$(date -u +%Y-%m-%d)"
tmp="$(mktemp)"
awk -v new="$NEW_VERSION" -v date="$DATE" '
    /^## Unreleased$/ && !done {
        # Fresh empty Unreleased above, then the dated section.
        print "## Unreleased"
        print ""
        print "## v" new " — " date
        done = 1
        next
    }
    { print }
' "$CHANGELOG" > "$tmp"
mv "$tmp" "$CHANGELOG"

# Sanity: confirm both headings are now present.
if ! grep -qE "^## v${NEW_VERSION} — ${DATE}$" "$CHANGELOG"; then
    err "CHANGELOG.md stamp didn't take"
fi
if ! grep -qE '^## Unreleased$' "$CHANGELOG"; then
    err "CHANGELOG.md lost its Unreleased heading after stamp"
fi

# Regenerate the rendered changelog doc from the stamped CHANGELOG.md so the
# release PR stays in parity with the CI gate (gen-changelog-doc.sh --check).
# Without this, EVERY release PR fails the changelog-parity check and needs a
# manual fix on the release branch — the recurring release gotcha.
if [ -x "$ROOT/scripts/gen-changelog-doc.sh" ]; then
    "$ROOT/scripts/gen-changelog-doc.sh" >/dev/null \
        || err "gen-changelog-doc.sh failed after CHANGELOG stamp"
fi

# ── Stamp the deployment pins ───────────────────────────────────────
# The Kubernetes operator and the Helm chart default to a proxy image, so a
# release that leaves them behind ships a fix that Helm and operator users do
# not get (v2.0.1). Point both at the new proxy version, and bump the
# operator's and the chart's own patch versions: operator-ci.yml publishes a
# new operator image on main when the operator version changes. The lib test
# `deployment_pins_follow_the_proxy_version` fails when these drift.
bump_patch() { awk -F. -v OFS=. '{ $3 = $3 + 1; print }' <<< "$1"; }
OP_TOML="$ROOT/operator/Cargo.toml"
OP_CUR="$(grep -E '^version = ' "$OP_TOML" | head -1 | sed -E 's/^version = "([^"]+)".*/\1/')"
OP_NEW="$(bump_patch "$OP_CUR")"
CHART="$ROOT/charts/deltaglider-proxy/Chart.yaml"
CH_CUR="$(sed -nE 's/^version: *([0-9.]+).*/\1/p' "$CHART" | head -1)"
CH_NEW="$(bump_patch "$CH_CUR")"
[[ "$OP_NEW" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ && "$CH_NEW" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || err "could not read the operator ($OP_CUR) or chart ($CH_CUR) version"
stamp() { # file, awk program; atomic replace
    local f="$1" t; t="$(mktemp)"
    awk -v proxy="$NEW_VERSION" -v op="$OP_NEW" -v chart="$CH_NEW" "$2" "$f" > "$t" && mv "$t" "$f"
}
stamp "$OP_TOML" '/^version = "/ && !d { print "version = \"" op "\""; d = 1; next } { print }'
stamp "$ROOT/operator/src/crd.rs" '{ sub(/beshultd\/deltaglider_proxy:[0-9]+\.[0-9]+\.[0-9]+/, "beshultd/deltaglider_proxy:" proxy); print }'
stamp "$ROOT/operator/README.md" '{ sub(/beshultd\/deltaglider_proxy:[0-9]+\.[0-9]+\.[0-9]+/, "beshultd/deltaglider_proxy:" proxy); print }'
stamp "$ROOT/operator/deploy/operator.yaml" '{ sub(/beshultd\/deltaglider-operator:[0-9]+\.[0-9]+\.[0-9]+/, "beshultd/deltaglider-operator:" op); print }'
stamp "$CHART" '/^version:/ { print "version: " chart; next } /^appVersion:/ { print "appVersion: \"" proxy "\""; next } { print }'
# The docs that name one image version: the upgrade steps, the Docker Hub tag
# table, the first tutorial's startup line and the compose example. They
# still said 2.0.0 at v2.0.2.
stamp "$ROOT/docs/product/how-to/upgrade.md" '{ gsub(/beshultd\/deltaglider_proxy:[0-9]+\.[0-9]+\.[0-9]+/, "beshultd/deltaglider_proxy:" proxy); print }'
stamp "$ROOT/DOCKERHUB.md" '{ sub(/^[|] `[0-9]+\.[0-9]+\.[0-9]+` [|]/, "| `" proxy "` |"); print }'
stamp "$ROOT/docs/product/tutorials/first-delta-savings.md" '{ sub(/Starting DeltaGlider Proxy v[0-9]+\.[0-9]+\.[0-9]+/, "Starting DeltaGlider Proxy v" proxy); print }'
stamp "$ROOT/examples/docker-compose/secrets.env.example" '{ sub(/DGP_IMAGE_TAG=[0-9]+\.[0-9]+\.[0-9]+/, "DGP_IMAGE_TAG=" proxy); print }'
( cd "$ROOT/operator" && { cargo update -p deltaglider-operator --offline >/dev/null 2>&1 \
    || cargo update -p deltaglider-operator >/dev/null 2>&1; } ) \
    || err "cargo update failed in operator/ — operator/Cargo.lock won't be in sync"
grep -q "beshultd/deltaglider_proxy:${NEW_VERSION}\"" "$ROOT/operator/src/crd.rs" \
    || err "operator DEFAULT_IMAGE stamp didn't take"
echo "operator: $OP_CUR → $OP_NEW, chart: $CH_CUR → $CH_NEW" >&2

# ── Output for the workflow to consume ──────────────────────────────
echo "NEW_VERSION=$NEW_VERSION"
