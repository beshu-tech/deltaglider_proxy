# Storage format

This document is for debugging, incident response, and “what exactly is in my bucket?” curiosity.

## Deltaspaces and baselines

DeltaGlider Proxy groups objects into **deltaspaces** based on their key prefix (everything before the last `/`). Each deltaspace maintains one internal baseline called `reference.bin`.

- Baseline data: `reference.bin` (full bytes)
- Baseline metadata: `reference.bin.meta` (JSON)

The baseline is **not** a user-visible S3 key. It’s internal, and it is seeded from the first delta-eligible upload in that deltaspace. Every delta patch is computed **against this baseline** (no delta chains).

Root keys (no `/` in the key) use the special deltaspace id `_root_`.

## What gets stored for user objects

For a user key like `releases/v2.zip`, DeltaGlider Proxy stores one of:

- Delta object:
  - `v2.zip.delta`
  - `v2.zip.delta.meta`
- Direct object:
  - `v2.zip.direct`
  - `v2.zip.direct.meta`

Delta eligibility is currently a hard-coded extension allowlist in `src/deltaglider/file_router.rs` (archives/backups/db dumps by default).

## Filesystem backend layout

All data lives under `${DELTAGLIDER_PROXY_DATA_DIR}`:

```text
data/
  deltaspaces/
    releases/
      reference.bin
      reference.bin.meta
      v1.zip.delta
      v1.zip.delta.meta
      readme.txt.direct
      readme.txt.direct.meta
    _root_/
      ...
```

If a deltaspace prefix contains `/`, it becomes nested directories under `deltaspaces/`.

## S3 backend layout

The configured backend bucket (`DELTAGLIDER_PROXY_S3_BUCKET`) stores DeltaGlider artifacts using the same naming scheme as the filesystem backend:

```text
releases/reference.bin
releases/reference.bin.meta
releases/v1.zip.delta
releases/v1.zip.delta.meta
```

## Metadata schema

Metadata is JSON serialized from `crate::types::FileMetadata`. Fields to know:

- Common:
  - `tool`, `original_name`, `file_sha256`, `file_size`, `md5`, `created_at`, `content_type`
- `note = "delta"`:
  - `ref_key`, `ref_sha256`, `delta_size`, `delta_cmd`
- `note = "direct"`: no extra fields
- `note = "reference"`: internal baseline only (`source_name` records the user key that seeded the baseline)

