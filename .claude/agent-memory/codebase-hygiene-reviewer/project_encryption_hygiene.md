---
name: Encryption-at-rest hygiene review
description: DRY/dead-code findings from the per-backend encryption feature (commits c92447c..15df8e2)
type: project
---

The per-backend encryption feature (12 commits, ~5k LOC) introduces several consolidation opportunities.

**Key architectural observation:** the user expected 3 "similar types" (EncryptionConfig / ResolvedEncryption / BackendEncryptionConfig) but only 2 exist — the "resolved" shape is an untyped tuple returned from `wrap_backend_with_encryption`. There is NO `ResolvedEncryption` type.

**Why:** shared reference for future reviews of this feature — the triple-type concern was unfounded.
**How to apply:** when reviewing this feature, focus on (1) the shared `legacy_key` extraction across 3 enum variants, (2) the two `derive_key_id` implementations (engine + field_level), (3) the two near-identical `chunked_decrypt_stream[_from_chunk]` unfold loops, (4) the `BackendEncryptionKeyProbe<'a>` with dead lifetime/phantom, not on type-count duplication.
