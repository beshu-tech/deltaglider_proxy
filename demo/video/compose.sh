#!/usr/bin/env bash
# Compose the final 4:3 captioned demo MP4 from the raw Playwright WebM.
#
# This machine's ffmpeg has no libass/libfreetype (no subtitles/drawtext), so
# captions and title/outro cards are rendered to PNGs with ImageMagick and
# composited with ffmpeg's overlay/concat filters.
#
#   raw .webm (1280x960, ~77s, VP8)
#     → H.264 4:3 body with PNG lower-third captions overlaid per time window
#     → prepend a branded title card + append an outro card
#     → deltaglider-demo-90s.mp4 (1280x960, H.264, YouTube-ready)
#
# Usage: demo/video/compose.sh [raw.webm]
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DIR/../.." && pwd)"
SHOT="${DGP_DEMO_DIR:-/private/tmp/dgp-demo-video}"
RAW="${1:-$(ls -t "$SHOT"/video/*.webm | head -1)}"
WORK="$SHOT/compose"; rm -rf "$WORK"; mkdir -p "$WORK/caps"
OUT="$ROOT/deltaglider-demo-90s.mp4"

W=1280; H=960; FPS=30
SANS='/System/Library/Fonts/SFNS.ttf'
MONO='/System/Library/Fonts/SFNSMono.ttf'
GREEN='#10b981'; INK='#0f1419'; MUTE='#9aa4ad'
MAGICK=$(command -v magick || echo "convert")

TITLE_SEC=$(python3 -c "import json;print(json.load(open('$DIR/captions.json'))['title_card_seconds'])")
OUTRO_SEC=$(python3 -c "import json;print(json.load(open('$DIR/captions.json'))['outro_card_seconds'])")

echo "raw: $RAW"

# ── 1. Title + outro cards (full-frame PNG → looped video) ───────────────────
echo "==> 1/4 title + outro cards"
"$MAGICK" -size ${W}x${H} xc:"$INK" \
  -gravity center \
  -font "$SANS" -pointsize 66 -fill white   -annotate +0-100 'DELTAGLIDER PROXY' \
  -font "$MONO" -pointsize 24 -fill "$MUTE"  -annotate +0+0   'S3-compatible storage with transparent delta compression' \
  -font "$SANS" -pointsize 28 -fill "$GREEN" -annotate +0+60  'encrypted · deduplicated · drop-in' \
  "$WORK/title.png"
"$MAGICK" -size ${W}x${H} xc:"$INK" \
  -gravity center \
  -font "$SANS" -pointsize 42 -fill white   -annotate +0-50 'Point any S3 client at it. Carry on.' \
  -font "$MONO" -pointsize 26 -fill "$GREEN" -annotate +0+40 'github.com/beshu-tech/deltaglider_proxy' \
  "$WORK/outro.png"
ffmpeg -y -loop 1 -t "$TITLE_SEC" -i "$WORK/title.png" -r $FPS \
  -vf "scale=$W:$H,format=yuv420p" -c:v libx264 -crf 20 -preset medium "$WORK/title.mp4" >/dev/null 2>&1
ffmpeg -y -loop 1 -t "$OUTRO_SEC" -i "$WORK/outro.png" -r $FPS \
  -vf "scale=$W:$H,format=yuv420p" -c:v libx264 -crf 20 -preset medium "$WORK/outro.mp4" >/dev/null 2>&1

# ── 2. Caption strips (transparent lower-third PNGs) ─────────────────────────
echo "==> 2/4 caption strips"
N=$(python3 -c "import json;print(len(json.load(open('$DIR/captions.json'))['captions']))")
python3 - "$DIR/captions.json" "$WORK/caps" "$SANS" "$W" <<'PY'
import json, sys, subprocess, shutil
cfg, outdir, font, W = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
caps = json.load(open(cfg))['captions']
magick = shutil.which('magick') or 'convert'
for i, c in enumerate(caps):
    # Lower-third band, 110px tall, translucent ink, white text.
    subprocess.run([
        magick, '-size', f'{W}x110', 'xc:rgba(15,20,25,0.85)',
        '-gravity', 'center', '-fill', 'white', '-font', font, '-pointsize', '32',
        '-annotate', '+0+0', c['text'],
        f'{outdir}/cap{i}.png',
    ], check=True)
print(f'{len(caps)} caption strips')
PY

# ── 3. Body: webm → H.264 with caption strips overlaid per window ────────────
echo "==> 3/4 body + captions"
# Build the overlay filterchain: each cap PNG is an input, overlaid at the
# bottom, enabled only during its [start+offset, end+offset] window.
mapfile -t TIMES < <(python3 -c "
import json
cfg=json.load(open('$DIR/captions.json'))
off=cfg['title_card_seconds']  # captions are relative to body; body sits AFTER title in final, but we overlay on the body BEFORE concat, so NO offset here
for c in cfg['captions']: print(f\"{c['start']:.2f} {c['end']:.2f}\")
")
INPUTS=(-i "$RAW")
for i in $(seq 0 $((N-1))); do INPUTS+=(-i "$WORK/caps/cap$i.png"); done
FC="[0:v]fps=$FPS,scale=$W:$H[base];"
PREV="base"
for i in $(seq 0 $((N-1))); do
  read -r s e <<< "${TIMES[$i]}"
  NEXT="v$i"
  IN=$((i+1))
  FC+="[$PREV][$IN:v]overlay=x=0:y=$H-110:enable='between(t,$s,$e)'[$NEXT];"
  PREV="$NEXT"
done
FC="${FC%;}"  # drop trailing semicolon; last label is $PREV
ffmpeg -y "${INPUTS[@]}" -filter_complex "$FC" -map "[$PREV]" \
  -c:v libx264 -pix_fmt yuv420p -crf 20 -preset medium -an "$WORK/body.mp4" >/dev/null 2>&1

# ── 4. Concat title + body + outro ───────────────────────────────────────────
echo "==> 4/4 concat"
printf "file '%s'\nfile '%s'\nfile '%s'\n" "$WORK/title.mp4" "$WORK/body.mp4" "$WORK/outro.mp4" > "$WORK/concat.txt"
ffmpeg -y -f concat -safe 0 -i "$WORK/concat.txt" -c:v libx264 -pix_fmt yuv420p -crf 20 -preset medium "$OUT" >/dev/null 2>&1

DUR=$(ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 "$OUT")
echo "done: $OUT  (${DUR}s, ${W}x${H})"
