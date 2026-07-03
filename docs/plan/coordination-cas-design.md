# Coordination via S3 conditional-writes — platform detection, capability gating, and the one-path question

> **STATUS (shipped).** The boot-time coordination-bucket CAS validation + witness
> is live. The dead speculative CAS surface (platform.rs, the trait
> `supports_conditional_writes`/probe, the AtomicU8 cache) was DELETED. The
> `CoordinationLease` seam (`src/coordination/`) shipped with `LocalLease` (SQLite,
> single-instance) + `S3Lease` (cross-instance CAS object) + `durable_node_id`.
> **REPLICATION** is wired to the shared lease → automatic leader failover on a
> CAS-capable coordination bucket (integration-tested against MinIO). Still
> node-local + documented follow-ups: LIFECYCLE, MAINTENANCE, PARITY, admin
> RUN-NOW; and cross-node kill/pause + resume-from-cursor + post-failover run
> history (those need the coordination TABLES shared, a larger change — a
> cross-node takeover currently RESTARTS the run, which is safe-but-wasteful).

Status: DESIGN (2026-07-03). Prereq facts are empirically confirmed (see
`memory/project_backend_cas_support.md`): Hetzner/Ceph enforces atomic
`If-None-Match:*` (HTTP 412 + 1-of-N race); Backblaze B2 rejects conditional
writes with HTTP **501** (loud, not silent-clobber); MinIO enforces (self-IDs via
`Server: MinIO`). This doc decides how DGP consumes those primitives.

## The four questions (answered)

### 1. Do we need hidden coordination directories in data buckets?

**No — and this is the key simplification.** Enumerate the coordination data:

- **Reference-lock CAS** needs **zero** new objects. The lock *is* the
  `reference.bin` write itself (`If-None-Match:*` on create, `If-Match:<etag>` on
  update). No lock file. The scariest data-loss path adds no coordination object.
- **Job leases** (replication/lifecycle/maintenance leader election) DO need a
  persistent CAS object per lease (`{owner, epoch, expires_at}` at a stable key).
  But a lease is **rule-scoped / cluster-scoped, not bucket-scoped** — a
  replication lease spans a source and a dest bucket possibly on *different*
  backends. It has no natural home in any single data bucket.
- **The config DB** already lives as ONE object in the sync bucket, already CAS'd
  (`config_db_sync.rs`).

So the only *new* persistent coordination objects are leases, and they are
cluster-scoped. That kills the "hidden dir in a data bucket" idea on its own.

### 2. The bucket-delete problem (the user's sharp catch)

If coordination objects lived under `<databucket>/.deltaglider/…` and we filtered
`.deltaglider/` from LIST (as we already hide `.dg/` reference dirs), then
`delete_bucket` sees non-empty at the backend and fails `BucketNotEmpty` forever —
the bucket looks empty to the user but never deletes. Special-casing "enumerate
hidden keys, delete, then delete bucket" is fragile and spreads coordination cruft
into every data bucket. **Avoided entirely by (1): coordination doesn't live in
data buckets.** The existing `.deltaglider/*` hydration cache is a DIFFERENT thing
(per-bucket, ephemeral, presigned-GET hydration) and keeps its current hide-from-
list treatment; it is not coordination state and is safe to purge on bucket delete.

### 3. Mandate a coordination BUCKET for HA?

**Yes — and we already have it.** `DGP_CONFIG_SYNC_BUCKET` is that bucket today
(hosts the synced config DB). HA mode extends its role to host leases too. The
bucket MUST be on a CAS-capable backend — which we can now DETECT
(`supports_conditional_writes`, this doc) and REFUSE at startup if it isn't
(B2/501 → hard error "coordination bucket needs conditional-write support").
No hidden dirs, no bucket-delete hazard, no per-bucket cruft.

### 4. HA active vs inactive — ONE path, not two (the important instinct)

The user is right to reject "sqlite path vs object-sync path" as parallel code.
The resolution is a single seam with two impls selected once at startup — the
callers (schedulers, workers, reapers) never branch on HA mode:

```
trait CoordinationStore {
    async fn acquire_lease(rule, owner, ttl) -> Result<Option<Lease>>;
    async fn renew_lease(...) -> Result<bool>;
    async fn release_lease(...);
    async fn reap_expired(...) -> Result<usize>;   // zombie settle
    // …epoch-fencing helpers
}
```

Two impls, ONE consumer contract:
- **No coordination bucket configured** → today's SQLite `job_store` CAS
  (`UPDATE … WHERE leader IS NULL OR expired`). Zero S3 traffic. "HA inactive."
- **Coordination bucket configured** → S3-CAS leases (`If-Match` on a lease
  object + `writer_epoch` fencing, SlateDB-style). "HA active."

This is NOT "two independent paths" — it is one trait with two backends, exactly
like `StorageBackend` has Filesystem + S3. The scheduler code is identical;
`if ha { s3 } else { sqlite }` never appears in business logic.

**The elegant endgame (stage 2, not now):** a `CoordinationStore` built ON TOP of
`StorageBackend` + the conditional-write primitive. Single-instance points it at a
LOCAL FILESYSTEM dir (DGP's filesystem backend already emulates `If-None-Match`
via O_EXCL), HA points it at the shared S3 coordination bucket. Then there is
literally ONE coordination implementation, parameterised by which `StorageBackend`
it writes to — and the SQLite lease tables can be DELETED. That collapses
`config_db_sync.rs`'s IAM-merge-vs-coordination split too. Big change; stage it
after the capability foundation (below) lands and the reference-lock CAS ships.

## What ships FIRST (this turn): the capability foundation

Both the reference-lock fix and the coordination store need the same two
primitives, independent of the larger decision. These are shippable now:

1. **`platform.rs`** — pure fingerprinting: `detect_platform(headers) -> S3Platform`
   from response headers (`Server`, `x-amz-request-id` shape, `x-amz-id-2`
   presence). Empirically grounded (see table). Pure fn → unit-testable, no I/O.
2. **`supports_conditional_writes(bucket)`** — a `StorageBackend` capability gate
   in the `supports_native_multipart` / `lite_list_carries_logical_facts` family.
   Backed by an actual startup PROBE (send one `If-None-Match:*`, observe
   412/200/501), NOT by vendor-inference — because the capability is version-gated
   (old MinIO lacked it). Fingerprint is a HINT + telemetry, the probe is TRUTH.

### Empirical fingerprint table (measured 2026-07-03, not docs)

| Impl | `Server` | `x-amz-request-id` | `x-amz-id-2` | anon `GET /` | cond-write |
|------|----------|--------------------|--------------| ------------|------------|
| MinIO | `MinIO` | 16 UPPER-hex | absent | AccessDenied, 1-line XML `<Resource><HostId>` | ✅ (recent) |
| Ceph RGW (Hetzner) | *(absent)* | `tx…-<zone>-<cluster>` ends e.g. `-ceph4` | absent | `ListAllMyBucketsResult` owner=`anonymous` | ✅ |
| Backblaze B2 | `nginx` | 16 lower-hex | **present** (short b64) | AccessDenied, `standalone="yes"` XML | ❌ 501 |
| AWS S3 | `AmazonS3` | AWS format | present (long b64) | — | ✅ |

(SeaweedFS/Garage/R2/Wasabi rows to be appended from the research pass.)

### Why probe, not infer

Vendor-fingerprinting tells you WHO; capability-probing tells you WHAT WORKS.
Conditional-write support is **version-gated** (MinIO added it in a specific
release; a self-hosted old MinIO answers `Server: MinIO` but 501s the conditional
PUT). Inferring capability from identity would be wrong exactly when it matters.
So: **probe once at startup, cache the result, log the detected platform for
operators.** Fingerprint drives telemetry + a fast-path hint + better error
messages ("this looks like Backblaze B2, which doesn't support conditional
writes"); the probe drives the actual `if supports_xxx` branch.
