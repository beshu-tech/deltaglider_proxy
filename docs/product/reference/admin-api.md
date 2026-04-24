# Admin API reference

*Every endpoint under `/_/api/admin/*`, grouped by purpose.*

The admin UI and GitOps integrations talk to this surface. All mutation routes require a session cookie (issued by `POST /_/api/admin/login`). Sessions are IP-bound — a token is rejected from a different source IP — and default to a 4-hour TTL (`DGP_SESSION_TTL_HOURS`).

Endpoints documented here are **admin** only. The S3-compatible API lives under `/` and is documented by AWS themselves.

## Authentication and session

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/_/api/admin/login` | Bootstrap password → session cookie |
| `POST` | `/_/api/admin/login-as` | Log in as an IAM user (access_key_id + secret_access_key) |
| `POST` | `/_/api/admin/logout` | End the current session |
| `GET` | `/_/api/admin/session` | `{valid: true/false}` |
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
| `GET` | `/_/api/admin/config/defaults[?section=<name>]` | — | JSON Schema (for YAML LSP and Monaco) |
| `POST` | `/_/api/admin/config/validate` | `{yaml: <doc>}` | Dry-run full-document apply |
| `POST` | `/_/api/admin/config/section/:name/validate` | `{<section-body>}` | Dry-run section apply; in declarative mode warns with `diff_iam` preview (see [declarative-iam.md](declarative-iam.md#preview-before-applying)) |
| `POST` | `/_/api/admin/config/apply` | `{yaml: <doc>}` | Atomic full-document apply + persist |
| `POST` | `/_/api/admin/config/trace` | synthetic request body | Evaluate against the admission chain |
| `GET` | `/_/api/admin/config/trace?method=&path=&...` | — | Query-param variant (bookmarkable trace URLs) |

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

## IAM (gated by `iam_mode`)

`POST`/`PUT`/`DELETE` return `403 { "error": "iam_declarative" }` when `access.iam_mode: declarative`. Reads stay open for diagnostics.

| Method | Path | Purpose |
|---|---|---|
| `GET` / `POST` | `/_/api/admin/users` | List / create |
| `PUT` / `DELETE` | `/_/api/admin/users/:id` | Update / delete |
| `POST` | `/_/api/admin/users/:id/rotate-keys` | Rotate access keys |
| `GET` / `POST` | `/_/api/admin/groups` | List / create |
| `PUT` / `DELETE` | `/_/api/admin/groups/:id` | Update / delete |
| `POST` | `/_/api/admin/groups/:id/members` | Add user to group |
| `DELETE` | `/_/api/admin/groups/:id/members/:user_id` | Remove user from group |
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
| `GET` | `/_/api/admin/audit[?limit=N]` | Snapshot of the in-memory audit ring, newest first. Bounded (default 500, override `DGP_AUDIT_RING_SIZE`). Stdout `tracing::info!` is still the long-term audit source. |
| `GET` / `PUT` / `DELETE` | `/_/api/admin/session/s3-credentials` | Per-session S3 credential store for the browse panel |

## Admin GUI keyboard shortcuts

Reachable via `?` in any admin page.

| Key | Action |
|---|---|
| `⌘K` / `Ctrl+K` | Command palette (fuzzy nav + shell actions) |
| `⌘S` / `Ctrl+S` | Apply the currently-visible dirty section |
| `?` | This shortcuts reference |
| `Esc` | Close palette / modal |
| `↑` / `↓` + `Enter` | Navigate + run inside the palette |

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
