# Storage format

This document is for debugging, incident response, and "what exactly is in my bucket?" curiosity.

## Deltaspaces and baselines

DeltaGlider Proxy groups objects into **deltaspaces** based on their key prefix (everything before the last `/`). Each deltaspace maintains one internal baseline called `reference.bin`.

- Baseline data: `reference.bin`
- Baseline metadata: stored as `user.dg.metadata` xattr on `reference.bin`

The baseline is **not** a user-visible S3 key. It's internal, and it is seeded from the first delta-eligible upload in that deltaspace. Every delta patch is computed **against this baseline** (no delta chains).

Root-level keys (no `/` in the key) use an empty deltaspace id and their files are stored directly in the deltaspaces root (filesystem) or bucket root (S3).

## What gets stored for user objects

For a user key like `releases/v2.zip`, DeltaGlider Proxy stores one of:

- Delta object: `v2.zip.delta` (metadata in xattr)
- Passthrough object: `v2.zip` (stored as-is with original filename, metadata in xattr)

Delta eligibility is currently a hard-coded extension allowlist in `src/deltaglider/file_router.rs` (archives/backups/db dumps by default).

## Filesystem backend layout

All data lives under `${DGP_DATA_DIR}`:

```text
data/
  deltaspaces/
    releases/
      reference.bin
      v1.zip.delta
      readme.txt               # Passthrough: stored with original filename
    reference.bin              # Root-level deltaspace
    file.zip.delta
```

If a deltaspace prefix contains `/`, it becomes nested directories under `deltaspaces/`.

### Metadata storage (xattr)

On the filesystem backend, metadata is stored as a `user.dg.metadata` extended attribute on each data file's inode. This eliminates the need for separate `.meta` sidecar files, preventing metadata/data desync and halving inode usage.

**Requirements:** The data directory must be on a filesystem that supports extended attributes — ext4, XFS, Btrfs, ZFS, or APFS. The server validates xattr support at startup and will refuse to start if the filesystem does not support them.

**Inspecting metadata:**
- macOS: `xattr -p user.dg.metadata <file>`
- Linux: `getfattr -n user.dg.metadata --only-values <file>`

## S3 backend layout

Each API bucket maps 1:1 to a real S3 bucket. DeltaGlider artifacts are stored using the same naming scheme as the filesystem backend, with `.meta` sidecar objects for metadata:

```text
releases/reference.bin
releases/v1.zip.delta
releases/readme.txt            # Passthrough: stored with original filename
```

Metadata is stored as S3 user metadata headers (`x-amz-meta-dg-*`) on each object.

## Metadata schema

Metadata is JSON serialized from `crate::types::FileMetadata`. Fields to know:

- Common:
  - `tool`, `original_name`, `file_sha256`, `file_size`, `md5`, `created_at`, `content_type`
- `note = "delta"`:
  - `ref_key`, `ref_sha256`, `delta_size`, `delta_cmd`
- `note = "passthrough"`: no extra fields (stored as-is with original filename)
- `note = "reference"`: internal baseline only (`source_name` records the user key that seeded the baseline)
