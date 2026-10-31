# Plan: encryption needs atomic writes; conditional rewrites; no plaintext_reads

Status: approved by the owner 2026-10-31 (decisions below are his).

## Decisions
1. The conversion's rewrites are conditional (P8 of encryption-conversion-marker.md): every
   object write of the re-encrypt job carries a precondition on the listed ETag. A lost
   precondition means a client wrote a newer object (already encrypted): skip it, count it.
   Where every write of the job is conditional, the per-node write gate is dropped for that
   phase, so client writes on every instance stay safe and are never overwritten.
2. Encryption requires a backend with atomic writes (conditional PUT/DELETE). A fresh
   atomicity probe runs right before every action that turns encryption on or keeps a
   conversion going: apply/validate of an encryption block (bucket or backend), Resume /
   `open_window`, bucket create with encryption. A backend that fails or cannot be verified
   is refused with a clear error naming the backend and the reason; the UI disables the
   encryption options on such a backend and says why. Probe evidence on 2026-10-31: Backblaze
   B2 (eu-central-003) closes the connection on `If-None-Match`/`If-Match` PUTs.
3. An encrypted bucket that already exists on a backend without atomic writes (prod:
   `ror-encrypted` on `b2`) becomes READ-ONLY: reads keep working; every write (S3, form POST,
   admin bulk ops, jobs that write into it) is refused with a clear error. Boot logs it; the UI
   shows it on the bucket and backend cards.
4. `plaintext_reads` is removed, not deprecated: a config that contains the field fails to
   load with an error that names the field. All `serve` code paths, the Convert-now notice and
   the deprecation warnings go. No compatibility shim.

## Milestones
- N1: conditional rewrites + gate removal where safe (backend only).
- N3: remove `plaintext_reads` everywhere (backend, UI, docs), in parallel with N1.
- N2: atomicity probe + refusal on every encryption entry point + read-only encrypted buckets
  on non-atomic backends (backend + UI + docs), after N1 and N3 merge.
- Final: four-way deep review, visual check, gates.
