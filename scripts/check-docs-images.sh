#!/usr/bin/env bash
# Every image that a product doc shows must exist and have alt text.
#
#   ![Alt text.](/_/screenshots/<name>.webp)   a pipeline shot, theme-neutral:
#       docs/screenshots/<name>.light.webp AND <name>.dark.webp must exist, and
#       the alt text must be a full sentence (five words or more, final period),
#       because both viewers show it as the caption.
#   ![Alt](/_/screenshots/<file>)              any other image: the file must exist.
#
# Every image needs non-empty alt text. Fenced code blocks are skipped.
# Run it from anywhere; exit 1 lists every problem.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SHOTS="$ROOT/docs/screenshots"
fail=0
count=0

while IFS= read -r -d '' md; do
  rel="${md#"$ROOT"/}"
  # Markdown images outside fenced code: "<line> US <alt> US <src>". The
  # unit separator is not IFS whitespace, so an empty alt stays a field.
  while IFS=$'\037' read -r line alt src; do
    count=$((count + 1))
    where="$rel:$line"
    if [[ -z "${alt// /}" ]]; then
      echo "IMAGE WITHOUT ALT TEXT: $where ($src)" >&2
      fail=1
    fi
    case "$src" in
      /_/screenshots/*) ;;
      *) continue ;; # external images are not ours to check
    esac
    file="${src#/_/screenshots/}"
    if [[ "$file" =~ ^[A-Za-z0-9_-]+\.webp$ ]]; then
      stem="${file%.webp}"
      for theme in light dark; do
        if [[ ! -f "$SHOTS/$stem.$theme.webp" ]]; then
          echo "MISSING SCREENSHOT: $where needs docs/screenshots/$stem.$theme.webp" >&2
          echo "  -> add the shot to demo/s3-browser/ui/e2e/docs-screenshots/shots/ and run scripts/docs-screenshots.sh --update" >&2
          fail=1
        fi
      done
      words="$(wc -w <<<"$alt")"
      if [[ "$words" -lt 5 || "${alt: -1}" != "." ]]; then
        echo "ALT TEXT NOT A SENTENCE: $where: \"$alt\"" >&2
        echo "  -> write a full sentence (five words or more, ending with a period) that says what the annotation marks" >&2
        fail=1
      fi
    elif [[ ! -f "$SHOTS/$file" ]]; then
      echo "MISSING IMAGE: $where references /_/screenshots/$file, not in docs/screenshots/" >&2
      fail=1
    fi
  done < <(awk '
    /^[[:space:]]*(```|~~~)/ { fence = !fence; next }
    fence { next }
    {
      s = $0
      while (match(s, /!\[[^]]*\]\([^) ]+( "[^"]*")?\)/)) {
        img = substr(s, RSTART, RLENGTH)
        s = substr(s, RSTART + RLENGTH)
        alt = img; sub(/^!\[/, "", alt); sub(/\]\(.*$/, "", alt)
        src = img; sub(/^.*\]\(/, "", src); sub(/[) ].*$/, "", src)
        printf "%d\037%s\037%s\n", NR, alt, src
      }
    }' "$md")
done < <(find "$ROOT/docs/product" -name '*.md' -print0 | sort -z)

# docs/screenshots/ holds pipeline pairs only, and every pair is in use: the
# binary embeds the folder, so a stray or unused file costs every download.
for f in "$SHOTS"/*; do
  name="$(basename "$f")"
  if [[ ! "$name" =~ ^[a-z0-9]+(-[a-z0-9]+)*\.(light|dark)\.webp$ ]]; then
    echo "NOT A PIPELINE SHOT: docs/screenshots/$name (only <id>.light.webp / <id>.dark.webp from scripts/docs-screenshots.sh)" >&2
    fail=1
    continue
  fi
  stem="${name%.*.webp}"
  # Plain grep, not git grep: CI runs in a container where git refuses the
  # checkout ("dubious ownership"), which made every shot look unused.
  if ! grep -rqF --exclude-dir=__tests__ --exclude-dir=node_modules --exclude-dir=dist "$stem" \
      "$ROOT/docs/product" "$ROOT/README.md" "$ROOT/marketing/src" "$ROOT/demo/s3-browser/ui/src"; then
    echo "UNUSED SCREENSHOT: docs/screenshots/$name (no page references $stem; delete the pair and its shot)" >&2
    fail=1
  fi
done

if [[ "$fail" -eq 0 ]]; then
  echo "docs images OK: $count image references, every file present, every alt text set, every shot in use"
fi
exit "$fail"
