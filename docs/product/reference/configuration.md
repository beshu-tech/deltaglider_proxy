# Configuration

You configure DeltaGlider Proxy with a YAML file, with environment variables (`DGP_*` prefix), or with both. Environment variables always take precedence over the file contents. The admin GUI edits the same configuration and writes it back to the file; see [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md) for how the GUI, the file, the environment and the IAM database relate.

YAML is the only supported format. TOML support was removed in v1.4.1. A `.toml` config makes the proxy fail at startup, whether you set it with `DGP_CONFIG` or the proxy finds it on the default search path. The error is `TOML configs are no longer supported (removed in v1.4.1)`. If you still carry a TOML config, run `deltaglider_proxy config migrate` **on v1.4.0** to convert it, then point the server at the YAML file before upgrading. See [How to upgrade the proxy](../how-to/upgrade.md).

Most readers need three sections of this page: the [YAML layout](#yaml-layout), the [full example](#full-example), and the [environment variable registry](#environment-variable-registry).

## YAML layout

The canonical YAML has four optional top-level sections:

```yaml
# fragment
# deltaglider_proxy.yaml

admission:   # pre-auth request gating (deny / reject / allow-anonymous)
  blocks: [...]

access:      # SigV4 credentials + iam_mode selector
  iam_mode: gui             # gui (default) or declarative
  access_key_id: admin
  secret_access_key: changeme

storage:     # backend(s) + per-bucket overrides
  s3: https://s3.example.com
  buckets: {...}

advanced:    # process-level tunables
  listen_addr: "0.0.0.0:9000"
  cache_size_mb: 2048
  log_level: deltaglider_proxy=info
```

Every section is optional. Canonical exports (`GET /api/admin/config/export`) leave out every field that equals its default, so GitOps diffs stay small.

The flat (pre-Phase-3) shape, with root-level keys such as `listen_addr:` and `backend:`, still loads unchanged. A document that mixes the two shapes is a hard parse error, and the error names the conflicting keys. The sectioned shape refuses an unknown key. The flat shape ignores an unknown root key, so a typo such as `cache_size_mbb` keeps the default value. For this reason, the proxy logs a warning that names every unknown root key of a flat document, and `config lint` refuses the document with exit code `4`.

The same document is editable from the admin UI. The form shows which section owns each field, and it shows the YAML path of a field on hover. A field that an environment variable sets is read-only, and it shows a "from env" badge that names the `DGP_*` variable.

### What happens to the file when the admin UI saves

When you apply a change in the admin UI, the proxy writes the whole configuration file again. It does not edit the file in place. The proxy serializes its running configuration into the canonical form and replaces the file with the result, in one atomic rename. As a consequence, the saved file differs from a file that you wrote by hand in these ways:

- Your comments and your blank lines are not kept, because the running configuration does not hold them.
- The keys appear in the canonical order, and the file uses the four-section layout even when you wrote the flat layout.
- A field that equals its default value is left out of the file.
- A shorthand such as `public_prefixes: [""]` can come back as `public: true`.

Some values do survive the rewrite. An encryption key that you wrote into the YAML stays in the YAML. A `${env:NAME}` reference survives only in some positions, because the running configuration holds the resolved value, not your text. At load time, the proxy records which value came from which variable. At save time, it writes a reference back wherever a whole string value equals a recorded value. These rules follow from that mechanism:

- A reference that is the whole value, such as `secret_access_key: ${env:S3_SECRET}`, stays a reference. A secret that came from the environment does not end up in the file.
- A reference inside a longer string, such as `endpoint: "https://${env:S3_HOST}:9000"`, is written out as its resolved value, for example `endpoint: "https://s3.acme.example:9000"`. The file then no longer follows the variable.
- When a value of 16 characters or more occurs in several fields, every one of those fields gets the reference. For this reason, a secret that you copy into a second field also stays out of the file.
- When a shorter value, such as a region, occurs in several fields, no field gets the reference. The proxy cannot tell which field came from the variable, and it does not couple an unrelated field to it.

To keep a composed value under the control of the environment, put the whole value in one variable, for example `endpoint: ${env:S3_ENDPOINT}`.

If you manage the file in Git, keep your commented copy in the repository and treat the file on the server as generated output. To see the layout that the proxy writes, with its secrets redacted, request `GET /api/admin/config/export`.

## Shorthands

The proxy expands these operator shorthands into their canonical forms at load time.

### Storage shorthand for a single backend

```yaml
# validate
storage:
  s3: https://s3.example.com       # expands to backend: { type: s3, endpoint: ... }
  region: eu-central-1             # optional
  access_key_id: admin             # optional
  secret_access_key: changeme      # optional
  force_path_style: true           # optional
```

or

```yaml
# validate
storage:
  filesystem: /var/lib/deltaglider
```

Only one of `backend:` / `s3:` / `filesystem:` may be set. Companion fields (`region`, `access_key_id`, etc.) apply only to `s3:`.

### Bucket `public: true`

```yaml
# validate
storage:
  buckets:
    docs-site:
      public: true           # shorthand for public_prefixes: [""]
```

The canonical exporter collapses `public_prefixes: [""]` back to `public: true` when the result is unambiguous. The GUI "Public read" toggle maps 1:1 to the YAML.

Mixing `public: true` and a non-empty `public_prefixes` is a hard error.

## Config-file search order

`Config::resolve_config_path` returns the first match from:

1. `DGP_CONFIG` env var (returned unconditionally: when it is set, the proxy uses the path even when the file does not exist yet).
2. `./deltaglider_proxy.yaml`
3. `./deltaglider_proxy.yml`
4. `./deltaglider_proxy.toml` (tripwire: startup fails)
5. `/etc/deltaglider_proxy/config.yaml`
6. `/etc/deltaglider_proxy/config.yml`
7. `/etc/deltaglider_proxy/config.toml` (tripwire: startup fails)

The `.toml` entries are tripwires, because the proxy cannot load a TOML file. When the search matches a leftover TOML config (with no YAML earlier in the order), startup stops with an actionable error. The proxy does not skip the file.

CLI flags (`--config <path>`, `--listen <addr>`) take precedence over all of the above. Env vars take precedence over the file contents.

## Server / advanced

These are the process-level settings. In sectioned YAML, they live under `advanced:`.

### `listen_addr`

HTTP listen address.

| | |
|---|---|
| **Env var** | `DGP_LISTEN_ADDR` |
| **YAML** | `advanced.listen_addr` (sectioned) or root `listen_addr:` (flat) |
| **Default** | `0.0.0.0:9000` |
| **Hot-reload** | No (restart required) |

```yaml
# validate
advanced:
  listen_addr: "0.0.0.0:8080"
```

### `log_level`

Tracing filter string (`tracing-subscriber` syntax). `RUST_LOG` overrides it when set. You can change it at runtime in the admin GUI (the **Logging** card of **System → System**), which hot-reloads the filter through the apply pipeline. When `RUST_LOG` or `DGP_LOG_LEVEL` is set, that variable decides the level, so the Logging card shows its value read-only and names the variable. An apply cannot replace that level, because the environment variable wins over the file at every apply, not only at startup.

Resolution order: `RUST_LOG` > `DGP_LOG_LEVEL` > `advanced.log_level` in the file > the `--verbose` CLI flag (`deltaglider_proxy=trace,tower_http=trace`) > default. A file `log_level` equal to the default counts as not set, so `--verbose` wins over it.

| | |
|---|---|
| **Env var** | `DGP_LOG_LEVEL` |
| **YAML** | `advanced.log_level` |
| **Default** | `deltaglider_proxy=info,tower_http=info` |
| **Hot-reload** | Yes (via admin GUI or `config apply`) |

```yaml
# validate
advanced:
  log_level: deltaglider_proxy=info,tower_http=warn
```

#### Structured logs and the in-GUI log ring

Three env-only knobs control log output and the admin **System logs** viewer (see [View live logs](../how-to/view-live-logs.md)):

| Env var | Default | Effect |
|---|---|---|
| `DGP_LOG_FORMAT` | `text` | `json` emits one JSON object per stdout line, which you can filter with `jq` by client IP, bucket or action. The proxy reads it only at startup. |
| `DGP_LOG_RING_SIZE` | `2000` | Capacity of the in-memory operational-log ring behind the admin Logs viewer. |
| `DGP_LOG_RING_LEVEL` | `info` | Minimum severity captured into the ring/live-tail stream. The ring sees only the events that the global log level lets through, so this floor can narrow the capture but not widen it. |

The ring is per-instance, in memory, and bounded. It is meant for triage. Point a log shipper at the `DGP_LOG_FORMAT=json` stdout stream for retention and aggregation.

### `request_timeout_secs`

Per-request deadline (HTTP 504 when exceeded).

| | |
|---|---|
| **Env var** | `DGP_REQUEST_TIMEOUT_SECS` |
| **Default** | `300` (5 minutes) |
| **Hot-reload** | No |

### `max_concurrent_requests`

Global tower `ConcurrencyLimit`. Requests beyond this queue.

| | |
|---|---|
| **Env var** | `DGP_MAX_CONCURRENT_REQUESTS` |
| **Default** | `1024` |
| **Hot-reload** | No |

### `max_multipart_uploads`

The cap on concurrent multipart uploads. The proxy holds the parts of each open upload itself, in memory or in relay files in `DGP_SPOOL_DIR`, until the upload completes. A CreateMultipartUpload past the cap fails with `503 SlowDown`.

| | |
|---|---|
| **Env var** | `DGP_MAX_MULTIPART_UPLOADS` |
| **Default** | `1000` |
| **Hot-reload** | No |

These env-only variables set the other multipart limits and the sweeper:

| Env var | Default | Effect |
|---|---|---|
| `DGP_MAX_TOTAL_MULTIPART_BYTES` | `max_object_size × DGP_MAX_MULTIPART_UPLOADS / 4` | Cap on the multipart part bytes that all open uploads hold together. A part past the cap fails with `503 SlowDown` |
| `DGP_MULTIPART_IDLE_TTL_HOURS` | 24 | An open multipart upload that receives no part for this many hours is garbage-collected |
| `DGP_MULTIPART_SWEEP_INTERVAL_SECS` | 300 | How often the multipart sweeper runs, in seconds |
| `DGP_MULTIPART_SWEEP_MAX_AGE_SECS` | 3600 | Age in seconds after which the sweeper removes an open multipart upload |
| `DGP_MULTIPART_COMPLETING_TIMEOUT_SECS` | `DGP_MULTIPART_SWEEP_MAX_AGE_SECS` | Seconds after which the sweeper releases an upload that is stuck in the completing state |
| `DGP_BUCKET_USAGE_FLUSH_SECS` | 10 | How often the per-bucket usage counters are written to storage, in seconds |

### `blocking_threads`

Tokio blocking thread-pool size. It controls how many CPU-bound operations (xdelta3 subprocesses) can run at the same time.

| | |
|---|---|
| **Env var** | `DGP_BLOCKING_THREADS` |
| **YAML** | `advanced.blocking_threads` |
| **Default** | tokio default (512) |
| **Hot-reload** | No |

### `debug_headers`

Expose debug/fingerprinting headers: `x-amz-storage-type` and `x-deltaglider-stored-size` on an object response, and `x-deltaglider-listing-facts-misses` on a LIST. Disable in production to prevent server fingerprinting.

| | |
|---|---|
| **Env var** | `DGP_DEBUG_HEADERS` |
| **Default** | `false` |
| **Hot-reload** | No |

### `cors_permissive`

Enable permissive CORS for cross-origin admin access. Use it only in development, because it opens the door to CSRF against the session-cookie endpoints.

| | |
|---|---|
| **Env var** | `DGP_CORS_PERMISSIVE` |
| **Default** | `false` |
| **Hot-reload** | No |

### `config`

Path to the config file.

| | |
|---|---|
| **Env var** | `DGP_CONFIG` |
| **Default** | Auto-detect (search list above) |

When `DGP_CONFIG` is set, the proxy uses that path unconditionally. When the file is missing, the proxy does not fall back to the default search list. This prevents the admin API from persisting to a CWD-relative file that the operator never asked for.

## Delta engine

### `max_delta_ratio`

Store an object as a delta only if `delta_size / original_size` is below this ratio. A lower value saves space more aggressively. A higher value keeps more files as deltas.

| | |
|---|---|
| **Env var** | `DGP_MAX_DELTA_RATIO` |
| **YAML** | `advanced.max_delta_ratio` |
| **Default** | `0.75` |
| **Hot-reload** | Yes |

### `max_object_size`

Maximum object size in bytes. The proxy enforces it as the HTTP request body limit, so it caps uploads of both delta and passthrough objects. It is also the per-object ceiling for delta processing (an xdelta3 memory constraint), and it sizes the multipart upload budget. `0` rejects all uploads, and the proxy logs a warning at startup.

| | |
|---|---|
| **Env var** | `DGP_MAX_OBJECT_SIZE` |
| **Default** | `104857600` (100 MB) |
| **Hot-reload** | Yes |

### `cache_size_mb`

In-memory reference cache size in MB. Use 1024 MB or more in production. A cache smaller than 1024 MB causes a startup warning.

| | |
|---|---|
| **Env var** | `DGP_CACHE_MB` |
| **YAML** | `advanced.cache_size_mb` |
| **Default** | `100` |
| **Hot-reload** | Yes (triggers engine rebuild; the new cache starts empty) |

### `metadata_cache_mb`

In-memory `FileMetadata` cache size in MB. Set it to `0` to disable the cache. 50 MB holds about 125K-150K entries. Entries have a 10-minute TTL.

| | |
|---|---|
| **Env var** | `DGP_METADATA_CACHE_MB` |
| **YAML** | `advanced.metadata_cache_mb` |
| **Default** | `50` |
| **Hot-reload** | Yes (triggers engine rebuild; the new cache starts empty) |

### `filtered_list_max_engine_pages`

The number of backend pages that one filtered LIST request may read. A LIST is filtered when the caller's policy cannot be narrowed to key prefixes, for example an `Allow` on the whole bucket with a `Deny` exception. The proxy then reads the requested prefix page by page and skips the keys that the caller cannot see. When it finds no visible key within this number of pages, the request fails with `400 InvalidRequest`, and the caller must list a narrower prefix.

| | |
|---|---|
| **Env var** | `DGP_FILTERED_LIST_MAX_ENGINE_PAGES` |
| **YAML** | `advanced.filtered_list_max_engine_pages` |
| **Default** | `50` |
| **Hot-reload** | Yes |

### `codec_concurrency`

Maximum concurrent xdelta3 subprocesses. Auto-detected as `num_cpus * 4` (min 16).

| | |
|---|---|
| **Env var** | `DGP_CODEC_CONCURRENCY` |
| **YAML** | `advanced.codec_concurrency` |
| **Default** | `num_cpus * 4` (min 16) |
| **Hot-reload** | Yes (triggers engine rebuild) |

### `range_spool_ttl_secs`

The number of seconds that a verified reconstruction of a large delta object stays in the spool for more range reads of the same object. A range GET of a delta object that is larger than `DGP_SPOOL_THRESHOLD_BYTES` reconstructs the whole object into a spool file and checks its SHA-256 before it sends the range. The proxy keeps that file for this long, so the other range reads of the object (a parallel downloader sends many) read the same file instead of reconstructing the object again. A range read that arrives while the reconstruction runs waits for it. The file counts against `DGP_SPOOL_MAX_BYTES`, and the proxy deletes cached files first when another request needs spool space that is not free. At most 16 reconstructions are cached. `0` turns the cache off.

| | |
|---|---|
| **Env var** | `DGP_RANGE_SPOOL_TTL_SECS` |
| **YAML** | `advanced.range_spool_ttl_secs` |
| **Default** | `60` |
| **Hot-reload** | Yes (triggers engine rebuild) |

### `codec_timeout_secs`

Maximum time for an xdelta3 subprocess. The proxy kills a hung process after this time.

| | |
|---|---|
| **Env var** | `DGP_CODEC_TIMEOUT_SECS` |
| **Default** | `60` |
| **Hot-reload** | No |

## Storage backend

### Filesystem backend

Local filesystem. The proxy uses it when you set `DGP_DATA_DIR` or a `backend:` block with `type = "filesystem"`.

#### `data_dir`

| | |
|---|---|
| **Env var** | `DGP_DATA_DIR` |
| **YAML (shorthand)** | `storage.filesystem: <path>` |
| **YAML (canonical)** | `storage.backend.path` |
| **Default** | `./data` |
| **Hot-reload** | Yes (triggers engine rebuild) |

```yaml
# fragment
# Shorthand
storage:
  filesystem: /var/lib/deltaglider

# Canonical (equivalent)
storage:
  backend:
    type: filesystem
    path: /var/lib/deltaglider
```

The proxy rejects a path that contains `..` components at load time.

### S3 backend

AWS S3, MinIO, Hetzner, Backblaze, or any other S3-compatible service. The proxy uses it when you set `DGP_S3_ENDPOINT` or `DGP_S3_REGION`, or a `backend:` block with `type = "s3"`. When one of these two variables is set, the proxy replaces the whole singleton backend with an S3 backend built from the `DGP_S3_*` and `DGP_BE_AWS_*` variables, and a member without a variable takes its default. Without one of the two variables, the proxy ignores `DGP_S3_PATH_STYLE` and the `DGP_BE_AWS_*` keys. `DGP_BACKEND_ALLOW_LOCAL=true` applies to every S3 backend, named backends included, as if each one set `allow_local: true`.

#### `endpoint` / `region` / `force_path_style` / `access_key_id` / `secret_access_key`

| Field | Env var | YAML shorthand | YAML canonical | Default |
|-------|---------|----------------|----------------|---------|
| endpoint | `DGP_S3_ENDPOINT` | `storage.s3: <url>` | `storage.backend.endpoint` | — (AWS default) |
| region | `DGP_S3_REGION` | `storage.region` | `storage.backend.region` | `us-east-1` |
| force_path_style | `DGP_S3_PATH_STYLE` | `storage.force_path_style` | `storage.backend.force_path_style` | `true` |
| allow_local | `DGP_BACKEND_ALLOW_LOCAL` | — | `storage.backend.allow_local` (also on each named backend) | `false` (allow `http://` and private-IP endpoints, for MinIO, development and CI) |
| access_key_id | `DGP_BE_AWS_ACCESS_KEY_ID` | `storage.access_key_id` | `storage.backend.access_key_id` | — |
| secret_access_key | `DGP_BE_AWS_SECRET_ACCESS_KEY` | `storage.secret_access_key` | `storage.backend.secret_access_key` | — |

```yaml
# fragment
# Shorthand
storage:
  s3: https://hel1.your-objectstorage.com
  region: hel1
  access_key_id: AKIAIOSFODNN7EXAMPLE
  secret_access_key: wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY

# Canonical
storage:
  backend:
    type: s3
    endpoint: https://hel1.your-objectstorage.com
    region: hel1
    force_path_style: true
    access_key_id: AKIAIOSFODNN7EXAMPLE
    secret_access_key: wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
```

Endpoint URLs must start with `http://` or `https://`. The proxy rejects a value without a scheme at load time.

The admin API shows the `access_key_id` of every S3 backend in `GET /api/admin/config/export`, in the storage section, and in `GET /api/admin/backends`, because an access key id is an identifier and not a secret. The `secret_access_key` is never shown. When a document or section that you apply carries the same `access_key_id` and no `secret_access_key`, the proxy keeps the current secret, so an unedited export applies without change. A different `access_key_id` without a secret is a rotation that is missing its secret: the proxy does not pair the new id with the old secret, and it returns a warning.

## Access: authentication

The proxy **refuses to start** without credentials unless you set `authentication = "none"`.

### `authentication`

Explicit auth-mode selector. When the field is absent, the proxy detects the mode from the credentials. `"none"` means open access (dev only). In open access, the proxy still checks the signature of a signed request, with the access key as the secret, so a signed client must use the same value for both keys (see [Authentication and access](authentication.md#authentication-modes)).

| | |
|---|---|
| **Env var** | `DGP_AUTHENTICATION` |
| **YAML** | `access.authentication` |
| **Default** | None (auto-detect; a **fatal error** when both this field and the credentials are absent) |
| **Hot-reload** | No |

### `access_key_id` / `secret_access_key`

Proxy-level SigV4 credentials (the "bootstrap admin" credential pair).

| | |
|---|---|
| **Env vars** | `DGP_ACCESS_KEY_ID` / `DGP_SECRET_ACCESS_KEY` |
| **YAML** | `access.access_key_id` / `access.secret_access_key` |
| **Default** | None |
| **Hot-reload** | Yes |

```yaml
# validate
access:
  access_key_id: admin
  secret_access_key: changeme
```

### `bootstrap_password_hash`

Bcrypt hash of the bootstrap password (it signs session cookies and gates admin GUI access in bootstrap mode). The proxy generates it on the first run. It accepts base64-encoded hashes, so that you do not need to escape `$` in Docker and env vars. It does not encrypt the IAM config DB: that is the job of [`DGP_CONFIG_DB_KEY`](#config-db-key).

| | |
|---|---|
| **Env var** | `DGP_BOOTSTRAP_PASSWORD_HASH` (legacy alias: `DGP_ADMIN_PASSWORD_HASH`) |
| **YAML** | `advanced.bootstrap_password_hash` (an infra secret, which canonical exports strip) |
| **Default** | Auto-generated on first run |

### Config DB key

The SQLCipher key of the IAM config DB (`deltaglider_config.db`). When the variable is unset, the proxy uses the key file `deltaglider_config.db.key` next to the DB, and it generates that file (mode 0600) on the first start. When `config_sync_bucket` is set, the variable is required and must be identical on every instance, because all instances share one encrypted DB. The value needs at least 32 characters. See [Config database key](authentication.md#config-database-key) for the upgrade from the hash-keyed DB of earlier releases.

| | |
|---|---|
| **Env var** | `DGP_CONFIG_DB_KEY` |
| **YAML** | none (env only, so that the key never lands in a config file or an export) |
| **Default** | Key file next to the DB, generated on first start |

To rotate the key, set `DGP_CONFIG_DB_KEY_PREVIOUS` to the old key and `DGP_CONFIG_DB_KEY` to the new one; see [Config database key](authentication.md#config-database-key).

### `DGP_BOOTSTRAP_PASSWORD`

Plaintext bootstrap password for the `config apply` / `admission trace` admin CLI commands. They read the password from this env var and not from an argument, because argv leaks through `ps`. The server itself does not read it.

| | |
|---|---|
| **Env var** | `DGP_BOOTSTRAP_PASSWORD` |
| **Consumer** | Admin CLI (`deltaglider_proxy config apply`, `... admission trace`) |

## Access: IAM mode

The `access.iam_mode` YAML selector controls where IAM state (users, groups, OAuth providers, mapping rules) lives. It is independent of the `authentication` selector.

| Mode | Meaning |
|------|---------|
| `gui` *(default)* | The encrypted SQLCipher DB is the source of truth. The admin GUI and the admin API change it. YAML `access.*` carries only the legacy SigV4 pair and the `authentication` selector. |
| `declarative` | YAML `access.iam_users`, `iam_groups`, `auth_providers`, and `group_mapping_rules` are authoritative. Admin API IAM mutation routes (`POST/PUT/PATCH/DELETE` on `/users`, `/groups`, `/ext-auth/*`, `/migrate`, backup import) return `403 { "error": "iam_declarative" }`. Read routes stay accessible. |

```yaml
# validate
access:
  iam_mode: declarative
```

The proxy audit-logs mode transitions at `warn` level on the `deltaglider_proxy::config` target. In declarative mode, every `/config/apply` or section-PUT on `access` runs a dry validation and a diff, and then reconciles the encrypted config DB to YAML in one SQLite transaction. Creates, updates, and deletes emit `iam_reconcile_*` audit entries.

The initial `gui → declarative` flip is guarded: if YAML contains no users or groups while the DB is non-empty, apply fails instead of wiping IAM by accident. To seed GitOps YAML from an existing DB, use `GET /_/api/admin/config/declarative-iam-export`; see [Declarative IAM](declarative-iam.md) for the full workflow.

## Admission chain

Request rules gate requests before authentication. Each entry of `blocks` is one rule. The proxy checks the rules from top to bottom, and the first rule that matches decides. The proxy checks your rules *before* the public-access rules that it creates from `storage.buckets[*].public_prefixes`.

```yaml
# validate
admission:
  blocks:
    - name: deny-known-bad-ips
      match:
        source_ip_list:
          - "198.51.100.17"
          - "198.51.100.0/24"
      action: deny

    - name: maintenance-mode
      match: {}              # empty = match every request
      action:
        type: reject
        status: 503
        message: "Planned maintenance — back at 18:00 UTC."

    - name: allow-public-zips
      match:
        method: [GET, HEAD]
        bucket: releases
        path_glob: "*.zip"
      action: allow-anonymous
```

### Block fields

| Field | Type | Notes |
|-------|------|-------|
| `name` | string (required) | 1-128 chars, `[A-Za-z0-9_:.-]`. Must be unique across the chain. `public-prefix:*` is reserved for the public-access rules. |
| `match` | object (default `{}`) | AND-combined predicates. Empty `{}` fires on every request. |
| `match.method` | `[string]` | HTTP methods: `GET` `HEAD` `PUT` `POST` `DELETE` `PATCH` `OPTIONS`. Case-insensitive on parse. |
| `match.source_ip` | IP | Exact match. Mutually exclusive with `source_ip_list`. |
| `match.source_ip_list` | `[IP \| CIDR]` | Accepts bare IPs (promoted to `/32` or `/128`) and CIDRs. Cap: 4096 entries. |
| `match.bucket` | string | Target bucket (lowercased on parse). |
| `match.path_glob` | string | Glob against the full key: `*.zip`, `releases/**`, `docs/readme.md`. |
| `match.authenticated` | bool | `true` = only authenticated; `false` = only anonymous; absent = either. |
| `match.config_flag` | string | Named flag. The flag registry is not live yet: the proxy recognises `maintenance_mode`, but it always evaluates to false, and the proxy logs a warning when it builds the chain. |
| `action` | string \| object (required) | Simple: `allow-anonymous`, `deny`, `continue`. Tagged: `{ type: reject, status: <4xx\|5xx>, message?: <string> }`. |

The operator chain holds at most 1000 blocks. The proxy checks the blocks for every request, so it refuses a longer chain with an error that names the count. To gate many addresses, list them in the `source_ip_list` of one block.

`continue` is an explicit terminal action that passes the request on to authentication. It is useful as the final block, because the trace output then shows where the chain ended.

`allow-anonymous` lets the request that matched the block through without credentials, as the `$anonymous` principal. It grants exactly that one request, and only when the request is a read: a `GET` or `HEAD` of the matched object, or a listing of the matched bucket with the requested `prefix`. It never grants a write. An object key that contains `*` or `?` gets no grant, because a permission reads those characters as wildcards and would then cover other keys too; such a request continues without credentials and is refused with `403`. A `PUT`, `POST` or `DELETE` that matches an `allow-anonymous` block continues without credentials and is refused with `403 AccessDenied`. In the example above, an unsigned `GET /releases/builds/app.zip` returns the object, and an unsigned `PUT` of the same key returns `403`. The trace (`POST /_/api/admin/config/trace`) shows the grant in its `anonymous_grant` field (`{"action": "read", ...}`, `{"action": "list", ...}`, `{"action": "public-prefixes", ...}` for a public-access rule, whose grant is the bucket's `public_prefixes`, or `null`), and the live request path uses the same function to decide.

The `source_ip` and `source_ip_list` conditions match the address of the TCP connection. The proxy reads the client address from `X-Forwarded-For` only when `DGP_TRUST_PROXY_HEADERS=true` and the connection comes from a network that `DGP_TRUSTED_PROXY_CIDRS` lists. Without the list the proxy cannot tell a header that a reverse proxy wrote from a header that the client forged, so it refuses to start with `DGP_TRUST_PROXY_HEADERS=true` alone.

### Round-trip

`source_ip_list` entries round-trip verbatim (bare IPs stay bare, CIDRs stay CIDRs) so GitOps diffs do not change on every apply.

The admin UI page **Request rules** (`/_/admin/access/admission`) edits these rules. The `public-prefix:*` public-access rules show read-only below your rules; change them in **Storage → Buckets** instead.

## Security

### `trust_proxy_headers`

Trust `X-Forwarded-For` / `X-Real-IP` from the reverse proxies that `DGP_TRUSTED_PROXY_CIDRS` lists. Disable it when the proxy faces the internet without a reverse proxy.

A client can write any `X-Forwarded-For` value, so the proxy reads the header only on a connection from a network in `DGP_TRUSTED_PROXY_CIDRS`. On any other connection it uses the address of the TCP connection. The proxy uses the one resulting client address for every decision: the per-IP rate limit, the IP binding of admin sessions, the known-good exemption from the login lockout, admission `source_ip` rules, and IAM `aws:SourceIp` conditions. The proxy refuses to start when this setting is `true` and `DGP_TRUSTED_PROXY_CIDRS` is unset or holds no valid network. The same rule covers `X-Forwarded-Host` and `X-Forwarded-Proto`: they count for the same-origin check of admin requests, the `Secure` flag of session cookies, and the OAuth callback address only on a connection from a trusted proxy.

| | |
|---|---|
| **Env var** | `DGP_TRUST_PROXY_HEADERS` |
| **Default** | `false` (secure-by-default) |
| **Hot-reload** | No |

**Production-critical behind a reverse proxy.** If the proxy sits behind Coolify, Traefik, nginx, Caddy, or an ALB and this setting stays `false`, every request appears to come from the IP of the reverse proxy. All clients then share a single rate-limit bucket, and one busy client can lock out everyone else with `503 SlowDown`. Set it to `true` whenever a trusted reverse proxy injects `X-Forwarded-For` / `X-Real-IP`. The save-time config advisories (see [Config advisories](#config-advisories)) flag the combination of rate limiting on and this setting off. Leave it `false` only when the proxy faces the internet directly, because there a client could spoof the headers.

### `session_ttl_hours`

Admin GUI session TTL.

| | |
|---|---|
| **Env var** | `DGP_SESSION_TTL_HOURS` |
| **Default** | `4` |
| **Hot-reload** | No |

### `clock_skew_seconds`

SigV4 clock skew tolerance.

| | |
|---|---|
| **Env var** | `DGP_CLOCK_SKEW_SECONDS` |
| **Default** | `900` (15 min) |
| **Hot-reload** | No |

### `replay_window_secs`

SigV4 replay detection window. The proxy treats a mutating request as a replay when it already saw the same signature within this many seconds. The default is twice the clock skew tolerance (`DGP_CLOCK_SKEW_SECONDS`, 900 s). The proxy accepts a signature that is dated up to the skew in the future, so a signature can stay valid for up to twice the skew after its first use. The default therefore refuses a captured mutation for its whole valid life.

- The proxy rejects a replayed mutating request (PUT, POST, DELETE and the other mutating methods) with `400 Request replay detected`.
- The proxy serves a PUT or DELETE that the client retries inside its signing second. SigV4 timestamps have 1-second granularity, so when an SDK retries a PUT or DELETE within the same second in which it signed it, the retry carries the same signature. This happens, for example, when a load balancer loses the response of a PUT that succeeded. The proxy serves a duplicate PUT or DELETE that arrives less than one second after the first copy, measured on the proxy's own clock, so client clock skew cannot stretch that second. Repeating a PUT or DELETE repeats the same effect. A duplicate that arrives one second or more after the first copy is rejected, and every other mutating method (POST) stays strict.
- The proxy does not track idempotent reads (GET and HEAD). Their signatures never enter the cache, and the proxy serves a duplicate normally. This is deliberate. SigV4 timestamps have 1-second granularity, so boto3 and botocore emit byte-identical SigV4 signatures when they send the same request (or auto-retry it) within one signing second. A replayed read only reads the same bytes again, so it has no double effect to guard against.
- Presigned URLs are fully exempt, because they are designed for reuse during their whole expiry.
- Only a request that succeeds (2xx or 3xx) keeps its signature in the cache. When a request fails, for example with `503 SlowDown` while a maintenance job holds the bucket, the proxy forgets its signature, so the SDK can retry it with the same signature inside the same second. A duplicate POST that arrives while the first request still runs is rejected.
- The signature of a replayed request is valid, so a replay rejection is not an authentication failure. The proxy audits it as `replay_rejected`, and it does not count toward the per-IP brute-force lockout.

Set `DGP_REPLAY_WINDOW_SECS=0` to disable replay rejection entirely (the window never matches). This is useful in CI, or as an escape hatch when a client sends mutations closer together than the default tolerates.

| | |
|---|---|
| **Env var** | `DGP_REPLAY_WINDOW_SECS` |
| **Default** | twice the value of `DGP_CLOCK_SKEW_SECONDS` (`1800`) |
| **Hot-reload** | No |

### `secure_cookies`

Controls the `Secure` flag on admin session cookies. `true` always sets it and `false` never sets it. When the variable is unset, the proxy sets the flag when its own listener serves TLS (`advanced.tls.enabled: true` or `DGP_TLS_ENABLED=true`), or when `DGP_TRUST_PROXY_HEADERS=true` and the request carries `X-Forwarded-Proto: https`.

| | |
|---|---|
| **Env var** | `DGP_SECURE_COOKIES` |
| **Default** | unset (automatic, as described above) |
| **Hot-reload** | No |

### Config advisories

At save time, the proxy runs a set of cross-field checks in the admin **Apply** dialog and in `config apply` / `config lint`. It shows a warning for each combination of settings that are valid one by one but suspicious together. The advisories never block a save. The current rules are:

| Advisory | Fires when | Why it matters |
|---|---|---|
| Shared rate-limit bucket | Rate limiting is enabled but `trust_proxy_headers` is `false` | Behind a reverse proxy, every client appears with the IP of the reverse proxy and shares one bucket, so one client can lock out all others. |
| Stale IAM template | A permission resource uses a bare `${username}` instead of `${iam:username}` | The bare form is not substituted, so the rule matches nothing and silently denies the user. |
| Frozen bucket quota | A bucket's `quota_bytes` is `0` | A zero quota rejects all writes to that bucket. |
| Redundant public prefix | `public_prefixes` are set while `authentication: none` | Auth is already open, so the public-prefix rules add nothing. |

### Rate limiting

Per-IP brute-force protection for auth endpoints. See [Rate limits and concurrency](rate-limits.md) for the full model.

| Setting | Env var | Default |
|---------|---------|---------|
| Max failures before lockout | `DGP_RATE_LIMIT_MAX_ATTEMPTS` | `100` |
| Rolling window | `DGP_RATE_LIMIT_WINDOW_SECS` | `300` (5 min) |
| Lockout duration | `DGP_RATE_LIMIT_LOCKOUT_SECS` | `600` (10 min) |

## TLS

When TLS is enabled, both the S3 API and the admin GUI serve HTTPS on the single listener.

```yaml
# validate
advanced:
  tls:
    enabled: true
    cert_path: /etc/ssl/certs/proxy.pem
    key_path: /etc/ssl/private/proxy-key.pem
```

| Field | Env var | YAML | Default |
|-------|---------|------|---------|
| enabled | `DGP_TLS_ENABLED` | `advanced.tls.enabled` | `false` |
| cert_path | `DGP_TLS_CERT` | `advanced.tls.cert_path` | Auto-generate self-signed |
| key_path | `DGP_TLS_KEY` | `advanced.tls.key_path` | Auto-generate |

When `cert_path` and `key_path` are both absent, the proxy generates a self-signed certificate at startup.

The proxy binds TLS once, at startup. When you apply a `tls.*` change at runtime (admin API or `config apply`), the proxy persists it, but the change does **not** take effect until you restart the proxy. The apply response flags this with a `requires_restart` warning.

## Config sync

Config sync coordinates several instances through an S3 bucket. When you enable it, the shared bucket has these roles:

- The proxy replicates the encrypted config DB file to it (IAM sync).
- It hosts the per-rule replication leader leases (`_dgp/leases/replication/…`, automatic failover) and the cross-instance `reference.bin` locks (`_dgp/locks/reference/…`).
- Setting it turns on the boot-time conditional-write validation of this bucket and of every named S3 backend that hosts client-writable buckets. When one of them is not CAS-capable, the proxy refuses to start (see [backend capability validation](../how-to/backend-capability-validation.md)).

| | |
|---|---|
| **Env var** | `DGP_CONFIG_SYNC_BUCKET` |
| **YAML** | `advanced.config_sync_bucket` |
| **Default** | None (disabled) |

`DGP_CONFIG_SYNC_KEY` (YAML `advanced.config_sync_object_key`) sets the object key of the synced DB in the sync bucket. The default is `.deltaglider/config.db`.

```yaml
# validate
advanced:
  config_sync_bucket: dgp-iam-sync
```

Every instance that shares the bucket must set `DGP_CONFIG_DB_KEY` to the same value, because the proxy encrypts the synced DB with that key. The proxy refuses to start with a sync bucket but without the variable. For the same reason, the admin API refuses a configuration change that sets or changes `config_sync_bucket` when the instance runs without `DGP_CONFIG_DB_KEY`. An instance whose key does not open the synced DB refuses to merge it and logs an error that names `DGP_CONFIG_DB_KEY`.

The sync bucket lives on an S3 backend, and the proxy uses the endpoint and the credentials of that backend. The proxy picks the backend in this order:

- When no named backends exist, or when the singleton backend (`storage.backend`) is S3, the sync bucket is on the singleton backend.
- Otherwise, the proxy routes the sync bucket like any other bucket: a `backend:` route under `storage.buckets.<sync bucket>` names its backend, otherwise `default_backend` hosts it. For this reason, a filesystem singleton beside a named S3 backend also works.

Sync needs that backend to be S3, because a filesystem backend is local to one node. On every IAM mutation, the proxy uploads the DB to `s3://<bucket>/.deltaglider/config.db` (`advanced.config_sync_object_key` or `DGP_CONFIG_SYNC_KEY` changes the key). The other instances poll the ETag of that object every 5 minutes and download the DB when the ETag changes.

The sync bucket is reserved for the proxy itself. The proxy refuses every S3 request to it with `403 AccessDenied`, for every identity including administrators, because an object that a client writes there could replace the synced IAM database or a lease on every instance. For the same reason, ListBuckets and the admin bucket list do not show it, and the admin bulk endpoints refuse it. A configuration that makes it public, gives it an alias, aliases another bucket onto it, or uses it in a replication or lifecycle rule is refused. A `backend:` route for it is allowed, because that only says which backend hosts it.

Every upload carries a sync generation, a number that the uploading instance takes one above every copy that it merged before. A later copy therefore always has a higher number than the copy that it replaces. A downloaded copy whose generation is lower than the generation of the copy that this instance last synced is a copy put back from the past, so the instance refuses it as a rollback and logs both numbers. The instance clocks play no part in this decision, so an instance whose clock runs behind its peers does not look like a rollback.

A copy from an instance on a release before the sync generation (config DB schema v29) carries no generation. For such a copy, the instance uses the older rule: a copy that holds a row in a version more than five minutes older than the copy that this instance last synced is refused as a rollback. This older rule reads the clock of each writer, so during a rolling upgrade, keep the instance clocks within five minutes of each other. An instance on the older release does not merge a copy from an upgraded instance (it logs that the copy has a newer schema) until it is upgraded too.

The sync poller starts once, at boot. When you enable or change `config_sync_bucket` at runtime, the proxy persists the change, but it does **not** take effect until a restart. The apply response flags this with a `requires_restart` warning.

## Multi-backend routing

You can route different buckets to different storage backends. When `backends` is non-empty, the proxy ignores the legacy single `backend` at runtime.

```yaml
# validate
storage:
  default_backend: hetzner-fsn1
  backends:
    - name: hetzner-fsn1
      type: s3
      endpoint: https://fsn1.your-objectstorage.com
      region: fsn1
      access_key_id: HETZNER_KEY
      secret_access_key: HETZNER_SECRET
    - name: aws-dr
      type: s3
      endpoint: https://s3.eu-west-1.amazonaws.com
      region: eu-west-1
      access_key_id: AWS_KEY
      secret_access_key: AWS_SECRET
    - name: local-disk
      type: filesystem
      path: /var/lib/dgp-local
  buckets:
    db-archive:
      backend: hetzner-fsn1
      alias: acme-db-archive-prod
```

You can add and remove backends in the admin GUI (**Storage → Backends**) without a restart. At load time, the proxy checks `default_backend` against the `backends` list. It clears an invalid reference and logs a warning.

## Bucket policies

Bucket policies are per-bucket overrides. All fields are optional.

```yaml
# fragment
storage:
  buckets:
    releases:
      compression: true
      max_delta_ratio: 0.9
      backend: hetzner-fsn1
      alias: acme-prod-releases-fsn1
      quota_bytes: 10737418240    # 10 GiB
    downloads:
      public_prefixes: ["public/"]
    docs-site:
      public: true                # shorthand for public_prefixes: [""]
    releases-mirror:
      backend: b2-archive
      replication_target_only: true
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `compression` | bool | global | Enable/disable delta compression for this bucket |
| `max_delta_ratio` | float (0-1) | global | Override the delta-keep threshold |
| `backend` | string | default | Route to a named backend from `storage.backends` |
| `alias` | string | same as bucket name | Virtual → real bucket name mapping on the backend |
| `public_prefixes` | `[string]` | `[]` | Anonymous read (GET/HEAD/LIST) scoped to these key prefixes |
| `public` | bool | — | Shorthand for `public_prefixes: [""]` (entire bucket public) |
| `quota_bytes` | u64 | — | Soft storage quota (it may overshoot by up to 5 minutes of writes); `0` freezes the bucket |
| `replication_target_only` | bool | `false` | Client writes return 403; replication is the only writer. This makes a non-CAS backend (e.g. Backblaze B2) a safe mirror (see [backend capability validation](../how-to/backend-capability-validation.md)) |

### Public prefixes

When `public_prefixes` (or `public: true`) is set, anonymous users can GET, HEAD, and LIST objects under the prefix. Writes always require authentication. Use trailing `/` for directory-aligned matching (`"public/"` matches `public/installer.zip` but not `publicity/`). The empty string `""` makes the entire bucket public, and the proxy logs a warning. The proxy rejects prefixes that contain `..`, null bytes, or `//`. The proxy creates `public-prefix:<bucket>` request rules from this setting.

## Lifecycle rules

Expiration (delete) and transition/archive rules live under `storage.lifecycle`. Lifecycle is disabled by default. Every delete and copy goes through the DeltaGlider engine.

```yaml
# validate
storage:
  lifecycle:
    enabled: false
    tick_interval: "1h"
    max_failures_retained: 100
    rules:
      - name: expire-nightly-dumps
        enabled: false
        bucket: db-archive
        prefix: "nightly/"
        action: delete
        expire_after: "90d"
        include_globs: ["nightly/**/*.dump"]
        exclude_globs: [".deltaglider/**", "nightly/golden/**"]
```

Use `POST /_/api/admin/jobs/lifecycle:<name>/preview` (or the Preview button on the Jobs screen) before enabling a rule. See [Lifecycle rules](lifecycle.md) for API details, skip rules, and limitations.

## Job leases

A background job holds a lease while it runs, so that two runners never run the same job at the same time. The runner renews the lease at a fixed interval. When a runner dies, its lease lapses after the TTL, and another runner can then take the job. `advanced.jobs` sets one TTL and one renewal interval for the leases of maintenance jobs, lifecycle rules, parity audits and rule deletes.

```yaml
# validate
advanced:
  jobs:
    lease_ttl: "2m"
    heartbeat_interval: "40s"
```

| Field | Default | Meaning |
|---|---|---|
| `lease_ttl` | per job kind: maintenance `60s`, lifecycle `5m`, parity audit `30m`, rule delete `60s` | How long one renewal holds the lease (humantime, minimum `15s`). A shorter TTL lets another runner take a dead runner's job sooner. A longer TTL gives a slow runner more time between renewals. |
| `heartbeat_interval` | a third of `lease_ttl` when you set `lease_ttl`, else per job kind: maintenance `20s`, lifecycle `60s`, parity audit `10m` | How often a running job renews its lease (humantime, minimum `5s`). It must be lower than `lease_ttl`. The proxy replaces a value at or above it with half the TTL. |

The proxy ignores a value that does not parse, or that is below the minimum, and logs a config warning. The default of the job kind then applies. Replication rules do not use `advanced.jobs`: they keep `storage.replication.lease_ttl` and `heartbeat_interval` (see [Replication](replication.md)), because their lease can be a cross-instance S3 lease with its own failover window.

## Event delivery

The proxy always appends durable object mutation events to the encrypted config
DB when the DB is available. HTTP delivery is disabled by default. When you
enable `advanced.event_delivery`, a background dispatcher starts, and it POSTs
each event to every configured webhook endpoint.

```yaml
# validate
advanced:
  event_delivery:
    enabled: true
    webhook_url: "https://events.example.com/deltaglider"
    webhook_urls:
      - "https://audit.example.com/deltaglider"
    webhook_headers:
      authorization: "Bearer redacted-token"
      x-dgp-env: "prod"
    tick_interval: "10s"
    batch_size: 50
    request_timeout: "5s"
    max_attempts: 8
    retry_base: "5s"
    retry_max: "5m"
    stale_claim_after: "60s"
    delivered_retention: "24h"
    delivered_max_rows: 10000
    prune_batch: 100
```

`webhook_url` is the single-endpoint shortcut. `webhook_urls` adds fan-out
endpoints, and the proxy attaches `webhook_headers` to every delivery request.
The proxy marks a row delivered only after all endpoints return 2xx. A failed
row backs off, and you can requeue it from the admin API or UI. See [Event log](event-outbox.md)
for payload and diagnostics details.

### Slack format

Set `format: slack` to render each event as a Slack message (Block Kit + text
fallback) instead of the raw `{schema,event}` envelope. There are two modes, and
you pick one:

```yaml
# validate
advanced:
  event_delivery:
    enabled: true
    format: slack
    # Incoming Webhook mode (simplest, single channel):
    webhook_url: "https://hooks.slack.com/services/T000/B000/XXXX"
    slack_username: "DeltaGlider"        # optional cosmetic override
    slack_icon_emoji: ":package:"        # optional
    # Bot-token mode (multi-channel + @mentions) — set these INSTEAD of webhook_url:
    # slack_bot_token: "xoxb-..."        # needs chat:write + chat:write.public scopes
    # slack_channel: "C0123456"          # channel id or #name (required in this mode)
    # Scope what gets posted:
    slack_notify_kinds: ["ObjectCreated"]   # add ObjectDeleted, etc.
    slack_include_globs: ["firmware/**"]    # empty = all user objects
    slack_exclude_globs: ["**/*.tmp"]       # exclude wins over include
    # Per-bucket/prefix routing (bot-token mode only):
    slack_routes:
      - name: "Releases → #ci"
        bucket: releases
        prefix_globs: ["firmware/**"]    # empty = any key in the bucket
        channel: "C_CI"
```

| Key | Notes |
|-----|-------|
| `format` | `raw` (default) or `slack`. |
| `slack_bot_token` | `xoxb-…` Slack Web API token. It is a secret: the export masks it, and an untouched round-trip keeps it. It selects bot-token mode (`chat.postMessage`). |
| `slack_channel` | Target channel (`C0123` or `#name`). Required in bot-token mode; ignored for Incoming Webhook URLs (each URL is bound to one channel by Slack). |
| `slack_username` / `slack_icon_emoji` | Cosmetic sender overrides (Incoming Webhook mode). |
| `slack_notify_kinds` | Which event kinds post. Default `["ObjectCreated"]`. |
| `slack_include_globs` / `slack_exclude_globs` | Key-glob pre-filter (exclude wins). |
| `slack_routes` | Routing from a bucket or prefix to a channel (bot-token mode only). When non-empty, an eligible event posts to every matching route; `slack_channel` is the fallback for events matching no route. |

You can edit all of these settings in the admin GUI at **Integrations →
Event delivery** (set the format to *Slack*). See [Event log](event-outbox.md#slack-format)
for delivery semantics.

## Encryption at rest

Per-backend encryption with four modes: `none`, `aes256-gcm-proxy`, `sse-kms`, `sse-s3`. Each backend has its own `encryption` block. Operators can therefore mix modes (e.g. SSE-KMS for the production backend and plaintext for a public-CDN backend), and no single key has every backend in its blast radius.

YAML for the named-backends path:

```yaml
# validate
storage:
  backends:
    - name: hetzner-fsn1
      type: s3
      # endpoint, region, credentials …
      encryption:
        mode: aes256-gcm-proxy
        key: "${env:DGP_BACKEND_HETZNER_FSN1_ENCRYPTION_KEY}"
        key_id: hetzner-2026-06   # optional; derived from SHA-256(name + key) when absent
    - name: aws-dr
      type: s3
      # region, credentials …
      encryption:
        mode: sse-kms
        kms_key_id: arn:aws:kms:eu-west-1:123456789012:key/abcd-ef01
        bucket_key_enabled: true
```

YAML for the singleton-backend path (`backends:` empty):

```yaml
# fragment
storage:
  backend: { ... }
  backend_encryption:
    mode: aes256-gcm-proxy
    key: "${env:DGP_ENCRYPTION_KEY}"
```

Environment variables (infra secrets) are the recommended key source. Canonical exports strip every `key` / `kms_key_id` field from the YAML.

| Env var | Binds to |
|---|---|
| `DGP_ENCRYPTION_KEY` | `backend_encryption.key` (singleton path) |
| `DGP_BACKEND_<NAME>_ENCRYPTION_KEY` | `backends[name=<NAME>].encryption.key` |
| `DGP_SSE_KMS_KEY_ID` | `backend_encryption.kms_key_id` (singleton SSE-KMS) |
| `DGP_BACKEND_<NAME>_SSE_KMS_KEY_ID` | named SSE-KMS override |

Name normalisation: the proxy uppercases `<NAME>` and replaces `-` and `.` with `_`. For example, `hetzner-fsn1` becomes `DGP_BACKEND_HETZNER_FSN1_ENCRYPTION_KEY`.

When the `encryption` block is absent, the mode is `none` (plaintext).

`key` / `legacy_key` are 64-character lowercase hex strings (256 bits). `kms_key_id` is a KMS ARN or alias. `key_id` (optional) must match `[A-Za-z0-9_.-]{1,64}` (S3 user-metadata header-safe).

The proxy does not automate key rotation within a single mode. To rotate, use the `legacy_key` / `legacy_key_id` shim fields (decrypt-only, for proxy→native transitions), or copy the objects to a new backend. See the [encryption reference](encryption.md) for the full wire format, key-id mismatch mechanics, and the shim lifecycle.

## CLI subcommands

See [Command-line tools](cli.md).

## Full example

This YAML example covers every top-level section. The fields that it leaves out keep their defaults.

```yaml
# validate
# deltaglider_proxy.yaml

# Request rules (checked before authentication)
admission:
  blocks:
    - name: deny-known-bad-ips
      match:
        source_ip_list: ["198.51.100.0/24"]
      action: deny

    - name: allow-public-zips
      match:
        method: [GET, HEAD]
        bucket: releases
        path_glob: "*.zip"
      action: allow-anonymous

# SigV4 credentials + IAM mode
access:
  iam_mode: gui               # or declarative
  access_key_id: admin
  secret_access_key: changeme

# Backends + per-bucket overrides
storage:
  default_backend: hetzner-fsn1
  backends:
    - name: hetzner-fsn1
      type: s3
      endpoint: https://fsn1.your-objectstorage.com
      region: fsn1
      force_path_style: true
      access_key_id: HETZNER_KEY
      secret_access_key: HETZNER_SECRET
    - name: aws-dr
      type: s3
      endpoint: https://s3.eu-west-1.amazonaws.com
      region: eu-west-1
      access_key_id: AWS_KEY
      secret_access_key: AWS_SECRET
  buckets:
    releases:
      backend: hetzner-fsn1
      compression: true
    db-archive:
      backend: aws-dr
      alias: acme-db-archive-prod
      compression: false
    downloads:
      public_prefixes: ["public/"]

# Process-level tunables
advanced:
  listen_addr: "0.0.0.0:9000"
  log_level: deltaglider_proxy=info,tower_http=warn
  max_delta_ratio: 0.75
  cache_size_mb: 2048
  metadata_cache_mb: 100
  codec_concurrency: 32
  config_sync_bucket: dgp-iam-sync
  event_delivery:
    enabled: true
    webhook_urls:
      - "https://audit.example.com/deltaglider"
    webhook_headers:
      authorization: "Bearer redacted-token"
  tls:
    enabled: true
    cert_path: /etc/ssl/certs/proxy.pem
    key_path: /etc/ssl/private/proxy-key.pem
```

Equivalent environment variables for container deployments:

```bash
DGP_LISTEN_ADDR=0.0.0.0:9000
DGP_MAX_DELTA_RATIO=0.75
DGP_MAX_OBJECT_SIZE=104857600
DGP_CACHE_MB=2048
DGP_METADATA_CACHE_MB=100
DGP_CODEC_CONCURRENCY=32
DGP_LOG_LEVEL=deltaglider_proxy=info,tower_http=warn
DGP_ACCESS_KEY_ID=admin
DGP_SECRET_ACCESS_KEY=changeme
DGP_BOOTSTRAP_PASSWORD_HASH=JDJiJDEyJENYbDVPRm84bDg2...
DGP_CONFIG_SYNC_BUCKET=dgp-iam-sync
DGP_S3_ENDPOINT=https://fsn1.your-objectstorage.com
DGP_S3_REGION=fsn1
DGP_S3_PATH_STYLE=true
DGP_BE_AWS_ACCESS_KEY_ID=HETZNER_KEY
DGP_BE_AWS_SECRET_ACCESS_KEY=HETZNER_SECRET
DGP_TLS_ENABLED=true
DGP_TLS_CERT=/etc/ssl/certs/proxy.pem
DGP_TLS_KEY=/etc/ssl/private/proxy-key.pem
```

## Environment variable registry

This section lists the `DGP_*` variables that the server reads. The unit test `every_dgp_literal_in_src_is_registered` in `src/config/tests/general.rs` scans the source code and fails when the code reads a variable that `ENV_VAR_REGISTRY` does not list. `deltaglider_proxy --show-env` prints that registry.

The server reads these variables when it starts, and again when an admin apply rebuilds the engine. A request never reads the environment itself: it uses the values of the running config. The environment of a running process does not change, so a changed variable takes effect only after a restart.

### Server / advanced variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_CONFIG` | auto | Path to the YAML config file (`.yaml` / `.yml`) |
| `DGP_LISTEN_ADDR` | `0.0.0.0:9000` | HTTP listen address |
| `DGP_LOG_LEVEL` | `deltaglider_proxy=info,tower_http=info` | Tracing filter (overridden by `RUST_LOG`) |
| `DGP_LOG_FORMAT` | `text` | Stdout log format: `text` or `json` (one JSON object per line) |
| `DGP_LOG_RING_SIZE` | `2000` | In-memory operational-log ring capacity (admin Logs viewer) |
| `DGP_LOG_RING_LEVEL` | `info` | Minimum severity captured into the log ring/stream |
| `DGP_AUDIT_RING_SIZE` | `500` | In-memory audit ring capacity (admin Audit log viewer) |
| `DGP_BLOCKING_THREADS` | 512 | Max tokio blocking threads |
| `DGP_REQUEST_TIMEOUT_SECS` | 300 | Per-request timeout (returns 504) |
| `DGP_READY_TIMEOUT_SECS` | 3 | Per-attempt backend timeout for the `/_/ready` probe |
| `DGP_READY_RETRIES` | 2 | Extra `/_/ready` backend attempts before reporting not-ready (short backoff) |
| `DGP_READY_CACHE_TTL_SECS` | 0 | Last-known-good window for `/_/ready`, in seconds. `0` keeps the strict behaviour: the `ListBuckets` probe must succeed or the node reports not-ready. When you set a value above zero and the list fails, the proxy first tries a much cheaper `HeadBucket` reachability check, and then accepts a backend request that succeeded within this many seconds. A storage provider that throttles `ListBuckets` therefore does not pull a node out of rotation while that node is still serving reads and writes. |
| `DGP_MAX_CONCURRENT_REQUESTS` | 1024 | Tower concurrency limit |
| `DGP_MAX_MULTIPART_UPLOADS` | 1000 | Concurrent multipart upload cap |
| `DGP_DEBUG_HEADERS` | false | Expose fingerprinting headers |
| `DGP_CORS_PERMISSIVE` | false | Enable permissive CORS (dev only) |
| `DGP_METRICS_EXPOSE_VERSION` | false | Put the exact build version in the `version` label of `deltaglider_build_info` on the unauthenticated `/_/metrics` endpoint. Off by default so that anonymous callers cannot fingerprint the deployment; the version stays available through the authenticated admin API |
| `DGP_METRICS_BEARER_TOKEN` | unset | When set, `/_/metrics` answers only to `Authorization: Bearer <token>` (the Prometheus `authorization:` scrape setting) or to an admin session. Unset keeps the scrape endpoint public. See [Monitor with Prometheus](../how-to/monitor-with-prometheus.md) |
| `DGP_USAGE_CACHE_TTL_SECS` | 300 | Lifetime of a cached prefix-usage scan result, in seconds |
| `DGP_REFERENCE_SCAN_LIMIT` | built-in cap | Maximum number of reference baselines that the savings panel reads for one request |
| `DGP_RELAY_FOREIGN_MIN_AGE_SECS` | 3600 | Minimum age, in seconds, before startup removes a multipart relay directory that another process left behind. The relay directories are in `DGP_SPOOL_DIR`; startup also sweeps the relay directory of earlier releases in the system temp dir |

### Delta engine variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_MAX_DELTA_RATIO` | 0.75 | Keep delta only if `delta/original < ratio` |
| `DGP_MAX_OBJECT_SIZE` | 104857600 | Largest object that a client can upload (the request body limit), and the largest object that the proxy delta-encodes; see [`max_object_size`](#max_object_size) |
| `DGP_CACHE_MB` | 100 | Reference cache size in MB |
| `DGP_SPOOL_DIR` | `<system temp>/dgp-spool` | Directory for every scratch file of the proxy: the delta codec's files, the multipart relay parts, and the temporary files of encrypted uploads |
| `DGP_SPOOL_MAX_BYTES` | 17179869184 (16 GiB) | Byte budget for all the files in `DGP_SPOOL_DIR`. A request that needs spool space while it holds none waits for it (up to `DGP_SPOOL_ACQUIRE_TIMEOUT_SECS`). A request that already holds spool space, or that holds a lock, does not wait: it fails with `503 SlowDown`, and S3 clients retry it. A relayed multipart upload holds its parts here until it completes; see `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES` |
| `DGP_RANGE_SPOOL_TTL_SECS` | 60 | Seconds that a verified reconstruction of a large delta object stays in the spool for more range reads of it; see [`range_spool_ttl_secs`](#range_spool_ttl_secs). `0` turns it off |
| `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES` | half of `DGP_SPOOL_MAX_BYTES` (8 GiB with the default) | The most spool space that one relayed multipart upload can hold. The proxy stages every client multipart upload itself. An upload that can become a delta keeps its parts in memory until it holds more than 64 MiB (`DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES`), and then the proxy moves them to files in `DGP_SPOOL_DIR` (the relay). An upload that cannot become a delta (a key that is not delta-eligible, the `dg-no-delta: true` hint, or a bucket with compression disabled) writes every part to the relay from the first part. So this limit applies to every such upload, and to every other client multipart upload larger than 64 MiB, on every backend. Only the copies that replication and lifecycle make use the backend's own multipart upload, and they use no spool space. A part that would take one upload past this limit fails with `413 EntityTooLarge`, and the error message names this variable and the limit. Set `0` to remove the per-upload limit; then only `DGP_SPOOL_MAX_BYTES` applies, and one large upload can use the whole budget, so the requests of other clients that need spool space fail with `503 SlowDown` until it completes. The largest object is also limited by `max_object_size` |
| `DGP_METADATA_CACHE_MB` | 50 | `FileMetadata` cache size in MB (0 to disable) |
| `DGP_FILTERED_LIST_MAX_ENGINE_PAGES` | 50 | Backend pages one filtered LIST request may read before it fails with `400 InvalidRequest` (see `filtered_list_max_engine_pages`) |
| `DGP_LIST_SIZE_CACHE_MB` | 32 | Listing-size cache in MB: the original size and ETag of stored deltas, for listings |
| `DGP_LISTING_FACTS_GC` | true | On an S3 backend, every six hours each instance reads part of each bucket's listing-facts namespace (`.dg/facts/`) and deletes the entries whose object is gone or was overwritten. An entry younger than one hour is never deleted. Set `false` to turn this off, for example to save the requests on a large bucket |
| `DGP_CODEC_CONCURRENCY` | `num_cpus * 4` (min 16) | Max concurrent xdelta3 subprocesses |
| `DGP_CODEC_TIMEOUT_SECS` | 60 | Per-subprocess timeout |
| `DGP_CODEC_STALL_SECS` | 30 | Streaming codec: the proxy stops an xdelta3 process that makes no progress for this many seconds |
| `DGP_CODEC_ABSOLUTE_SECS` | 7200 | Streaming codec: the longest time one operation may take, in seconds, even while it makes progress |
| `DGP_SPOOL_ACQUIRE_TIMEOUT_SECS` | 120 | How long, in seconds, a request that needs spool space (a large PUT or POST, a copy, a delta GET) waits for spool budget before it fails with `503 SlowDown` |
| `DGP_SPOOL_THRESHOLD_BYTES` | 16 MiB (`16777216`), or `max_object_size` when that is smaller | A delta GET of an object larger than this reconstructs the object to a spool file and streams the file, instead of reconstructing it in memory. A delta-eligible upload larger than this is encoded from a spool file. Objects of this size or smaller use the in-memory path |
| `DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES` | 64 MiB | Largest multipart upload that CompleteMultipartUpload assembles in memory to try a delta. The parts stay in memory up to this total, then go to relay files in the spool. A larger upload, or one that tries no delta, is stored from its parts without a delta |

### Storage variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_DATA_DIR` | `./data` | Filesystem backend data directory |
| `DGP_S3_ENDPOINT` | — | S3 endpoint (activates S3 backend when set) |
| `DGP_S3_REGION` | `us-east-1` | AWS region |
| `DGP_S3_PATH_STYLE` | true | Use path-style URLs (MinIO/LocalStack) |
| `DGP_BE_AWS_ACCESS_KEY_ID` | — | Backend S3 access key |
| `DGP_BE_AWS_SECRET_ACCESS_KEY` | — | Backend S3 secret key |
| `DGP_MAX_PASSTHROUGH_OBJECT_SIZE` | 64 GiB | Largest passthrough (non-delta) object that the proxy stores from a spool file or a streaming copy: replication, lifecycle transitions, bucket migrations, copies and the `s3` CLI verbs. A client upload is limited first by `DGP_MAX_OBJECT_SIZE` |
| `DGP_S3_CONNECT_TIMEOUT_SECS` | 10 | Backend S3 connect timeout |
| `DGP_BACKEND_REQUEST_TIMEOUT_SECS` | 30 | Deadline for one S3-backend request that carries no large body (HEAD, GET until the first byte, LIST, DELETE), including the SDK's retries. When a backend does not answer in time, the client gets `503 ServiceUnavailable` naming the backend, and the proxy marks the backend unhealthy, so the next requests to its buckets get the 503 at once. Uploads and server-side copies are not capped by this value, because their duration grows with the object size. `0` turns the deadline off |
| `DGP_BOOT_CREATE_DECLARED_BUCKETS` | true | At startup, create every bucket declared under `storage.buckets` that its backend does not have: a directory on a filesystem backend, or a `CreateBucket` (after a `HeadBucket`) on an S3 backend. A failure is a warning, not fatal. `false` turns this off for every backend |
| `DGP_BACKEND_HEALTH_INTERVAL_SECS` | 30 | How often the proxy probes every storage backend, healthy or not. A backend that hangs or goes down turns unhealthy within one interval even with no traffic, and an unhealthy backend reopens when a probe succeeds. `0` turns the loop off; `DGP_BOOT_BACKEND_PROBE=off` also turns it off |
| `DGP_S3_READ_TIMEOUT_SECS` | 60 | Backend S3 read timeout |
| `DGP_S3_OPERATION_ATTEMPT_TIMEOUT_SECS` | 300 | Per-attempt backend S3 operation timeout |
| `DGP_S3_STALL_GRACE_SECS` | 20 | Backend S3 no-progress stall grace |
| `DGP_PARITY_HEAD_CONCURRENCY` | 15 | Concurrent HEADs during a replication Verify (parity) audit on an S3 backend; raise to speed up a large audit, lower to be gentler on a throttling backend (clamped to 1 to 64) |
| `DGP_PARITY_MAX_OBJECTS` | 1000000 | Max objects a Verify audit scans across both sides before it caps and reports a partial ("scan capped") result. It is a safety ceiling against a runaway scan (about 500k objects per side). Raise it for larger mirrors (minimum 1000) |
| `DGP_BOOT_BACKEND_PROBE` | enforce | Boot-time backend health gate: `enforce` probes the connectivity and credentials of every configured backend at startup, and refuses to start when all of them fail; `warn` probes and logs but never exits; `off` skips probing. Unhealthy backends' buckets answer 503 until recovery (every backend is re-probed every `DGP_BACKEND_HEALTH_INTERVAL_SECS`) |
| `DGP_BACKEND_LIST_COOLDOWN_SECS` | 30 | After a backend fails a bucket listing, skip it (serve last-known-good, flagged unavailable) for this long before it probes the backend again. This way, one dead backend does not add a connect timeout to every `ListBuckets` |
| `DGP_BACKEND_LIST_TIMEOUT_SECS` | 5 | Per-backend timeout for a single bucket-listing request. It bounds the wait for a hung (not refusing) backend |
| `DGP_BACKEND_LIST_FRESH_SECS` | 5 | The browser sends `ListBuckets` and the origins lookup back to back. The proxy serves a bucket listing that it fetched this recently without probing upstream again, so the two requests become one upstream request per backend. A bucket create or delete through the proxy invalidates the listing immediately. `0` disables it |

### Replication / streaming copy variables

These variables tune the streaming multipart copy of large objects (replication and lifecycle transitions). The defaults suit most deployments. Raise the concurrency only when the backend and the network have headroom.

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_STREAM_COPY_THRESHOLD` | 64 MiB | Object size at/above which a passthrough copy streams via multipart (floored at 1) |
| `DGP_MULTIPART_PART_SIZE` | 64 MiB | Part size for the streaming copy (clamped to the 5 MiB S3 minimum) |
| `DGP_UPLOAD_CONCURRENCY` | 4 | In-flight parts per streaming object. Overrides `storage.replication.upload_concurrency` (1 to 16). It is also the value for every other streaming copy: event-driven replication, lifecycle transitions, bucket migrations and admin copies |
| `DGP_REPLICATION_TRANSFERS` | 4 | Concurrent objects per replication run. Overrides `storage.replication.transfers` (1 to 64) |

### Authentication variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_AUTHENTICATION` | — | `"none"` for open access; absent = auto-detect |
| `DGP_ACCESS_KEY_ID` | — | Proxy SigV4 access key |
| `DGP_SECRET_ACCESS_KEY` | — | Proxy SigV4 secret key |
| `DGP_BOOTSTRAP_PASSWORD_HASH` | auto | Bcrypt hash (legacy alias: `DGP_ADMIN_PASSWORD_HASH`) |
| `DGP_CONFIG_DB_KEY` | key file | Encryption key of the IAM config DB, at least 32 characters; required and identical on every instance when `config_sync_bucket` is set |
| `DGP_CONFIG_DB_KEY_PREVIOUS` | — | The previous config DB key during a key rotation; a DB or synced copy that opens only with it is re-encrypted with `DGP_CONFIG_DB_KEY`. Remove it after the rotation |
| `DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC` | `false` | Only for a rolling upgrade from a release before `DGP_CONFIG_DB_KEY`: accept a synced config DB that opens only with the bootstrap password hash. The proxy logs a warning at start while it is set. Remove it when every instance runs the new release |
| `DGP_BOOTSTRAP_PASSWORD` | — | Plaintext password for admin CLI only |

### Security variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_TRUST_PROXY_HEADERS` | false | Trust `X-Forwarded-For` / `X-Real-IP` from the `DGP_TRUSTED_PROXY_CIDRS` networks for the client address |
| `DGP_TRUSTED_PROXY_CIDRS` | unset | Comma-separated networks of trusted reverse proxies. Only a connection from these networks can name the client in `X-Forwarded-For`. Required when `DGP_TRUST_PROXY_HEADERS=true` |
| `DGP_SESSION_TTL_HOURS` | 4 | Admin session lifetime |
| `DGP_CONFIG_ENV_ALLOWLIST` | — | Comma-separated names (a trailing `*` matches a prefix) that an admin apply, import, restore or section PUT may resolve as `${env:NAME}` from the server environment, in addition to the names that the boot config file uses. A `DGP_*` name never matches a `*` pattern (list it exactly), and a `DGP_*` name that contains `BOOTSTRAP_`, `ENCRYPTION_KEY`, `SECRET`, `DB_KEY`, `PASSWORD` or `TOKEN` never matches |
| `DGP_CLOCK_SKEW_SECONDS` | 900 | SigV4 clock skew tolerance |
| `DGP_REPLAY_WINDOW_SECS` | 2 × clock skew (1800) | SigV4 replay detection window for mutating requests (0 disables) |
| `DGP_SECURE_COOKIES` | auto | `Secure` flag on session cookies: `true` always, `false` never; unset = when the listener serves TLS or a trusted `X-Forwarded-Proto: https` arrives |
| `DGP_RATE_LIMIT_MAX_ATTEMPTS` | 100 | Max auth failures before lockout |
| `DGP_RATE_LIMIT_WINDOW_SECS` | 300 | Rate-limit rolling window |
| `DGP_RATE_LIMIT_LOCKOUT_SECS` | 600 | Lockout duration |
| `DGP_RATE_LIMIT_ACCOUNT_MAX_ATTEMPTS` | 10 | Failed logins for one account, from any IP address, before that account locks |
| `DGP_RATE_LIMIT_ACCOUNT_WINDOW_SECS` | 3600 | Rolling window for the per-account count of failed logins |
| `DGP_RATE_LIMIT_ACCOUNT_LOCKOUT_SECS` | 3600 | Per-account lockout duration |

### TLS / config sync / encryption at rest / misc variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_TLS_ENABLED` | false | Enable HTTPS |
| `DGP_TLS_CERT` | auto self-signed | PEM cert path |
| `DGP_TLS_KEY` | auto self-signed | PEM key path |
| `DGP_CONFIG_SYNC_BUCKET` | — | S3 bucket for encrypted-DB multi-instance sync |
| `DGP_CONFIG_SYNC_KEY` | `.deltaglider/config.db` | Object key of the synced config DB inside the sync bucket (`advanced.config_sync_object_key`) |
| `DGP_REFERENCE_LOCK_TTL_SECS` | 120 | Lifetime of the cross-instance `reference.bin` lock, when config sync is on |
| `DGP_REFERENCE_LOCK_ACQUIRE_TIMEOUT_SECS` | 30 | How long a PUT waits for the cross-instance reference lock before it fails |
| `DGP_NODE_ID` | `HOSTNAME`, else generated | Stable node label for coordination leases. When `DGP_NODE_ID` is unset, the proxy uses the `HOSTNAME` variable. When that is unset too, the proxy generates an id and saves it in the `node-id` file next to the config database. A restarted instance takes back its own live lease at once only when the `boot-id` file next to the config database names the process that wrote the lease, so two live instances with one node id never take a lease from each other |
| `DGP_BUCKET_USAGE_FLUSH_SECS` | 10 | How often the per-bucket usage counters are written to their own file (`deltaglider_usage.db`), in seconds |
| `DGP_ENCRYPTION_KEY` | — | Singleton-backend AES-256 key (64-char hex). Named backends use `DGP_BACKEND_<NAME>_ENCRYPTION_KEY`. |
| `DGP_SSE_KMS_KEY_ID` | — | Singleton-backend SSE-KMS ARN/alias. Named backends use `DGP_BACKEND_<NAME>_SSE_KMS_KEY_ID`. |

### Consumed only by tests / build

| Variable | Consumer |
|----------|----------|
| `DGP_BUILD_TIME` | `build.rs` (compile-time timestamp) |
| `DGP_BUCKET` | historical comment in tests; no longer read |
