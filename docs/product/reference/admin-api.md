# Admin API

*Every endpoint under `/_/api/admin/*`, grouped by purpose.*

The admin UI and GitOps integrations talk to this surface. All mutation routes require a session cookie (issued by `POST /_/api/admin/login`). Sessions are IP-bound — a token is rejected from a different source IP — and default to a 4-hour TTL (`DGP_SESSION_TTL_HOURS`).

Endpoints documented here are **admin** only. The S3-compatible API lives under `/` and is documented by AWS themselves.

## Authentication and session

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/_/api/admin/login` | Bootstrap password → session cookie |
| `POST` | `/_/api/admin/login-as` | Log in as an IAM user (access_key_id + secret_access_key) |
| `POST` | `/_/api/admin/logout` | End the current session |
| `GET` | `/_/api/admin/session` | `{valid, admin_gui}` |
| `POST` | `/_/api/admin/session/browser-connect` | Issue a limited browser-lift session for an IAM non-admin (S3 browse only) |
| `POST` | `/_/api/admin/session/open-browser-connect` | Browser-lift session when `authentication: none` |
| `GET` | `/_/api/whoami` | `{mode, version, user, external_providers}` |
| `POST` | `/_/api/admin/recover-db` | Reset the config DB when the bootstrap hash doesn't match (public, rate-limited) |
| `PUT` | `/_/api/admin/password` | Change the bootstrap password — re-encrypts the SQLCipher DB atomically |

## Configuration — three scopes

All three scopes route through the same `apply_config_transition` path, so hot-reload semantics are identical no matter which level you use.

### Field-level (legacy GUI forms)

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/config` | Runtime config as flat JSON |
| `PUT` | `/_/api/admin/config` | Partial JSON update |

### Section-level

| Method | Path | Body | Purpose |
|---|---|---|---|
| `GET` | `/_/api/admin/config/section/:name[?format=yaml]` | — | Section slice as JSON or YAML |
| `PUT` | `/_/api/admin/config/section/:name` | RFC 7396 JSON Merge Patch | Partial section update |
| `POST` | `/_/api/admin/config/section/:name/validate` | same as PUT | Dry-run: `{ok, warnings[], diff, requires_restart}` |

`:name` ∈ `admission` / `access` / `storage` / `advanced`. Unknown names → 404.

**Merge-patch semantics:** keys missing from the body are preserved; `null` deletes; objects merge recursively. Secrets round-trip (GET → edit → PUT never clears credentials).

### Document-level (GitOps)

| Method | Path | Body | Purpose |
|---|---|---|---|
| `GET` | `/_/api/admin/config/export[?section=<name>]` | — | Canonical YAML (secrets redacted) |
| `GET` | `/_/api/admin/config/declarative-iam-export` | — | Project current DB IAM into `access:` YAML fragment (for declarative GitOps seeding; see [declarative-iam.md](declarative-iam.md#workflow-a-already-populated-db--gitops)) |
| `POST` | `/_/api/admin/config/declarative-iam-validate` | `{yaml: <access fragment>}` | Dry-run the declarative IAM reconcile (`diff_iam` preview, zero DB writes) |
| `POST` | `/_/api/admin/config/declarative-iam-apply` | `{yaml: <access fragment>}` | Atomic single-transaction IAM reconcile from the YAML fragment |
| `GET` | `/_/api/admin/config/defaults[?section=<name>]` | — | JSON Schema (for YAML LSP and Monaco) |
| `POST` | `/_/api/admin/config/validate` | `{yaml: <doc>}` | Dry-run full-document apply |
| `POST` | `/_/api/admin/config/section/:name/validate` | `{<section-body>}` | Dry-run section apply; in declarative mode warns with `diff_iam` preview (see [declarative-iam.md](declarative-iam.md#preview-before-applying)) |
| `POST` | `/_/api/admin/config/apply` | `{yaml: <doc>}` | Atomic full-document apply + persist |
| `POST` | `/_/api/admin/config/trace` | synthetic request body | Evaluate against the admission chain |
| `GET` | `/_/api/admin/config/trace?method=&path=&...` | — | Query-param variant (bookmarkable trace URLs) |
| `POST` | `/_/api/admin/config/sync-now` | — | Force an immediate config-DB pull from the sync bucket |

Full-document apply returns `{applied, persisted, requires_restart, warnings, persisted_path}`. **Persist failure returns HTTP 500**, not 200+warning — GitOps pipelines can't mistake a half-applied state for a clean success.

CLI wrapper:

```bash
export DGP_BOOTSTRAP_PASSWORD=...
deltaglider_proxy config apply deltaglider_proxy.yaml --server https://dgp.example.com
```

## Backends

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/backends` | List named backends |
| `POST` | `/_/api/admin/backends` | Create; validates S3 creds upfront |
| `DELETE` | `/_/api/admin/backends/:name` | Remove — refuses to delete the default or in-use backends |
| `POST` | `/_/api/admin/test-s3` | Test an arbitrary S3 connection without persisting |
| `GET` / `POST` | `/_/api/admin/buckets` | List bucket origins / create a bucket on a backend |
| `POST` | `/_/api/admin/buckets/:bucket/migrate` | Move a bucket's data to another backend as a durable, write-gated job — see [Jobs](#jobs--one-surface-for-everything-background) |

## IAM (gated by `iam_mode`)

`POST`/`PUT`/`DELETE` return `403 { "error": "iam_declarative" }` when `access.iam_mode: declarative`. Reads stay open for diagnostics.

| Method | Path | Purpose |
|---|---|---|
| `GET` / `POST` | `/_/api/admin/users` | List / create |
| `PUT` / `DELETE` | `/_/api/admin/users/:id` | Update / delete |
| `POST` | `/_/api/admin/users/:id/rotate-keys` | Rotate access keys |
| `POST` | `/_/api/admin/users/:id/clone` | Clone a user (new keys, copied permissions) |
| `GET` / `POST` | `/_/api/admin/groups` | List / create |
| `PUT` / `DELETE` | `/_/api/admin/groups/:id` | Update / delete |
| `POST` | `/_/api/admin/groups/:id/clone` | Clone a group |
| `POST` | `/_/api/admin/groups/:id/members` | Add user to group |
| `DELETE` | `/_/api/admin/groups/:id/members/:user_id` | Remove user from group |
| `GET` | `/_/api/admin/iam/version` | Monotonic IAM-index rebuild counter for deterministic diagnostics/tests |
| `GET` | `/_/api/admin/policies` | List canned policy templates (public, no session) |

## External auth (OAuth / OIDC)

| Method | Path | Purpose |
|---|---|---|
| `GET` / `POST` | `/_/api/admin/ext-auth/providers` | List / create |
| `PUT` / `DELETE` | `/_/api/admin/ext-auth/providers/:id` | Update / delete |
| `POST` | `/_/api/admin/ext-auth/providers/:id/test` | Probe the `.well-known` endpoint |
| `GET` / `POST` | `/_/api/admin/ext-auth/mappings` | List / create group mapping rules |
| `PUT` / `DELETE` | `/_/api/admin/ext-auth/mappings/:id` | Update / delete |
| `POST` | `/_/api/admin/ext-auth/mappings/preview` | Preview which groups a given identity would be assigned |
| `GET` | `/_/api/admin/ext-auth/identities` | List external identities (read-only, not gated) |
| `POST` | `/_/api/admin/ext-auth/sync-memberships` | Re-evaluate mapping rules and sync group memberships |
| `GET` | `/_/api/admin/ext-auth/version` | Monotonic external-auth rebuild counter (sibling of `iam/version`) for deterministic diagnostics/tests |
| `POST` | `/_/api/admin/migrate` | Migrate legacy bootstrap creds into an IAM user |

### OAuth redirect flow (public, no session)

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/oauth/authorize/:provider` | Kick off OAuth (PKCE, state, nonce) |
| `GET` | `/_/api/admin/oauth/callback` | Provider callback → issue session cookie |

## Full Backup (Wave 11.1)

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/backup` | Export zip (manifest + config + IAM + secrets) |
| `POST` | `/_/api/admin/backup` | Import — atomic; all parts sha256-verified before any state change. Gated by `iam_mode`. |

Response on `POST` carries per-resource counters:
`{users_created, users_skipped, groups_created, groups_skipped, memberships_created, external_identities_created, external_identities_skipped}`.

`external_identities` are remapped through the imported user + provider ID maps. Orphaned records (user or provider didn't import) are dropped with a WARN log.

Legacy JSON-only import path is still supported for pre-v0.8.4 scripts.

## Diagnostics and usage

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/_/api/admin/usage/scan` | Trigger a prefix-size scan |
| `GET` | `/_/api/admin/usage` | Read the cached usage tree |
| `GET` | `/_/api/admin/deltaspace/savings` | Per-prefix reference-aware delta savings (30s in-memory cache) |
| `GET` | `/_/api/admin/diagnostics/delta-efficiency` | Cached delta-efficiency report for a bucket's deltaspaces |
| `POST` | `/_/api/admin/diagnostics/delta-efficiency/scan` | Trigger a delta-efficiency scan |
| `POST` | `/_/api/admin/diagnostics/delta-efficiency/verify` | Verify reconstructed objects against stored deltas |
| `GET` | `/_/api/admin/diagnostics/scan[/status]` | Integrity-scan status (per-bucket or all-buckets map) |
| `POST` | `/_/api/admin/diagnostics/scan/start` / `/stop` | Start / stop a background integrity scan |
| `GET` | `/_/api/admin/diagnostics/scan/stream` | SSE stream of live scan progress |
| `GET` | `/_/api/admin/audit[?limit=N]` | Snapshot of the in-memory audit ring, newest first. Bounded (default 500, override `DGP_AUDIT_RING_SIZE`). Stdout `tracing::info!` is still the long-term audit source. |
| `GET` | `/_/api/admin/event-outbox[?status=failed&limit=N&offset=N&sort=occurred_at&order=desc]` | Paged durable object-event outbox rows plus status counts. Delivery is background-only; delivered rows default to 24h/10,000-row retention; see [event-outbox.md](event-outbox.md). |
| `POST` | `/_/api/admin/event-outbox/:id/requeue` | Requeue a single failed outbox row for re-delivery |
| `POST` | `/_/api/admin/event-outbox/requeue` | Bulk-requeue failed outbox rows |
| `GET` / `PUT` / `DELETE` | `/_/api/admin/session/s3-credentials` | Per-session S3 credential store for the browse panel |

## Object operations (browse panel)

Server-side helpers behind the embedded S3 browser's bulk actions.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/objects/list` | List all keys under a bucket/prefix |
| `POST` | `/_/api/admin/objects/copy` | Server-side copy of selected objects |
| `POST` | `/_/api/admin/objects/move` | Server-side move (copy + delete) |
| `POST` | `/_/api/admin/objects/delete` | Bulk delete selected objects |
| `GET` | `/_/api/admin/objects/zip` | Stream selected objects as a ZIP |

## Jobs — one surface for everything background

Replication rules, lifecycle rules, and one-off maintenance jobs (re-encrypt,
bucket migration) share a single read+action API. Job ids are namespaced:
`replication:<rule>`, `lifecycle:<rule>`, `maintenance:<n>`. Rules stay
YAML-authoritative under `storage.replication.rules[]` / `storage.lifecycle.rules[]`;
maintenance one-offs are DB-born. See [replication.md](replication.md) and
[lifecycle.md](lifecycle.md) for rule shapes and guardrails.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/jobs` | Every job as one normalized row: kind, scope, status (`idle` / `queued` / `running` / `cancelling` / `succeeded` / `failed` / `cancelled`), pause flag, progress, last run. |
| `GET` | `/_/api/admin/jobs/:id/runs?limit=N` | Recent runs, newest first. A maintenance one-off synthesizes a single run — the job IS its run. |
| `GET` | `/_/api/admin/jobs/:id/failures?limit=N` | Recent per-object failures, newest first. |
| `POST` | `/_/api/admin/jobs/:id/pause` / `/resume` | Replication and lifecycle rules. Persists across restarts. |
| `POST` | `/_/api/admin/jobs/:id/run-now` | Replication and lifecycle rules. Synchronous; 409 when paused or already leased. |
| `POST` | `/_/api/admin/jobs/:id/preview` | Lifecycle only — dry-run candidate keys. Read-only: no deletes, no history rows. |
| `POST` | `/_/api/admin/jobs/:id/cancel` | Maintenance only — cancel a queued or running one-off. A pre-flip migrate cancel unwinds cleanly. |
| `POST` | `/_/api/admin/jobs/reencrypt` | `{"buckets": [...]}` (max 100) → one durable re-encrypt job per bucket: `{started: [{bucket, job_id}], errors: [...]}`. |
| `POST` | `/_/api/admin/buckets/:bucket/migrate` | `{"target_backend": "...", "delete_source": false}` → `202 Accepted` + `{job_id, id: "maintenance:<n>", bucket, from_backend, to_backend}`. |
| `GET` | `/_/api/admin/jobs/bucket/:bucket` | The bucket's active maintenance job, if any — status/phase/counts only, no config detail. Session-light: browser-lift sessions can read it (powers the busy banner in the object browser). |

Actions outside a kind's capability matrix return `405` with the supported
list. Lifecycle preview is intentionally read-only; scheduler and run-now
executions persist history/failure rows in the config DB and use per-rule
leases so instances sharing the DB never double-execute.

**Write gate:** while a re-encrypt or migrate job is active, S3 **writes** to
that bucket return `503 SlowDown` (SDKs back off and retry); reads pass
untouched. The gate engages at job creation and lifts when the job finishes
(for migrations, the moment the bucket flips to the new backend).

## Resource limits (env vars)

| Variable | Default | Purpose |
|---|---|---|
| `DGP_MAX_OBJECT_SIZE` | `100 MiB` | Largest single object (and, per upload, largest multipart upload). |
| `DGP_MAX_MULTIPART_UPLOADS` | `1000` | Maximum concurrent multipart uploads across the proxy. |
| `DGP_MAX_TOTAL_MULTIPART_BYTES` | `max_object_size × max_uploads / 4` | Global in-flight byte cap across all multipart uploads. Protects against the C3 DoS pattern where many uploads accumulate without completing. Reject with `SlowDown` when exceeded. |
| `DGP_MULTIPART_IDLE_TTL_HOURS` | `24` | Idle-TTL for incomplete multipart uploads. The periodic sweeper drops uploads with no UploadPart activity for this long (excluding uploads currently being completed). |
| `DGP_AUDIT_RING_SIZE` | `500` | In-memory audit ring capacity. |
| `DGP_SESSION_TTL_HOURS` | `4` | Admin session cookie lifetime. |

## Keyboard shortcuts (app-wide)

Reachable via `?` anywhere in the UI (when focus is not in an input). `⌘` is `Ctrl` on non-Apple platforms.

| Key | Scope | Action |
|---|---|---|
| `⌘,` | Global | Open Settings |
| `⌘/` | Global | Open Docs |
| `?` | Global | This shortcuts reference |
| `↑` / `↓` | Object browser | Move between objects and folders |
| `Enter` / `→` | Object browser | Open folder / inspect object |
| `←` / `Backspace` | Object browser | Go up one folder |
| `Home` / `End` | Object browser | Jump to first / last row |
| `Esc` | Object browser | Close inspector, or go up one folder |
| `⌘K` | Settings | Command palette (fuzzy nav + shell actions) |
| `⌘S` | Settings | Apply the currently-visible dirty section |
| `↑` / `↓` + `Enter`, `Esc` | Palette | Navigate / run / close |

## Operational endpoints (no admin prefix)

Unauthenticated — needed for load-balancer probes and Prometheus:

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/health` | `{status, peak_rss_bytes, cache_*}` — no version (anti-fingerprinting) |
| `GET` | `/_/metrics` | Prometheus text format |

Session-protected (reveals per-bucket sizes):

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/stats` | Aggregate storage stats, 10s server-side cache |
