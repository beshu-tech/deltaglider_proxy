# Admin API

*Every endpoint under `/_/api/admin/*`, grouped by purpose.*

The admin UI and GitOps integrations talk to this surface. All mutation routes require a session cookie (issued by `POST /_/api/admin/login`). Sessions are IP-bound: the proxy rejects a token that comes from a different source IP. Sessions default to a 4-hour TTL (`DGP_SESSION_TTL_HOURS`).

A browser marks each request with a `Sec-Fetch-Site` header, and it adds an `Origin` header to cross-origin requests. The proxy refuses, with `403 cross_origin_request`, every `POST`, `PUT`, `PATCH` or `DELETE` under `/_/` that such a header marks as coming from another origin. The session cookie is `SameSite=Strict`, but that attribute does not stop a page on a sibling subdomain, because such a page belongs to the same site. A client that sends neither header, such as `curl` or `config apply`, is not a browser page, so the check lets it through. Development mode (`DGP_CORS_PERMISSIVE=true`) turns the check off.

A request body or query string that the proxy cannot accept gets `400` with a JSON body `{"error": <code>, "message": <text>}`. The message names the field and the rule that the value breaks, for example `dest_prefix: invalid path "../x/": '.' and '..' segments are not allowed`. The code is `invalid_path` for an object key or prefix with a `.` or `..` segment, a leading `/` or a NUL character, `invalid_bucket` for a bucket name that breaks the S3 naming rules, and `invalid_request` for every other bad input, such as a missing field or a body that is not valid JSON. A body sent without the `application/json` content type gets `415` with the same JSON shape.

An endpoint that needs the encrypted config DB (for example IAM users, groups, external auth, backup, declarative IAM, the event log, and the job actions) answers `503` with the message `config DB not available` when the instance has no config DB open.

This page documents only the admin endpoints. The S3-compatible API lives under `/`, and the AWS S3 documentation describes it.

## Authentication and session

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/_/api/admin/login` | Bootstrap password → session cookie |
| `POST` | `/_/api/admin/login-as` | Log in as an IAM user (access_key_id + secret_access_key) |
| `POST` | `/_/api/admin/logout` | End the current session |
| `GET` | `/_/api/admin/session` | `{valid, admin_gui}` |
| `POST` | `/_/api/admin/session/browser-connect` | Issue a limited browser-lift session for an IAM non-admin (S3 browse only) |
| `POST` | `/_/api/admin/session/open-browser-connect` | Browser-lift session when `authentication: none` |
| `GET` | `/_/api/whoami` | `{mode, user, external_providers, version, build_time}`. `mode` is `bootstrap`, `iam`, `open`, or `deny_all` (no IAM user, no bootstrap pair and no `authentication: none`: S3 refuses every request, and only the admin password signs in). `version` and `build_time` are present for authenticated callers only (a live session, or verified IAM credentials on `POST /_/api/iam/identity`); anonymous callers get `mode` and the provider list but nothing that identifies the build |
| `GET` | `/_/api/docs` | Session-gated: every product doc plus `docs/product/manifest.json` as one JSON payload (`{manifest, docs: [{path, content}]}`). The embedded docs viewer fetches it at runtime; the markdown is not part of the public JS bundle |
| `POST` | `/_/api/admin/recover-db` | Only while the config DB is locked (no key opens it): test a candidate config DB key, or a legacy bootstrap hash, against the preserved `.db.bak`. Read-only; the response names the kind of key (`key_kind`) so that the operator can set it and restart (public, rate-limited) |
| `PUT` | `/_/api/admin/password` | Change the bootstrap password. The config DB is not re-encrypted: its key does not depend on the password |

## Configuration: three scopes

All three scopes route through the same `apply_config_transition` path, so hot-reload semantics are identical no matter which level you use.

### Field-level (legacy GUI forms)

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/config` | Runtime config as flat JSON. It also carries `env_overrides`, `config_file_path` (the file that an apply writes) and `config_file_writable` (`false` when the proxy cannot write that file, for example a read-only mount) |
| `PUT` | `/_/api/admin/config` | Partial JSON update |
| `DELETE` | `/_/api/admin/config/bootstrap-credentials` | Remove the bootstrap SigV4 pair from the config: `{removed, warnings}` |

The bootstrap `access_key_id` is an identifier, not a secret, so every GET and export shows it; `secret_access_key` stays redacted. An unchanged `access_key_id` with no secret in a PUT or apply keeps the secret. A change that leaves no credential (no IAM user, no bootstrap pair, no `authentication: none`) is refused, because the proxy would then refuse every S3 request: `DELETE …/bootstrap-credentials` and the config writes answer `409`, and the field-level PUT keeps the pair and returns a warning. `DELETE /users/:id` of the last IAM user answers `409` for the same reason. The DELETE also answers `409` when `DGP_ACCESS_KEY_ID` or `DGP_SECRET_ACCESS_KEY` sets the pair. When the key id is also an IAM user (the first IAM user carries the pair over as `legacy-admin`), the warnings say so: that user still signs requests until you delete or disable it.

### Section-level

| Method | Path | Body | Purpose |
|---|---|---|---|
| `GET` | `/_/api/admin/config/section/:name[?format=yaml]` | — | Section slice as JSON or YAML |
| `PUT` | `/_/api/admin/config/section/:name` | RFC 7396 JSON Merge Patch | Partial section update |
| `POST` | `/_/api/admin/config/section/:name/validate` | same as PUT | Dry-run: `{ok, warnings[], existing_warnings[], diff, requires_restart, restart_reasons[]}`. `warnings` holds only the warnings this change introduces; `existing_warnings` holds the warnings the current config already produces. `restart_reasons` has one line for each changed field that takes effect only after a restart. The section PUT response splits the warnings the same way and carries `restart_reasons` too. |

A section PUT that applies the change in memory but cannot write the config file answers `500` with `ok: true`, `persist_error` (`<path>: <error>`) and a warning, because the change is lost at the next restart.

`:name` ∈ `admission` / `access` / `storage` / `advanced`. Unknown names → 404.

**Merge-patch semantics:** keys missing from the body are preserved; `null` deletes; objects merge recursively. Secrets round-trip (GET → edit → PUT never clears credentials).

**Optimistic concurrency.** The section GET returns the version of that section in an `ETag` header. Send it back in an `If-Match` header on the section PUT. When the section changed after you read it (another browser tab, another admin, or a GitOps apply), the PUT does not apply, and the proxy answers `409 Conflict` with `{ok: false, error: "config_conflict: …", current_version}` and the current version in `ETag`. A successful PUT returns the section's new version in `ETag`. A PUT without `If-Match` is not checked, so scripts keep working. The version of one section does not change when another section changes. `GET /_/api/admin/config/export` and `GET /_/api/admin/config` return the version of the whole document (or of the one section, with `?section=`), and `POST /_/api/admin/config/apply` and `PUT /_/api/admin/config` check `If-Match` against the whole document in the same way. A version is a keyed hash of the running config: it changes with every change, from any source. The one exception is the bootstrap password hash: no config editor can change it, so a change through `PUT /_/api/admin/password` does not change a version, and open editors do not get a `409` after it. The hash key is derived from the config DB key (never the key itself), so a version reveals nothing about secret values, it stays the same across a restart, and instances that share the config DB key agree on it. The admin GUI sends `If-Match` on every section apply; on a `409` it keeps your edits and offers to reload the section or to review your edits against the new version.

### Document-level (GitOps)

| Method | Path | Body | Purpose |
|---|---|---|---|
| `GET` | `/_/api/admin/config/export[?section=<name>]` | — | Canonical YAML (secrets redacted) |
| `GET` | `/_/api/admin/config/declarative-iam-export` | — | Project current DB IAM into `access:` YAML fragment (for declarative GitOps seeding; see [declarative-iam.md](declarative-iam.md)) |
| `POST` | `/_/api/admin/config/declarative-iam-validate` | `{yaml: <access fragment>}` | Dry-run the declarative IAM reconcile (`diff_iam` preview, zero DB writes) |
| `POST` | `/_/api/admin/config/declarative-iam-apply` | `{yaml: <access fragment>}` | Atomic single-transaction IAM reconcile from the YAML fragment |
| `GET` | `/_/api/admin/config/defaults[?section=<name>]` | — | JSON Schema (for YAML LSP and Monaco) |
| `POST` | `/_/api/admin/config/validate` | `{yaml: <doc>}` | Dry-run full-document apply: the same checks as `/config/apply` (including the `403` for a changed `bootstrap_password_hash`), without the If-Match check and without any state change |
| `POST` | `/_/api/admin/config/section/:name/validate` | `{<section-body>}` | Dry-run section apply; in declarative mode warns with `diff_iam` preview (see [declarative-iam.md](declarative-iam.md)) |
| `POST` | `/_/api/admin/config/apply` | `{yaml: <doc>}` | Atomic full-document apply + persist |
| `POST` | `/_/api/admin/config/trace` | synthetic request body | Evaluate against the admission chain |
| `GET` | `/_/api/admin/config/trace?method=&path=&...` | — | Query-param variant (bookmarkable trace URLs) |
| `POST` | `/_/api/admin/config/sync-now` | — | Force an immediate config-DB pull from the sync bucket. `200` = current, `409` = a newer copy was not merged (the body says why), `502` = the bucket cannot be read, `404` = no sync bucket |
| `GET` | `/_/api/admin/config/sync` | — | This instance's sync state: `healthy`, `last_pull_ok_at`, `last_push_ok_at`, `pull_error`, `push_error`, `last_error_at`, `pending_upload`, `base_present`, `sync_generation`. `404` = no sync bucket |

Full-document apply returns `{applied, persisted, requires_restart, warnings, existing_warnings, persisted_path}`, and `persist_error` (`<path>: <error>`) when the file write failed. Full-document validate returns `{ok, warnings, existing_warnings}`. When validate refuses a document, it returns `ok: false` with the reason in `error`, and it still returns the same warnings that `/config/apply` returns for that document. As on the section endpoints, `warnings` holds only the warnings the document introduces, and `existing_warnings` holds the warnings the running config already produces. The field-level `PUT /_/api/admin/config` returns only warnings about its own change. A bucket policy that the YAML loader would refuse, for example `public: true` beside non-empty `public_prefixes`, makes that PUT answer `400` with `{success: false, error}`, and nothing changes. A persist failure returns HTTP 500 instead of 200 with a warning, so a GitOps pipeline cannot mistake a half-applied state for a clean success.

**Environment variables win.** A `DGP_*` environment variable overrides its field at startup and again after every apply. `GET /_/api/admin/config` lists these fields in `env_overrides` (`{env, yaml_path, secret, value, set, activated_by}`; a secret never carries its value). Some variables control a whole block: `DGP_S3_ENDPOINT` or `DGP_S3_REGION` controls all of `storage.backend`, and `DGP_TLS_ENABLED=true` controls all of `advanced.tls`. A block member whose own variable is unset has `set: false`, and `activated_by` names the variable that controls the block. The config file and every export hold the value the file had, never the environment value. When an apply changes an env-controlled field, the new value is saved to the file, the environment value stays in effect, and the response carries a warning that says so. The comparison is per field, also inside a block such as `storage.backend`: a field that still holds the environment value keeps the file's value, so changing one field never writes the other environment values into the file. When an edit copies a secret environment value into another field (for example, turning off encryption moves an environment-provided key to `legacy_key`), the file stores the reference `${env:NAME}` instead of the value, and the response says so. As a last check, persist and export refuse to write any value of a secret environment variable that the config file did not already hold. Full-document validate runs the same steps as apply, so its warnings match.

**`${env:NAME}` references in a request body.** Full-document apply and validate, and every section PUT, resolve a `${env:NAME}` reference only when the config file that the server loaded at startup already uses the same name. The server takes the value that it recorded at that load. It never reads any other environment variable for an admin request, because an admin could otherwise read every secret of the proxy process, for example through an error message that quotes a field value. A reference to a name that the file does not use takes its `:-default` value; without a default, the request fails. Error messages and warnings show `${env:NAME}` in place of a resolved value. To use a new variable, reference it in the config file on disk, or run `config apply`, which expands references against the operator's environment before it sends the document. An operator can also list extra names in `DGP_CONFIG_ENV_ALLOWLIST` (comma-separated, and a trailing `*` matches every name with that prefix). The server then resolves those names from its own environment for admin requests too. This is useful when you import an export from another instance on a new host that sets the same variables. The allowlist never admits a `DGP_*` variable whose name contains `BOOTSTRAP_`, `ENCRYPTION_KEY`, `SECRET`, `DB_KEY`, `PASSWORD` or `TOKEN` (for example `DGP_CONFIG_DB_KEY` or `DGP_METRICS_BEARER_TOKEN`), because those variables hold the proxy's own secrets. A trailing `*` never matches a `DGP_*` variable, so a proxy variable must be listed by its exact name. A full-backup restore is the one other case: when this host's environment already supplies a secret that the backup's `secrets.json` holds (for example the AES key in `DGP_BACKEND_<NAME>_ENCRYPTION_KEY`), the restored file stores the reference `${env:NAME}` in place of the value, and the restore resolves exactly that reference.

CLI wrapper:

```bash
export DGP_BOOTSTRAP_PASSWORD=...
deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example
```

## Backends

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/backends` | List named backends |
| `POST` | `/_/api/admin/backends` | Create; validates S3 creds upfront |
| `DELETE` | `/_/api/admin/backends/:name` | Remove a backend. The proxy refuses to delete the default backend or a backend in use |
| `POST` | `/_/api/admin/backends/:name/probe` | Test connection: probe one backend now, update its health state, and return the result (`:name` is a named backend or `default`) |
| `GET` | `/_/api/admin/backends/:name/legacy-key-usage?limit=N&fresh=true` | Count the objects and delta references that still carry the backend's legacy key id (see below) |
| `POST` | `/_/api/admin/test-s3` | Test an arbitrary S3 connection without persisting |
| `GET` / `POST` | `/_/api/admin/buckets` | List bucket origins / create a bucket on a backend |
| `POST` | `/_/api/admin/buckets/:bucket/migrate` | Move a bucket's data to another backend as a durable, write-gated job. See [Jobs](#jobs--one-surface-for-everything-background) |

`GET /_/api/admin/backends/:name/legacy-key-usage` answers whether the backend's decrypt-only `legacy_key` is still needed. The proxy reads the metadata of every object (one `HEAD` per object) and of every delta reference (one `HEAD` per reference, at most 8 at a time) in the buckets that route to the backend, and counts those stamped with the legacy key id. The count is exact, not sampled. The scan stops after `limit` objects (default 10000, maximum 1000000); it then returns `complete: false`, and the counts are only a lower bound. The proxy keeps each result for 60 seconds, and requests that arrive while a scan runs wait for that same scan instead of starting another one. Send `fresh=true` to scan again, for example after a re-encrypt job finished. The response is `{backend, legacy_key_id, buckets, objects_scanned, objects_under_legacy_key, references_scanned, references_under_legacy_key, examples, errors, complete, limit, safe_to_clear, computed_at}`, where `computed_at` is the time of the scan. `safe_to_clear` is true only when the scan is complete, read every object without an error, and found no object and no delta reference under the legacy key id. `legacy_key_id` is `null` when the backend has no legacy key. `:name` is a named backend or `default` for the singleton backend; an unknown name returns `404`.

## IAM (gated by `iam_mode`)

`POST`/`PUT`/`DELETE` return `403 { "error": "iam_declarative" }` when `access.iam_mode: declarative`. Reads stay open for diagnostics.

| Method | Path | Purpose |
|---|---|---|
| `GET` / `POST` | `/_/api/admin/users` | List / create |
| `PUT` / `DELETE` | `/_/api/admin/users/:id` | Update / delete (`409` for the last user when no other credential is left) |
| `POST` | `/_/api/admin/users/:id/rotate-keys` | Rotate access keys |
| `POST` | `/_/api/admin/users/:id/clone` | Clone a user (new keys, copied permissions) |
| `GET` / `POST` | `/_/api/admin/groups` | List / create |
| `PUT` / `DELETE` | `/_/api/admin/groups/:id` | Update / delete |
| `POST` | `/_/api/admin/groups/:id/clone` | Clone a group |
| `POST` | `/_/api/admin/groups/:id/members` | Add user to group |
| `DELETE` | `/_/api/admin/groups/:id/members/:user_id` | Remove user from group |
| `GET` | `/_/api/admin/iam/version` | Monotonic IAM-index rebuild counter for deterministic diagnostics/tests |
| `GET` | `/_/api/admin/policies` | List canned policy templates (any live session; was public before the fingerprint hardening) |

## External auth (OAuth / OIDC)

| Method | Path | Purpose |
|---|---|---|
| `GET` / `POST` | `/_/api/admin/ext-auth/providers` | List / create (`422` with `{error}` when the issuer URL or `extra_config` is invalid) |
| `PUT` / `DELETE` | `/_/api/admin/ext-auth/providers/:id` | Update / delete |
| `POST` | `/_/api/admin/ext-auth/providers/:id/test` | Test a saved provider: fetch its `.well-known` discovery document (not gated) |
| `POST` | `/_/api/admin/ext-auth/providers/test` | Test a provider that is not saved yet; the body is the provider form (not gated) |
| `GET` / `POST` | `/_/api/admin/ext-auth/mappings` | List / create group mapping rules |
| `PUT` / `DELETE` | `/_/api/admin/ext-auth/mappings/:id` | Update / delete |
| `POST` | `/_/api/admin/ext-auth/mappings/preview` | Preview which groups a given identity would be assigned |
| `GET` | `/_/api/admin/ext-auth/identities` | List external identities (read-only, not gated) |
| `POST` | `/_/api/admin/ext-auth/sync-memberships` | Re-evaluate mapping rules and sync group memberships |
| `GET` | `/_/api/admin/ext-auth/version` | Monotonic external-auth rebuild counter (sibling of `iam/version`) for deterministic diagnostics/tests |
| `POST` | `/_/api/admin/migrate` | Migrate legacy bootstrap creds into an IAM user |

No response carries a provider's `client_secret`: it reads `****`. An OIDC provider's `extra_config` takes `allow_local` and `ca_cert_path` for an identity provider in a private network (see [How to set up SSO](../how-to/set-up-sso.md#an-identity-provider-in-a-private-network)).

A Test Connection request may carry a JSON body with the fields of the provider form (`client_id`, `client_secret`, `issuer_url`, `scopes`, `extra_config`). For a saved provider, each field in the body replaces the saved value for this test only, and the proxy saves nothing. A blank or absent `client_secret` keeps the saved secret, because the form never shows it. The answer is `200` with `{success, issuer, authorization_endpoint, error}`. A failed test, for example an unreachable issuer or a form without a client ID, has `success: false` and names the cause in `error`. An unknown provider id returns `404`.

### OAuth redirect flow (public, no session)

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/oauth/authorize/:provider` | Kick off OAuth (PKCE, state, nonce) |
| `GET` | `/_/api/admin/oauth/callback` | Provider callback → issue session cookie |

## Full Backup

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/backup` | Export zip (manifest + config + IAM + secrets) |
| `POST` | `/_/api/admin/backup[?mode=&iam=]` | Import. The import is atomic: the proxy verifies the sha256 of every part before any state change. With `access.iam_mode: declarative`, the import answers `403` (`iam_declarative`). |

`mode` selects what a zip restore applies: `full` (the default: configuration, secrets, IAM and the admin password), `preserve-bootstrap` (the same, but the admin password of this instance stays), `config-only` (configuration and secrets; the admin password of this instance stays) or `iam-only` (IAM only). `iam` selects how the IAM rows are restored: `replace` (the default) deletes the users, groups, OIDC providers, mapping rules and external identities that the backup does not hold, in one database transaction, and `merge` adds only the rows that the instance does not have. A restore that includes the configuration (every mode except `iam-only`) must be able to save the config file, so on a read-only config file it answers `409` with `stage: "config_file_read_only"` before anything changes. See [How to back up and restore](../how-to/back-up-and-restore.md#restore-a-full-backup).

Response on `POST` carries per-resource counters:
`{users_created, users_skipped, users_renamed, users_deleted, groups_created, groups_skipped, groups_deleted, memberships_created, external_identities_created, external_identities_skipped}`. `users_deleted` and `groups_deleted` count the rows that a `replace` restore removed. `users_renamed` lists the users that the restore imported under a new name, because another user already had the name.

`external_identities` are remapped through the imported user + provider ID maps. Orphaned records (user or provider didn't import) are dropped with a WARN log.

Legacy JSON-only import path is still supported for pre-v0.8.4 scripts.

## Diagnostics and usage

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/_/api/admin/usage/scan` | Trigger a prefix-size scan. The scan lists at most 100,000 keys (one LIST request per 1000 keys) and then returns `truncated: true`. At most two of these scans list at the same time; the others wait for a free slot. |
| `GET` | `/_/api/admin/usage` | Read the cached usage tree |
| `GET` | `/_/api/admin/usage/bucket/:bucket` | O(1) running usage counter for one bucket: `{object_count, logical_bytes, stored_bytes, savings_percentage, last_scan_at, never_scanned}`. The proxy updates it inline on every PUT and DELETE, without a scan. The counter is per-instance, so it is approximate across a fleet. When the proxy cannot read the object that a write replaces (a timeout or a `503` from the backend), it does not count that write and clears `last_scan_at`, so `never_scanned` is `true` until the next Refresh. DeleteBucket removes the counter of the bucket. |
| `POST` | `/_/api/admin/usage/refresh?bucket=X` | Full scan of the bucket that overwrites the counter with ground truth; it reconciles drift or seeds a never-scanned bucket. The scan is the bucket scan of `diagnostics/scan`: when a scan of the bucket already runs, the request waits for that scan instead of starting a second one, and `diagnostics/scan/stop` cancels it. The proxy reads the logical sizes from the listing and sends a `HEAD` only for an object whose size the listing does not tell. When such a `HEAD` fails, the result is an estimate: the counter keeps its value, and the answer carries `estimated: true` with the scan's numbers in `estimate`. Returns the counter row with `estimated`. |
| `GET` | `/_/api/admin/deltaspace/savings` | Per-prefix reference-aware delta savings, cached in memory for 5 minutes. One listing of the prefix gives the objects and the `reference.bin` baselines, and the proxy sends no `HEAD`. When the proxy does not know the original size of some objects, they count their stored size and the answer carries `estimated: true`. The listing stops after 100,000 keys with `truncated: true`. |
| `GET` | `/_/api/admin/diagnostics/delta-efficiency` | Cached delta-efficiency report for a bucket's deltaspaces. One listing of the bucket, grouped by folder, builds the report; `min_deltas` filters the cached report, so another value starts no new scan |
| `POST` | `/_/api/admin/diagnostics/delta-efficiency/scan` | Trigger a delta-efficiency scan |
| `POST` | `/_/api/admin/diagnostics/delta-efficiency/verify` | Verify reconstructed objects against stored deltas |
| `GET` | `/_/api/admin/diagnostics/scan[/status]` | Integrity-scan status (per-bucket or all-buckets map) |
| `POST` | `/_/api/admin/diagnostics/scan/start` / `/stop` | Start / stop a background integrity scan |
| `GET` | `/_/api/admin/diagnostics/scan/stream` | SSE stream of live scan progress |
| `DELETE` | `/_/api/admin/diagnostics/scan?bucket=X` | Remove the stored scan result of a bucket, so that the bucket shows as never scanned |
| `GET` | `/_/api/admin/audit[?limit=N]` | Snapshot of the in-memory audit ring, newest first. Bounded (default 500, override `DGP_AUDIT_RING_SIZE`). Stdout `tracing::info!` is still the long-term audit source. |
| `GET` | `/_/api/admin/logs[?level=&target=&q=&limit=N]` | Filtered backlog of the in-memory operational-log ring (INFO+ floor), newest first. Bounded (`DGP_LOG_RING_SIZE`, default 2000). |
| `GET` | `/_/api/admin/logs/stream[?level=&target=&q=]` | Live tail of operational logs via server-sent events, same filters as the backlog. |
| `GET` | `/_/api/admin/event-outbox[?status=failed&limit=N&offset=N&sort=occurred_at&order=desc]` | Paged durable object-event outbox rows plus status counts. Each row carries `deliveries`, its per-endpoint delivery state. Delivery is background-only; delivered rows default to 24h/10,000-row retention; see [event-outbox.md](event-outbox.md). |
| `POST` | `/_/api/admin/event-outbox/:id/requeue` | Requeue a single failed outbox row for re-delivery |
| `POST` | `/_/api/admin/event-outbox/requeue` | Bulk-requeue failed outbox rows |
| `POST` | `/_/api/admin/event-outbox/purge-failed` | Delete the failed outbox rows that no replication consumer still needs: `{purged}`. `409` when event-driven replication may still read a failed row |
| `GET` / `PUT` / `DELETE` | `/_/api/admin/session/s3-credentials` | Per-session S3 credential store for the browse panel |
| `GET` | `/_/api/admin/sessions` | Live sessions on this instance, redacted: `{sessions}`. The caller's own session has `current: true` |
| `DELETE` | `/_/api/admin/sessions/:id` | End one session by its short id. `400` for the caller's own session (use logout), `404` for an unknown id |
| `POST` | `/_/api/admin/sessions/revoke-user` | `{identity}` (an IAM access key id, or `provider:user_id` for an external login): end every session of that identity on this instance, and on the other instances through the config sync |

## Object operations (browse panel)

Server-side helpers behind the embedded S3 browser's bulk actions.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/objects/list` | List all keys under a bucket/prefix |
| `POST` | `/_/api/admin/objects/copy` | Server-side copy of selected objects |
| `POST` | `/_/api/admin/objects/move` | Server-side move (copy + delete) |
| `POST` | `/_/api/admin/objects/delete` | Bulk delete selected objects |
| `GET` | `/_/api/admin/objects/zip` | Stream selected objects as a ZIP |

These endpoints accept an admin GUI session and also the browser session of a user without admin rights (the session that an access-key sign-in in the file browser creates). For an admin session, the session is the authorization boundary. For a user without admin rights, the proxy checks every key against that user's own IAM permissions before it touches the key, with the same rules as the S3 API, including `aws:SourceIp` conditions. A copy needs `read` on the source key and `write` on the destination key; a move needs `delete` on the source key too; a delete needs `delete`; a ZIP needs `read`. A key that the user may not use is not touched: the response lists it under `failures` with an `AccessDenied` error, and the other keys are processed. A ZIP leaves such a key out and names it in the skip report inside the archive; when no selected key may be read, the ZIP answers `403`. A folder listing (`objects/list`) returns only the keys that the user can see, like an S3 `LIST`. It reads the backend in the same way as an S3 `LIST`: the proxy reads only the prefixes that the user's policy can reach, and when it must skip hidden keys, the whole folder listing reads at most `advanced.filtered_list_max_engine_pages` backend pages. A folder that holds more hidden keys than that limit can read makes the request fail with `400`, because a partial folder must not become the selection of a copy, move or delete; select a narrower folder instead. Because a move deletes its sources only when every copy of the request succeeded, one denied key keeps all the sources of that request in place. The file browser sends a copy or a move in requests of at most 500 objects, so a denied key keeps only the sources of its own batch in place. The file browser also stops listing the selected folders as soon as the selection passes 10,000 objects, the limit of one request. In open mode (`authentication: none`) an open browser session may use the endpoints without checks, like the S3 API in open mode.

### `GET /_/api/admin/objects/zip`

| Parameter | Value |
|---|---|
| `bucket` | The bucket that holds every object of the archive, for example `releases`. |
| `prefix` | Optional. A folder that every key is under, for example `builds/v1/`. The proxy puts it in front of each key. The file browser sends the deepest folder that the selected keys share, so that more keys fit in one request. |
| `keys` | A JSON array of keys below `prefix`, at most 10,000, for example `["a.zip","b, final.zip"]`. A JSON array keeps a key that contains a comma intact. The request line must stay below 64 KiB. |

The older form without `bucket` is still accepted in this release: `keys` is then a JSON array, or a comma-separated list, of `bucket/key` entries. A comma-separated list cannot name a key that contains a comma, and an entry without a `/` makes the request fail with `400`.

The response is `200` with `Content-Type: application/zip` and `Content-Disposition: attachment; filename="deltaglider-<date>.zip"`. The proxy streams the archive while it reads the objects, so the response has no `Content-Length` header and no size limit. Each object is read through the same path as an S3 `GET`, so a delta-stored object is reconstructed before its bytes enter the archive. The proxy does not hold a whole object or the whole archive in memory.

The proxy takes one snapshot of its storage configuration when the download starts, and it reads every object of the archive through that snapshot. A configuration change that is applied during the download, such as a changed backend or bucket route, therefore does not reach that archive. The next download uses the new configuration.

The archive uses these format choices:

- Every entry is stored without compression (method `STORE`). Most objects that the proxy holds are already compressed, so compression would cost CPU time and save little space.
- Every entry has a data descriptor, because the proxy computes the CRC-32 of an entry while its bytes pass through.
- An entry of 4 GiB or more, an entry that starts after the first 4 GiB, and an archive with 65,535 entries or more use ZIP64 records. Tools without ZIP64 support cannot open such an archive; Info-ZIP `unzip` 6 and Python's `zipfile` read it.
- Entry names are the keys below the deepest folder that all selected keys share. When the keys come from more than one bucket, each name starts with its bucket. A name that an extractor could place outside its target folder (one that starts with `/`, holds a `\`, or starts with a drive such as `C:`) is percent-escaped: `\` becomes `%5C`, a leading `/` becomes `%2F`, the drive colon becomes `%3A`, and `%` becomes `%25`.

Before the proxy sends the response headers, it checks every key against the caller's permissions and opens the first readable object. When no selected object can be read, the request fails with an error status instead of an empty archive: `404` when every object is missing, `403` when access to any object was denied, `413` when an object is too large to read, `503` when the proxy or the backend sheds load, and `502` for other backend failures. After the headers are sent, an object that cannot be opened is left out and named in a `_deltaglider-skipped-files.txt` entry at the end of the archive. An object that fails after some of its bytes are sent, or that ends with fewer bytes than its size, stops the response before the archive's central directory. The client then sees a failed or incomplete download, and never an archive that looks complete but lacks bytes. When the client closes the connection, the proxy stops at once: it opens and reads no more objects for the archive.

When the download starts, the proxy writes a `bulk_zip` audit entry. Its target names the buckets, the number of selected keys, and the number of keys that the caller's permissions denied. The entry also records the client IP address and User-Agent, like the audit entries of a bulk copy, move, or delete.

## Jobs — one surface for everything background

Replication rules, lifecycle rules, and one-off maintenance jobs (re-encrypt,
bucket migration, metadata backfill) share a single read+action API. Job ids are namespaced:
`replication:<rule>`, `lifecycle:<rule>`, `maintenance:<n>`. Rules stay
YAML-authoritative under `storage.replication.rules[]` / `storage.lifecycle.rules[]`;
maintenance one-offs are DB-born. See [replication.md](replication.md) and
[lifecycle.md](lifecycle.md) for rule shapes and guardrails.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/api/admin/jobs` | Every job as one normalized row: kind, scope, status (`idle` / `queued` / `running` / `cancelling` / `succeeded` / `completed_with_errors` / `failed` / `cancelled`), pause flag, progress, last run. |
| `GET` | `/_/api/admin/jobs/:id/runs?limit=N` | Recent runs, newest first. A maintenance one-off synthesizes a single run, because the job is its own run. |
| `GET` | `/_/api/admin/jobs/:id/failures?limit=N` | Recent per-object failures, newest first. |
| `POST` | `/_/api/admin/jobs/:id/pause` / `/resume` | Replication and lifecycle rules. Persists across restarts. |
| `POST` | `/_/api/admin/jobs/:id/run-now` | Replication and lifecycle rules. Starts the run in the background and answers `202` with `status: "running"`; poll `GET /jobs/:id/runs` for the result. Lifecycle returns the `run_id` of the new run. Replication opens its run row in the background, so its response has `run_id: 0`. Replication runs a disabled or paused rule once. Both answer `409` when the subsystem is disabled globally, when the rule is already running, or when a maintenance job is active on a bucket that the rule writes to. Lifecycle also answers `409` when the rule is disabled or paused. |
| `POST` | `/_/api/admin/jobs/:id/preview` | Lifecycle only: list the candidate keys as a dry run. Read-only: no deletes, no history rows. |
| `POST` | `/_/api/admin/jobs/:id/verify` | Replication only: start a parity audit in the background (`202`). `409` while a replication run of the rule is in progress. |
| `GET` | `/_/api/admin/jobs/:id/verify` | Replication only: the state of the parity audit: `{status, progress_scanned, progress_total, scanned_at, outcome, error}`. |
| `POST` | `/_/api/admin/jobs/:id/verify/cancel` | Replication only: stop a running parity audit. |
| `POST` | `/_/api/admin/jobs/:id/kill` | Replication only: stop the running run of the rule on the instance that runs it (`202`). `409` when no run is in progress. |
| `POST` | `/_/api/admin/jobs/:id/delete` | Replication and lifecycle rules: remove the rule from the config and delete its run state (`204`). `409` while the rule has a run or an audit in progress. |
| `POST` | `/_/api/admin/jobs/:id/cancel` | Maintenance only: cancel a queued or running one-off. A migrate cancel before the flip unwinds cleanly. |
| `POST` | `/_/api/admin/jobs/reencrypt` | `{"buckets": [...]}` (max 100) → one durable re-encrypt job per bucket: `{started: [{bucket, job_id}], errors: [...]}`. |
| `POST` | `/_/api/admin/jobs/backfill-metadata` | `{"buckets": [...], "refresh_last_modified": false}` (max 100) → one durable metadata-backfill job per bucket, with the same response as re-encrypt. See [Jobs](jobs.md#metadata-backfill). |
| `POST` | `/_/api/admin/buckets/:bucket/migrate` | `{"target_backend": "...", "delete_source": false, "target": "empty"}` → `202 Accepted` + `{job_id, id: "maintenance:<n>", bucket, from_backend, to_backend, target}`. `target: "empty"` (default) makes the job fail when the destination bucket already holds objects; `target: "mirror"` deletes the destination objects that the source does not hold. |
| `GET` | `/_/api/admin/jobs/parity-version`, `/_/api/admin/jobs/replication-run-version`, `/_/api/admin/jobs/replication-event-version` | Public monotonic counters (`{version}`), bumped when a parity audit settles, when a scheduled replication run settles, and when event-driven replication advances its cursor. Like `iam/version`, they let tests and tools wait for background work without a fixed sleep. `GET /_/api/admin/usage-scan-version` does the same for completed usage scans. |
| `GET` | `/_/api/admin/jobs/bucket/:bucket` | The bucket's active maintenance job, if any: status, phase and counts only, no config detail. Session-light: browser-lift sessions can read it (the busy banner in the object browser uses it). A session that may not list the bucket gets `403`. |

An action outside a kind's capability matrix returns `400`, and the error names
the actions that the kind supports. An unknown action returns `404`. Lifecycle
preview is read-only on purpose; scheduler and run-now executions persist
history/failure rows in the config DB and take a per-rule lease, so two
workers never run the same rule at the same time. With a coordination bucket,
the replication lease is shared by every instance. The lifecycle and
maintenance leases are node-local: the config sync does not carry them.

**Write gate.** While a re-encrypt, migrate or metadata-backfill job is active, S3 writes to
that bucket return `503 SlowDown` (SDKs back off and retry). Reads pass
untouched. The gate starts when the proxy creates the job and ends when the job finishes.
For a migration, the gate ends at the moment the bucket flips to the new backend.

## Resource limits (env vars)

| Variable | Default | Purpose |
|---|---|---|
| `DGP_MAX_OBJECT_SIZE` | `100 MiB` | Largest single object (and, per upload, largest multipart upload). |
| `DGP_MAX_MULTIPART_UPLOADS` | `1000` | Maximum concurrent multipart uploads across the proxy. |
| `DGP_MAX_TOTAL_MULTIPART_BYTES` | `max_object_size × max_uploads / 4` | Global in-flight byte cap across all multipart uploads. The cap protects against the C3 DoS pattern, in which many uploads accumulate without completing. The proxy rejects a request with `SlowDown` when the cap is exceeded. |
| `DGP_MULTIPART_IDLE_TTL_HOURS` | `24` | Idle-TTL for incomplete multipart uploads. The periodic sweeper drops uploads with no UploadPart activity for this long (excluding uploads currently being completed). |
| `DGP_AUDIT_RING_SIZE` | `500` | In-memory audit ring capacity. |
| `DGP_LOG_RING_SIZE` | `2000` | In-memory operational-log ring capacity (backs the admin Logs viewer). |
| `DGP_LOG_RING_LEVEL` | `info` | Minimum severity captured into the operational-log ring/stream (`error`/`warn`/`info`/`debug`/`trace`). The ring sees only the events that the global log level lets through, so this floor can narrow the capture but not widen it. |
| `DGP_LOG_FORMAT` | `text` | Stdout log format: `text` (human-readable) or `json` (one JSON object per line, `jq`-greppable). Startup-only. |
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

These endpoints need no authentication, because load-balancer probes and Prometheus use them:

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/health` | Liveness: the process answers. No backend I/O, and no version (anti-fingerprinting) |
| `GET` | `/_/ready` | Readiness: probes the storage backend and the config DB; `200 {status:"ready"}` or `503`. Point LB readiness checks here; use `/_/health` for liveness. |
| `GET` | `/_/metrics` | Prometheus text format. When `DGP_METRICS_BEARER_TOKEN` is set, it answers only to `Authorization: Bearer <token>` or to an admin session, and `401` to other requests |

`/_/ready` probes the backend with a bounded, retried `ListBuckets` so a brief provider latency spike doesn't flip readiness to a paging `503`: it reports not-ready only if every attempt fails. Tune it with `DGP_READY_TIMEOUT_SECS` (per attempt, default 3) and `DGP_READY_RETRIES` (extra attempts, default 2). Raise the timeout for a storage provider with a long tail latency. Raise the retries to ride out short blips.

The response also carries `backends`, a map from each backend name to its live health (`healthy`, `unreachable`, `auth-rejected` or `erroring`). The proxy keeps this map current with the periodic health probe (`DGP_BACKEND_HEALTH_INTERVAL_SECS`) and with requests that find a backend unavailable. When every backend in the map is `unreachable` or `auth-rejected`, the node reports `503 not_ready`, because it cannot serve any bucket. When only some backends are down, the node stays ready: the other backends' buckets still work, and every node sees the same outage.

Admin-session-protected (reveals per-bucket sizes; `401` without a live session, `403` for a browser-lift session):

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/_/stats` | Aggregate storage stats from the O(1) per-bucket counter (no scan, no object cap). 10s cache on the all-buckets aggregate; `?bucket=NAME` reads one bucket uncached. |
