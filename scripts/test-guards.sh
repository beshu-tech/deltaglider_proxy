#!/usr/bin/env bash
# Must-fail fixtures for the shell guards (a guard without one is a comment).
# Each case builds a tiny repo in a temp dir, copies the guard into it, and
# runs it: a known-bad input must exit non-zero (and name the cause), a
# known-good input must exit zero.
#
# Guards under test: check-integration-tests-in-ci.sh, check-workflows.sh,
# check-no-committed-secrets.sh, check-e2e-specs-run.sh.
#
# GUARD_SCRIPTS_DIR (default: scripts/ next to this file) names the directory
# that holds the guards: point it at an older copy to see a fixture go red.
#
# Secret-looking fixtures are assembled at run time (`"AKI""A..."`) so this
# file itself never matches the secret scan.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
GUARDS="${GUARD_SCRIPTS_DIR:-$HERE}"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT

fail=0
n=0

# new_repo <name>: an empty git repo with the three guards under scripts/.
new_repo() {
  local d="$T/$1"
  mkdir -p "$d/scripts" "$d/tests" "$d/.github/workflows"
  cp "$GUARDS"/check-integration-tests-in-ci.sh "$GUARDS"/check-workflows.sh \
     "$GUARDS"/check-no-committed-secrets.sh "$GUARDS"/check-e2e-specs-run.sh "$d/scripts/" 2>/dev/null || true
  # (check-e2e-specs-run.sh may be missing when GUARD_SCRIPTS_DIR names an old copy.)
  git -C "$d" init -q
  git -C "$d" config user.email t@example.invalid
  git -C "$d" config user.name t
  echo "$d"
}

commit() { git -C "$1" add -A -f && git -C "$1" commit -q -m "${2:-fixture}"; }

# expect <pass|fail> <name> <dir> <guard> [needle]: run the guard in <dir>.
# On `fail` the output must also contain <needle> (the guard fails for the
# right reason, not by crashing).
expect() {
  local want="$1" name="$2" dir="$3" guard="$4" needle="${5:-}" out rc=0
  n=$((n + 1))
  out="$(cd "$dir" && "./scripts/$guard" 2>&1)" || rc=$?
  if [[ "$want" == pass ]]; then
    if ((rc != 0)); then echo "FAIL $name: expected pass, exit $rc: $out" >&2; fail=1; else echo "ok   $name"; fi
  else
    if ((rc == 0)); then
      echo "FAIL $name: guard accepted a known-bad input: $out" >&2; fail=1
    elif [[ -n "$needle" && "$out" != *"$needle"* ]]; then
      echo "FAIL $name: guard failed but not for the right reason (want '$needle'): $out" >&2; fail=1
    else
      echo "ok   $name"
    fi
  fi
}

# ── check-integration-tests-in-ci.sh ─────────────────────────────────────
# A baseline repo: one module `foo_test`, declared and selected.
itest_repo() {
  local d; d="$(new_repo "$1")"
  echo '// fixture' > "$d/tests/foo_test.rs"
  printf 'mod foo_test;\n' > "$d/tests/all.rs"
  printf 'jobs:\n  t:\n    steps:\n      - run: cargo test --test all --locked -- foo_test::\n' > "$d/.github/workflows/ci.yml"
  echo "$d"
}

d="$(itest_repo it-good)"
expect pass "integration registry: declared and selected" "$d" check-integration-tests-in-ci.sh

d="$(itest_repo it-comment-full)"
printf 'jobs:\n  t:\n    steps:\n      # cargo test --test all -- foo_test::\n      - run: echo nothing\n' > "$d/.github/workflows/ci.yml"
expect fail "integration registry: module named only in a full-line comment" "$d" check-integration-tests-in-ci.sh "not selected"

d="$(itest_repo it-comment-tail)"
printf 'jobs:\n  t:\n    steps:\n      - run: echo nothing # foo_test:: \n' > "$d/.github/workflows/ci.yml"
expect fail "integration registry: module named only in a trailing comment" "$d" check-integration-tests-in-ci.sh "not selected"

d="$(itest_repo it-undeclared)"
printf '' > "$d/tests/all.rs"
expect fail "integration registry: module not declared in tests/all.rs" "$d" check-integration-tests-in-ci.sh "not declared"

d="$(itest_repo it-unselected)"
printf 'jobs:\n  t:\n    steps:\n      - run: cargo test --lib\n' > "$d/.github/workflows/ci.yml"
expect fail "integration registry: module not selected in ci.yml" "$d" check-integration-tests-in-ci.sh "not selected"

d="$(itest_repo it-ghost)"
printf 'jobs:\n  t:\n    steps:\n      - run: cargo test --test all --locked -- foo_test:: ghost_test::\n' > "$d/.github/workflows/ci.yml"
expect fail "integration registry: filter names no module" "$d" check-integration-tests-in-ci.sh "matches no tests/ghost_test.rs"

d="$(itest_repo it-dup)"
printf 'jobs:\n  t:\n    steps:\n      - run: cargo test --test all --locked -- foo_test::\n      - run: cargo test --test all --locked -- foo_test::\n' > "$d/.github/workflows/ci.yml"
expect fail "integration registry: module selected twice" "$d" check-integration-tests-in-ci.sh "more than once"

d="$(itest_repo it-substring)"
echo '// fixture' > "$d/tests/big_foo_test.rs"
printf 'mod foo_test;\nmod big_foo_test;\n' > "$d/tests/all.rs"
printf 'jobs:\n  t:\n    steps:\n      - run: cargo test --test all --locked -- foo_test:: big_foo_test::\n' > "$d/.github/workflows/ci.yml"
expect fail "integration registry: filter that is a substring of another module" "$d" check-integration-tests-in-ci.sh "also matches"

d="$(itest_repo it-ignore)"
mkdir -p "$d/src"
printf '#[test]\n#[ignore]\nfn slow() {}\n' > "$d/src/lib.rs"
expect fail "integration registry: bare #[ignore]" "$d" check-integration-tests-in-ci.sh "CI never runs it"

d="$(itest_repo it-ignore-manual)"
mkdir -p "$d/src"
printf '#[test]\n#[ignore = "manual: cargo test --test all -- slow --ignored"]\nfn slow() {}\n' > "$d/src/lib.rs"
expect pass "integration registry: #[ignore] naming its manual command" "$d" check-integration-tests-in-ci.sh

# ── check-workflows.sh ───────────────────────────────────────────────────
SHA=0123456789abcdef0123456789abcdef01234567
DIGEST_A="$(printf 'a%.0s' $(seq 64))"
DIGEST_B="$(printf 'b%.0s' $(seq 64))"

wf_repo() { # name, workflow body
  local d; d="$(new_repo "$1")"
  printf '%s\n' "$2" > "$d/.github/workflows/w.yml"
  commit "$d" base
  echo "$d"
}

d="$(wf_repo wf-good "steps:
  - uses: actions/checkout@${SHA} # v6.0.2")"
expect pass "workflows: SHA-pinned action with version comment" "$d" check-workflows.sh

d="$(wf_repo wf-tag "steps:
  - uses: actions/checkout@v4")"
expect fail "workflows: action pinned by tag" "$d" check-workflows.sh "not pinned to a commit SHA"

d="$(wf_repo wf-nocomment "steps:
  - uses: actions/checkout@${SHA}")"
expect fail "workflows: SHA pin without version comment" "$d" check-workflows.sh "no version comment"

d="$(wf_repo wf-silo-tag "image: pgsty/silo:RELEASE.2026-09-16")"
expect fail "workflows: MinIO image by tag" "$d" check-workflows.sh "by tag"

d="$(wf_repo wf-silo-two "a: pgsty/silo@sha256:${DIGEST_A}
b: pgsty/silo@sha256:${DIGEST_B}")"
expect fail "workflows: two MinIO digests" "$d" check-workflows.sh "different pgsty/silo digests"

# [skip-integration] scope.
skip_repo() { # name: base commit has src/a.rs and docs/x.md
  local d; d="$(wf_repo "$1" "steps: []")"
  mkdir -p "$d/src" "$d/docs"
  # Long enough for git's rename detection (50% similarity).
  seq 1 40 > "$d/src/a.rs"; echo doc > "$d/docs/x.md"
  commit "$d" "add sources"
  echo "$d"
}
run_skip() { # dir before-sha [message]
  (cd "$1" && SKIP_CHECK_BEFORE="$2" SKIP_CHECK_AFTER=HEAD SKIP_CHECK_MESSAGE="${3:-fix [skip-integration]}" ./scripts/check-workflows.sh 2>&1)
}
expect_skip() { # want name dir before needle
  local want="$1" name="$2" dir="$3" before="$4" needle="${5:-}" out rc=0
  n=$((n + 1))
  out="$(run_skip "$dir" "$before")" || rc=$?
  if [[ "$want" == pass && $rc -ne 0 ]]; then echo "FAIL $name: expected pass: $out" >&2; fail=1
  elif [[ "$want" == fail && $rc -eq 0 ]]; then echo "FAIL $name: guard accepted a known-bad input: $out" >&2; fail=1
  elif [[ "$want" == fail && -n "$needle" && "$out" != *"$needle"* ]]; then echo "FAIL $name: wrong reason (want '$needle'): $out" >&2; fail=1
  else echo "ok   $name"; fi
}

d="$(skip_repo skip-docs)"
before="$(git -C "$d" rev-parse HEAD)"
echo more >> "$d/docs/x.md"; commit "$d" docs
expect_skip pass "[skip-integration]: docs-only push" "$d" "$before"

d="$(skip_repo skip-src)"
before="$(git -C "$d" rev-parse HEAD)"
echo more >> "$d/src/a.rs"; commit "$d" code
expect_skip fail "[skip-integration]: push that changes source" "$d" "$before" "src/a.rs"

d="$(skip_repo skip-rename)"
before="$(git -C "$d" rev-parse HEAD)"
git -C "$d" mv src/a.rs docs/a.md; commit "$d" rename
expect_skip fail "[skip-integration]: rename of a source file into docs/" "$d" "$before" "src/a.rs"

d="$(skip_repo skip-nostart)"
expect_skip fail "[skip-integration]: unknown start of the push range" "$d" "0000000000000000000000000000000000000000" "no known start"

# ── check-no-committed-secrets.sh ────────────────────────────────────────
secret_repo() { # name; a clean tree
  local d; d="$(new_repo "$1")"
  echo "hello" > "$d/README.md"
  echo "$d"
}
secret_case() { # name, relative path, content, needle
  local d; d="$(secret_repo "sec-$1")"
  mkdir -p "$d/$(dirname "$2")"
  printf '%s\n' "$3" > "$d/$2"
  commit "$d"
  expect fail "secrets: $1" "$d" check-no-committed-secrets.sh "$4"
}

d="$(secret_repo sec-clean)"; commit "$d"
expect pass "secrets: clean tree" "$d" check-no-committed-secrets.sh

d="$(secret_repo sec-example)"
printf 'access_key_id: %s\n' "AKIA""IOSFODNN7""EXAMPLE" > "$d/doc.md"
printf 'fake: %s\n' "AKIA""DOCSDANA00000001" >> "$d/doc.md"
commit "$d"
expect pass "secrets: documentation placeholder keys" "$d" check-no-committed-secrets.sh

secret_case "AWS access key id"   conf.yaml "access_key_id: AKI""AJ7QWERTYUIOPASDF"          "possible live secret"
secret_case "AWS session key id"  conf.yaml "id: ASI""AJ7QWERTYUIOPASDF"                      "possible live secret"
secret_case "GitHub token"        notes.txt "token ghp""_0123456789abcdefghijABCDEFGHIJ012345"   "possible live secret"
secret_case "GitHub fine-grained" notes.txt "github""_pat_11ABCDEFG0123456789_abcdefghijklmn"   "possible live secret"
secret_case "Slack token"         notes.txt "xox""b-123456789012-abcdef"                       "possible live secret"
secret_case "PEM block"           k.txt     "-----BEGIN RSA PRIVATE"" KEY-----"               "possible live secret"
secret_case "Stripe live key"     notes.txt "sk_""live_0123456789abcdefghijkl"               "possible live secret"
secret_case "key file (.pem)"     certs/server.pem "x"                                       "forbidden file"
secret_case "key file (.key)"     certs/server.key "x"                                       "forbidden file"
secret_case "bootstrap hash file" .deltaglider_bootstrap_hash "x"                             "forbidden file"
secret_case "config DB key file"  data/deltaglider_config.db.key "x"                          "forbidden file"
secret_case ".env file"           .env      "X=1"                                             "forbidden file"
secret_case "nested secrets.env"  iac-prod/secrets.env "X=1"                                  "forbidden file"
secret_case "SQLite DB"           data/state.db "x"                                           "forbidden file"
secret_case "backup zip"          backup.zip "x"                                              "forbidden file"

# ── check-e2e-specs-run.sh ───────────────────────────────────────────────
e2e_repo() { # name, spec path, spec body
  local d; d="$(new_repo "$1")"
  mkdir -p "$d/demo/s3-browser/ui/e2e/$(dirname "$2")"
  printf '%s\n' "$3" > "$d/demo/s3-browser/ui/e2e/$2"
  printf 'export QA_REGRESSION=1\n' > "$d/scripts/run.sh"
  echo "$d"
}
d="$(e2e_repo e2e-good qa-x.spec.ts "test.skip(!process.env.QA_REGRESSION, 'qa');")"
expect pass "e2e specs: skip variable is set by a script" "$d" check-e2e-specs-run.sh
d="$(e2e_repo e2e-orphan review/r.spec.ts "test.skip(!process.env.REVIEW, 'Review reproductions: run with REVIEW=1');")"
expect fail "e2e specs: skip variable that nothing sets (the REVIEW=1 spec)" "$d" check-e2e-specs-run.sh "never runs"
d="$(e2e_repo e2e-orphan2 explore/e.spec.ts "test.skip(process.env.EXPLORE !== '1', 'x');")"
expect fail "e2e specs: skip on a variable that nothing sets (EXPLORE)" "$d" check-e2e-specs-run.sh "never runs"

if ((fail != 0)); then
  echo "FAIL: a shell guard accepts input it claims to refuse (see above)." >&2
  exit 1
fi
echo "OK: $n guard fixtures behave as expected."
