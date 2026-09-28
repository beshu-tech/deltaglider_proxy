# Config docs harmonisation plan (2026-09)

This plan answers the owner request of 2026-09-28: task guides must guide the reader through the admin UI with annotated screenshots, then recap the same change as a declarative YAML snippet, and the docs must explain the difference between the two ways. The convention is in [`docs/dev/writing-task-pages.md`](../dev/writing-task-pages.md). The explanation page is [`docs/product/explanation/two-ways-to-configure.md`](../product/explanation/two-ways-to-configure.md). The reference example is [`docs/product/how-to/route-a-bucket-to-a-backend.md`](../product/how-to/route-a-bucket-to-a-backend.md), which is already rewritten.

The audit covers the 3 tutorials and the 30 how-to pages. Among the reference pages, only `reference/authentication.md` has numbered steps that change the configuration (the rotation of the config DB key, through environment variables only), so it is listed as a note in batch D and needs no two-way structure. All UI labels below come from a grep of `demo/s3-browser/ui/src/components/**` in this tree. All routes start with `/_/admin/`. The staged editors share one flow: the bar button **Review & apply** (`StickyDirtyBar`), the dialog "Review changes before applying", and the button **Apply and Persist** (`ApplyDialog`).

## Counts

By today's state of the 33 tutorial and how-to pages, before the rewrite of the reference example:

| Today | Pages |
|---|---|
| YAML-only (YAML, env or CLI; at most a one-line pointer to the UI) | 16 |
| Mixed (YAML and UI steps interleaved) | 12 |
| GUI-only (plus API) | 2 |
| Neither (no configuration step) | 3 |

By target structure:

| Target | Pages |
|---|---|
| UI steps, then "The same change in YAML", then Verify | 18 (one of them, route-a-bucket, is done) |
| YAML-only: say first that the UI cannot, then Verify | 7 |
| UI-only: say that YAML cannot, then Verify | 2 |
| No structural change (no configuration step, or a diagnostics or symptom page; fix stale labels only) | 6 |

## Per-page table

Column "UI can do it" names the route and the exact labels. "Size" is the size of the rewrite: S (under an hour), M (a few hours), L (a day).

| Page | Config steps? | Today | UI can do it? | YAML can do it? | Target | Size | Notes on the current page |
|---|---|---|---|---|---|---|---|
| tutorials/first-delta-savings.md | no (only `DGP_AUTHENTICATION=none` on `docker run`; bucket and upload are S3 through the browser) | neither | n/a | n/a | no change | S | The upload button reads "Upload N file(s) to …", not "Upload". Re-shoot `delta-savings-badge.jpg` as `inspector-delta-savings`. |
| tutorials/kubernetes-hello-world.md | no (helm, kubectl, curl) | neither | n/a | n/a | no change | none | None. |
| tutorials/secure-your-proxy.md | yes | mixed | partial. Step 1 (first password) no: "Change Admin Password" needs "Current admin password". Step 2 yes: `access/credentials`, card "Bootstrap SigV4 credentials" ("Access key ID", "Secret access key"), "S3 authentication mode" ("Auto-detect (recommended)" / "Open access (dev only)"); the radio is read-only while `DGP_AUTHENTICATION` is set. Step 3 yes: `access/users` "New" → "Name", "Access Key ID", "Secret Access Key", rule row (Allow/Deny, "WHERE", "CAN DO") → "Create User" | `access.access_key_id`, `secret_access_key`, `authentication`; the user only in declarative mode | UI steps + YAML recap + Verify (step 1 stays CLI) | M | The page names the fields "Effect/Actions/Resources"; the UI shows Allow/Deny, WHERE and CAN DO. The tutorial must not set `DGP_AUTHENTICATION`, or the UI radio is locked. |
| how-to/backend-capability-validation.md | yes | YAML-only | partial: bucket re-route (`storage/buckets` "Backend"), "Config DB sync" → "Sync bucket" on `system`. The `replication_target_only` marker cannot be set in the UI (BucketCard says "Set or clear the marker in YAML.") | `storage.buckets.<b>.replication_target_only`, `advanced.config_sync_bucket` | YAML-only (say the UI cannot set the marker) + Verify | S | None. |
| how-to/back-up-and-restore.md | yes (a restore changes config and IAM) | GUI-only | yes, `system`: "Download backup", "Restore backup" → modal with the mode choice ("Everything except the admin password" / "Config only" / "Users and groups only" / "Everything, including the admin password"), "Replace (point in time)" / "Merge (keep existing)", "Restore" | no | UI-only + API + Verify | S | "Settings → System → Backup → Export" does not exist. |
| how-to/create-iam-users.md | yes | mixed | yes, except JSON shapes. `access/credentials` "Bootstrap SigV4 credentials"; `access/users` "New" … "Create User"; permission rows with preset pills (no JSON editor); "Duplicate user with fresh credentials"; rotation by editing "Access Key ID"/"Secret Access Key" ("Generate random key", "Generate random secret") and "Save"; `access/groups` "New", "Name", "Description", "Members", "Create Group" | users and groups only in declarative mode | UI steps + YAML recap (declarative alternative) + Verify | M | "untick Enabled" is a switch; "Save" on create is "Create User"; the UI rotation path is not documented. |
| how-to/deploy-on-kubernetes.md | yes (Helm `config.inline`) | YAML-only | no, not at deploy time | yes | YAML-only + Verify | none | None. |
| how-to/deploy-with-docker-compose.md | yes (config file, secret env) | YAML-only | no before boot; the setup wizard `/_/admin/setup` after boot | yes | YAML-only + Verify | S | Add one line that points at the setup wizard. |
| how-to/diagnose-backend-connectivity.md | env only (`DGP_BOOT_BACKEND_PROBE`, `DGP_BACKEND_HEALTH_INTERVAL_SECS`) | YAML-only | the diagnosis is in the UI (`storage/backends` badge, "Test connection"); the env knobs are not | env only | no change | S | Badges read "Connected", "CREDENTIALS REJECTED", "UNREACHABLE", "ERRORING". "Settings → Storage → Backends" is stale. |
| how-to/encrypt-data-at-rest.md | yes | mixed | partial, `storage/backends` card: "Encryption mode" ("None (plaintext)", "AES-256-GCM (proxy-side)", "SSE-KMS (AWS KMS)", "SSE-S3 (AWS-managed AES256)"), "Generated key (64 hex chars, shown ONCE)", "I have stored this key safely…", "Apply"; KMS "KMS key ARN or alias"; proposal "Encrypt existing objects?" → "Start now (N buckets)". `key_id` not editable; a key from an env var is YAML-only | `storage.backends[].encryption.*` | UI steps + YAML recap + Verify | M | Step 2 says to keep the key in an env var, but the UI writes it into the config file: say so. "+ New job" is "New job", and Jobs is under Storage. |
| how-to/expire-and-archive-objects.md | yes | YAML-only | partial, `jobs`: "New job" → "Lifecycle rule — scheduled expiry / archive" → "Rule name", "Enabled", "Scope", "Expire after", "Action" ("Delete" / "Keep newest N" / "Archive / move"), "Destination", "Delete source after copy", "Filters and batch size"; "Preview", "Run now", "Pause"/"Resume". **No UI control sets `storage.lifecycle.enabled`** (default `false`) | `storage.lifecycle.*` | UI steps + YAML recap + Verify, with the master switch stated first as YAML-only | M | Product gap, see decisions. |
| how-to/gate-requests-with-admission-rules.md | yes | mixed | yes, `access/admission`: "Add rule" → modal "Add request rule" ("Name", "HTTP methods", "Source IPs", "Bucket", "Object key pattern", "Signed request", "Config flag", "Then"), "Drag to reorder", "Review & apply" | `admission.blocks` | UI steps + YAML recap + Verify | M | The page says the editor "has a form view and a YAML view"; the modal has only a form. "Settings → Access → Request rules" is stale. |
| how-to/go-to-production.md | yes (checklist) | YAML-only | partial: item 2 `storage/backends` "Add Backend"; item 3 `system` "TLS"; item 8 `system` "Caches" ("Reference cache size (MB)", "Metadata cache size (MB)"). Rate limits and `DGP_TRUSTED_*` are env only | yes, except the env-only items | UI steps + YAML recap + Verify for items 2, 3, 8; the rest stays env | M | The page sets caches only with env vars; the UI can too (restart needed for `cache_size_mb`). |
| how-to/manage-iam-as-code.md | yes | YAML-only | partial: the mode switch on `access/credentials` card "IAM mode" ("GUI-managed (default)" / "Declarative (YAML-authoritative)"); account menu "Export full IAM (YAML)" (live secrets) and "Import full IAM (YAML)". The UI does not author the YAML | `access.iam_mode`, `iam_users`, `iam_groups`, `auth_providers`, `group_mapping_rules` | YAML-only + Verify (UI only for the mode switch and the preview) | M | The account-menu export and import and the UI mode switch are not mentioned. |
| how-to/migrate-existing-data-into-the-proxy.md | yes | mixed | yes: "Add Backend"; `storage/buckets` "More ways to add a bucket" → "Add settings for a bucket that does not exist yet", "Backend", "Advanced" → "Real name on backend"; `jobs` "New job" → "Backfill metadata… — for objects written without the proxy" → "Start now (N buckets)"; users as in create-iam-users | `storage.backends`, `storage.buckets.<b>.alias` | UI steps + YAML recap + Verify | M | "alias" is "Real name on backend" in the UI. "Settings → …" paths are stale. |
| how-to/monitor-with-prometheus.md | env only (`DGP_METRICS_BEARER_TOKEN`) | YAML-only | no | env only | YAML-only (say the UI cannot) | S | None. |
| how-to/move-a-bucket-between-backends.md | yes (a DB-born job) | GUI-only | yes: `storage/buckets` row "Migrate data…" → "Target backend", "Delete source objects after the switch-over", "Make the destination an exact mirror of the source", "Start migration"; also "New job" → "Migrate bucket…" | no (maintenance jobs are not YAML) | UI-only + API + Verify | S | "Settings → Jobs" is stale. |
| how-to/publish-a-public-folder.md | yes | mixed | yes, `storage/buckets` row: "Private (default)" / "Entire bucket public" / "Specific prefixes public", "Add prefix", "Review & apply" | `storage.buckets.<b>.public_prefixes`, `public` | UI steps + YAML recap + Verify | S | "Settings → Storage → Buckets" is stale. |
| how-to/replicate-a-bucket.md | yes | YAML-only | yes: destination bucket on `storage/buckets`; `jobs` "New job" → "Replication rule — continuous copy" → "Rule name", "Enabled", "Source", "Destination", "Advanced rule behavior" ("Interval", "Batch size", "Conflict policy", "Delete replication", globs), "Review & apply"; "Run now", "Pause"; tabs "Runs", "Failures", "Verify" ("Run verification") | `storage.replication.*`, `storage.buckets` | UI steps + YAML recap + Verify | M | The Verify button is "Run verification", not "Audit". `replication.enabled` has no UI control but defaults to true. |
| how-to/restrict-access-with-conditions.md | yes | mixed | partial, rule row "Conditions" → "List prefix" (s3:prefix StringLike) and "IP restriction" (IpAddress). `StringNotLike`, `NotIpAddress`, `StringEquals` are not in the UI; `DGP_TRUST_PROXY_HEADERS` is env only | declarative YAML only | UI steps + YAML recap + Verify, saying which operators need YAML or the API | M | "edit → permissions" is the "Conditions" button. |
| how-to/rotate-encryption-keys.md | yes | mixed | partial, `storage/backends`: "Rotate key", "I have stored this key safely…", "Apply"; alert "Decrypt-only shim active" → "Clear legacy key"; "Re-encrypt buckets…"; "Migrate data…". While a legacy key is set, the UI blocks key and mode changes, so recipes C and D with a live shim are YAML-only. No `key_id`/`legacy_key_id` fields | `storage.backends[].encryption.{key,key_id,legacy_key,legacy_key_id,mode}` | UI steps + YAML recap + Verify | L | "+ New job" is "New job". |
| how-to/route-a-bucket-to-a-backend.md | yes | mixed | partial (no edit of an existing backend, no change of `default_backend` after creation, no `${env:…}` refs) | yes | **Done** (reference example) | done | Rewritten in commit 33a7aebc. |
| how-to/run-multiple-instances.md | yes | YAML-only | partial: `system` "Config DB sync" → "Sync bucket" (restart); `DGP_CONFIG_DB_KEY*` env only; no sync-now button (API only) | `advanced.config_sync_bucket`; keys env only | UI steps + YAML recap + Verify for step 1 | M | UI help text says "the most recently saved copy wins", which contradicts the three-way merge. |
| how-to/scale-out-with-the-kubernetes-operator.md | no (CRD, kubectl) | neither | n/a | n/a | no change | none | None. |
| how-to/send-event-notifications.md | yes | YAML-only | yes, `integrations/event-delivery`: "Enable delivery", "Payload format" ("Raw webhook" / "Slack"), "Endpoints", "Add endpoint", "Allow local receivers", "Headers", "Add header", "Delivery tuning (retry, retention, batching)"; Slack "How to connect", "Incoming Webhook URL", "Bot token", "Channel", "Channel routing (per bucket / prefix)", "What gets posted (event kinds + prefix filters)"; `integrations/event-outbox` "Requeue" | `advanced.event_delivery.*` | UI steps + YAML recap + Verify | M | The "Failing" delivery state named on the page is not found in the UI source; verify before the rewrite. |
| how-to/serve-tls.md | yes | YAML-only | partial, `system`: "HTTP listener" → "Listen address"; "TLS" → "Enable TLS", "Certificate path", "Private key path" (restart). `DGP_TRUST_PROXY_HEADERS`, `_CIDRS`, `DGP_SECURE_COOKIES` env only | `advanced.tls.*`, `listen_addr` | UI steps + YAML recap + Verify (option A); option B stays env | S | None. |
| how-to/set-bucket-compression-and-quotas.md | yes | YAML-only | yes, `storage/buckets` row "Advanced": "Compression" ("Inherit — global default (…)" / "Always on" / "Off"), "Delta size cutoff", "Quota"; the "Object size limit" card on the same page | `storage.buckets.<b>.compression`, `max_delta_ratio`, `quota_bytes` | UI steps + YAML recap + Verify | S | The Quota field shows GB but converts GiB (`gibFromBytes`). "Settings → Storage → Buckets" is stale. |
| how-to/set-up-sso.md | yes | mixed | yes, except `priority`. `access/external-auth` "Add provider" ("Display Name", "Provider Name (unique identifier)", "Issuer URL", "Client ID", "Client Secret", "Scopes", "Network", "Enabled", "Create", "Test Connection"); "Allowed Users & Group Assignment" → "Add Rule" ("Match type", "Claim field", "Match value", "Assign to group", "Provider"), "Save Rules" | only in declarative mode | UI steps + YAML recap (declarative) + Verify | S | Field names in the page table differ in case from the UI. |
| how-to/trace-requests.md | env only (`DGP_DEBUG_HEADERS`, `DGP_AUDIT_RING_SIZE`) | YAML-only | diagnostics only (`diagnostics/trace`: "Test request", "Copy as JSON") | env only | no change | S | None. |
| how-to/troubleshooting.md | yes (symptom → fix) | mixed | partial | partly | no change (symptom index); fix labels only | S | "Settings → System → Logging" is the card "Log level". |
| how-to/upgrade.md | yes (TOML to YAML, v0.9 encryption) | YAML-only | backup only (`system` "Download backup") | yes | YAML-only + Verify | S | "Full Backup → Export" does not exist; it is "Download backup". |
| how-to/upgrade-to-2-0.md | env (`DGP_TRUSTED_PROXY_CIDRS`, `DGP_CONFIG_DB_KEY`, spool) | YAML-only | no | env only | YAML-only (say the UI cannot) + Verify | S | Name the "Download backup" button. |
| how-to/view-live-logs.md | yes | mixed | partial, `system` "Log level" → "Level" (Error/Warn/Info/Debug/Trace, "Custom EnvFilter"); the ring size is env only | `advanced.log_level` | UI steps + YAML recap + Verify | S | The card is "Log level", not "Logging"; the filter also has "All levels". |

## Annotated shots per area

Each shot is taken in both themes by `scripts/docs-screenshots.sh` and declared in `demo/s3-browser/ui/e2e/docs-screenshots/shots/<area>.ts`. A number in parentheses is the row of the shot list in [`docs-overhaul-2026-09.md`](docs-overhaul-2026-09.md) section 2, which the new shot replaces or reuses. "Step" is the step of the rewritten page that the shot serves. A callout number must match that step number.

### Area storage (`shots/storage.ts`)

| Shot id | Page | Route | Control to mark (visible label or role) | Step |
|---|---|---|---|---|
| `route-bucket-add-backend` | route-a-bucket | `storage/backends` | callout 1 sidebar "Backends", callout 2 "Add Backend" | 1.1–1.2 |
| `route-bucket-backend-form` | route-a-bucket | `storage/backends` (form "New Backend", S3, `hetzner-fsn1`) | callout 3 box around the S3 fields, callout 4 "Create Backend" | 1.3–1.4 |
| `route-bucket-buckets-row` | route-a-bucket | `storage/buckets` | callout 1 sidebar "Buckets", callout 2 the `downloads` row | 2.1–2.2 |
| `route-bucket-backend-select` | route-a-bucket | `storage/buckets` (`downloads` open, "Backend" list open) | arrow at option `local-disk` | 2.3 |
| `route-bucket-review-apply` | route-a-bucket | `storage/buckets` (dirty) | arrow at "Review & apply" | 2.4 |
| `route-bucket-apply-dialog` | route-a-bucket | `storage/buckets` (dialog open) | arrow at "Apply and Persist" | 2.5 |
| `route-bucket-alias` | route-a-bucket | `storage/buckets` (`db-archive`, "Advanced" open) | callouts 1 row, 2 "Advanced", 3 "Real name on backend" | 3.1–3.3 |
| `bucket-quota-compression` | set-bucket-compression-and-quotas | `storage/buckets` (`downloads`, "Advanced" open) | box "Compression" = "Off", arrow "Delta size cutoff" | 1 |
| `bucket-quota` (27) | set-bucket-compression-and-quotas | `storage/buckets` (`db-archive`, "Advanced" open) | arrow at "Quota" | 4–5 |
| `bucket-public-prefix` (29) | publish-a-public-folder | `storage/buckets` (`downloads` open) | callout 1 "Specific prefixes public", callout 2 "Add prefix" | 1 |
| `adopt-bucket-add-backend` (reuse 13 `backends-add`) | migrate-existing-data-into-the-proxy | `storage/backends` | arrow at "Add Backend" | route 1, 1 |
| `adopt-bucket-draft` | migrate-existing-data-into-the-proxy | `storage/buckets` | callout 1 "More ways to add a bucket", callout 2 "Add settings for a bucket that does not exist yet" | route 1, 2 |
| `adopt-bucket-alias` | migrate-existing-data-into-the-proxy | `storage/buckets` (`releases` draft, "Advanced" open) | arrow at "Real name on backend" | route 1, 3 |
| `backend-cas-reroute` | backend-capability-validation | `storage/buckets` | arrow at the row's "Backend" list | fix 1 |

### Area access (`shots/access.ts`)

| Shot id | Page | Route | Control to mark | Step |
|---|---|---|---|---|
| `credentials-bootstrap` (8) | secure-your-proxy, create-iam-users | `access/credentials` | box "Bootstrap SigV4 credentials", arrow "Auto-detect (recommended)" | 2 / 1 |
| `users-ci-uploader` (9) | secure-your-proxy, create-iam-users | `access/users` | callout 1 "New", callout 2 the rule row, callout 3 "Create User" | 3 / 2 |
| `user-permission-editor` (35) | create-iam-users | `access/users` (`dana`) | box "WHERE" and "CAN DO" | 3 |
| `iam-users-duplicate` | create-iam-users | `access/users` (`ci-uploader`) | arrow at "Duplicate user with fresh credentials" | 4 |
| `group-engineering` (36) | create-iam-users | `access/groups` | callout 1 "Members", callout 2 "Create Group" | 5 |
| `user-ip-condition` (37) | restrict-access-with-conditions | `access/users` (`backup-bot`) | callout 1 "Conditions", callout 2 "IP restriction" | 1 |
| `iam-conditions-list-prefix` | restrict-access-with-conditions | `access/users` | Deny control and "List prefix" | 2 |
| `iam-conditions-group-ip` | restrict-access-with-conditions | `access/groups` (`Engineering`) | "IP restriction", then "Save" | 4 |
| `iam-code-mode-switch` | manage-iam-as-code | `access/credentials` | arrow at "Declarative (YAML-authoritative)" in card "IAM mode" | 2 |
| `apply-dialog-iam-diff` (41) | manage-iam-as-code | `access/users` | arrow at "Apply and Persist" | 2 |
| `users-declarative-banner` (42) | manage-iam-as-code | `access/users` | box around the read-only note | Verify |
| `oidc-provider` (38) | set-up-sso | `access/external-auth` | box provider form, arrow "Create" | 2 |
| `group-mapping` (39) | set-up-sso | `access/external-auth` | callout 1 "Add Rule", callout 2 "Save Rules" | 3 |
| `admission-add-rule` | gate-requests-with-admission-rules | `access/admission` (modal "Add request rule") | box "Then", arrow "Add rule" | 1 |
| `request-rules` (30) | gate-requests-with-admission-rules | `access/admission` | arrow at the "Drag to reorder" handle | 2 |
| `admission-review-apply` | gate-requests-with-admission-rules | `access/admission` (dirty) | arrow at "Review & apply" | 3 |

### Area jobs (`shots/jobs.ts`)

| Shot id | Page | Route | Control to mark | Step |
|---|---|---|---|---|
| `replicate-dest-bucket` | replicate-a-bucket | `storage/buckets` | arrow at the "Backend" list of `releases-dr` | 1 |
| `replication-rule-editor` (14) | replicate-a-bucket | `jobs` (rule editor) | callouts on "Source", "Destination", "Conflict policy" | 2–4 |
| `replicate-run-now` | replicate-a-bucket | `jobs` | arrow at the row action "Run now" | 5 |
| `lifecycle-rule-editor` (18) | expire-and-archive-objects | `jobs` (rule editor) | box "Scope", "Expire after", "Action" | 1 |
| `lifecycle-transition` | expire-and-archive-objects | `jobs` | arrow "Action" = "Archive / move", box "Destination" | 2 |
| `lifecycle-preview` (19) | expire-and-archive-objects | `jobs?job=lifecycle:expire-old-downloads&tab=preview` | arrow at "Refresh preview" | 3 |
| `lifecycle-run-now` | expire-and-archive-objects | `jobs` | arrow at "Run now" and its confirm dialog | 4 |
| `migrate-dialog` (21) | move-a-bucket-between-backends, rotate-encryption-keys | `storage/buckets` ("Migrate data…" modal of `db-archive`) | callout 1 "Target backend", callout 2 "Start migration" | 1 / B2, C2 |
| `backend-encryption` (24) | encrypt-data-at-rest | `storage/backends` (`hetzner-fsn1`) | callout 1 "Encryption mode", callout 2 "I have stored this key safely…", callout 3 "Apply" | 2–3 |
| `reencrypt-dialog` (25) | encrypt-data-at-rest, rotate-encryption-keys | `storage/backends` (proposal "Encrypt existing objects?") | arrow at "Start now (N buckets)" | 5 / A3 |
| `rotate-key-button` | rotate-encryption-keys | `storage/backends` | arrow at "Rotate key" | A2 |
| `legacy-key-banner` (26) | rotate-encryption-keys | `storage/backends` | arrow at "Clear legacy key" | A4 |
| `adopt-bucket-backfill` (reuse 23 `backfill-dialog`) | migrate-existing-data-into-the-proxy | `jobs` ("New job" → "Backfill metadata…") | arrow at "Start now (N buckets)" | route 1, 4 |

### Area observability and system (`shots/system.ts`)

| Shot id | Page | Route | Control to mark | Step |
|---|---|---|---|---|
| `system-backup` (47) | back-up-and-restore | `system` | arrow at "Download backup" | 1 |
| `backup-restore-dialog` | back-up-and-restore | `system` ("Restore backup" modal) | box the mode choice, arrow "Restore" | 2 |
| `system-listener-tls` (48) | serve-tls, go-to-production | `system` | box "Enable TLS", "Certificate path", "Private key path" | A / 3 |
| `production-caches` | go-to-production | `system` | box card "Caches" | 8 |
| `system-sync-state` (49) | run-multiple-instances | `system` | arrow at "Sync bucket" | 1 |
| `logs-level-debug` | view-live-logs | `system` | arrow at "Debug" in card "Log level" | 1 |
| `event-webhook` (44) | send-event-notifications | `integrations/event-delivery` | callout 1 "Enable delivery", callout 2 "Add endpoint", callout 3 "Allow local receivers" | 1 |
| `event-slack` (45) | send-event-notifications | `integrations/event-delivery` | callout 2 "How to connect", callout 3 "What gets posted (event kinds + prefix filters)" | 2–3 |
| `event-log` (46) | send-event-notifications | `integrations/event-outbox` | arrow at "Requeue" | 5 |

Diagnostics pages (diagnose-backend-connectivity, trace-requests, troubleshooting) keep their illustrative shots from the overhaul plan (51, 31–33, 22) and get no step shots.

## Rewrite batches

Each page is in exactly one batch. A batch owns the shot file of its area, so two batches never edit the same page or the same shot file. The route-a-bucket page is done and sits in batch A only for its shots.

- **A. Storage, buckets and backends (7 pages, `shots/storage.ts`).** tutorials/first-delta-savings (labels only), set-bucket-compression-and-quotas, publish-a-public-folder, migrate-existing-data-into-the-proxy, backend-capability-validation, diagnose-backend-connectivity (labels only), route-a-bucket-to-a-backend (done). The backfill step of migrate-existing-data uses the jobs shot `adopt-bucket-backfill`, which batch C captures.
- **B. Access, IAM, SSO and admission (6 pages, `shots/access.ts`).** tutorials/secure-your-proxy, create-iam-users, manage-iam-as-code, restrict-access-with-conditions, set-up-sso, gate-requests-with-admission-rules.
- **C. Jobs, replication, lifecycle, migrate and encryption (5 pages, `shots/jobs.ts`).** replicate-a-bucket, expire-and-archive-objects, move-a-bucket-between-backends, encrypt-data-at-rest, rotate-encryption-keys.
- **D. Observability, system and deploy (15 pages, `shots/system.ts`).** tutorials/kubernetes-hello-world, go-to-production, deploy-with-docker-compose, deploy-on-kubernetes, scale-out-with-the-kubernetes-operator, serve-tls, run-multiple-instances, monitor-with-prometheus, trace-requests, view-live-logs, troubleshooting, upgrade, upgrade-to-2-0, back-up-and-restore, send-event-notifications. `reference/authentication.md` (key rotation, env only) is checked here too; it needs only the sentence that the UI cannot do it. Event delivery is under Integrations, which has no batch of its own.

## Decisions needed before the rewrite

These findings come from the audit. The first four are confirmed by a second read of the source.

1. **The lifecycle master switch has no UI control.** `storage.lifecycle.enabled` defaults to `false` (`src/config_sections.rs`, `LifecycleConfig`), and the Jobs UI only shows it ("Scheduler enabled/disabled" in `LifecycleSummary.tsx`). A reader who follows only the UI can never make a lifecycle rule run. Either add a switch to the Jobs page, or document the switch as YAML-only in the expire-and-archive page.
2. **The admission editor has no YAML view**, but gate-requests-with-admission-rules says it has one. Fix the page.
3. **The replication Verify button is "Run verification"**, but replicate-a-bucket calls it "Audit". Fix the page.
4. **The Sync bucket help text in the UI is wrong.** `advancedPanels.tsx` says "the most recently saved copy wins", but the sync is a three-way merge by name. Fix the UI text.
5. **The Quota field shows GB but converts GiB.** Change the label to GiB or the conversion to GB.
6. **The UI stores a generated encryption key in the config file**, while encrypt-data-at-rest recommends an env var. The page must say both, and say which one the UI does.
7. **Controls that exist only in YAML or the API:** the condition operators `StringNotLike`, `NotIpAddress` and `StringEquals`; editing an existing backend's endpoint or credentials; changing `default_backend` after creation; `replication_target_only`; sync-now; OIDC `priority`; `${env:…}` references for secrets. Each page that uses one says so in one sentence before the YAML.
8. **Stale menu paths everywhere.** Many pages say "Settings → Group → Leaf". The UI has no "Settings" level; the sidebar is `ADMIN_IA` (Observability, Access, Storage, Integrations, System), and Jobs is under Storage. The repo `CLAUDE.md` says "5 groups / 17 leaves", but `ADMIN_IA` has 18 leaves because of `access/sessions`.
