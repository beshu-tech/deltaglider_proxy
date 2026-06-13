# Product demo video

Reproducible ~90-second 4:3 screen-recording of the admin UI: add an
encrypted + compressed backend → create a routed bucket → set up an IAM
user + group → browse objects → show delta/encryption metadata. Captions are
burned in; no audio.

## One-shot

```bash
# Needs: release binary built (cargo build --release), Playwright in
# demo/s3-browser/ui/node_modules, ffmpeg + ImageMagick on PATH.
demo/video/make.sh
# → deltaglider-demo-90s.mp4 at the repo root
```

## Pieces

- `stage.sh` — boots a throwaway proxy on :9220 (bootstrap/GUI mode, filesystem
  backend, `admin`/`testpassword123`), seeds a `releases` bucket with three
  versioned tarballs that delta against each other, and leaves the encrypted
  backend / demo bucket / IAM user+group **uncreated** (the video makes those).
  Idempotent — restores a pristine config every run.
- `demo.mjs` — Playwright driver. Records a silent WebM at 1280×960 @2x while
  driving the five steps. Selects elements by `data-testid` (added to the
  admin UI for this; see the components under `demo/s3-browser/ui/src/`), so
  it's resilient to copy/icon changes.
- `captions.json` — the caption track (times relative to the screen capture).
- `make-captions.mjs` — renders `captions.json` → an `.ass` (kept for builds
  with libass; the current `compose.sh` renders PNG strips instead).
- `compose.sh` — assembles the final MP4: ImageMagick renders the title/outro
  cards and lower-third caption strips (this machine's ffmpeg has no
  libass/drawtext), ffmpeg overlays them per time window and concats.
- `voiceover-script.txt` — timestamped caption text, for recording a real
  voiceover later.

The throwaway instance, raw WebM, and intermediates live under
`/private/tmp/dgp-demo-video` (override with `DGP_DEMO_DIR`). The final
`deltaglider-demo-90s.mp4` is git-ignored — regenerate it with `make.sh`.

## Re-shooting after a UI change

If a panel changes, the `data-testid`s keep the driver working. If you add a
new step, edit `demo.mjs` + `captions.json` together (same step boundaries),
then `make.sh`. Never put real account names on screen — the cast is fixed:
`hetzner-fsn1`, `db-archive`, `releases`, `backup-bot`, `Engineering`.
