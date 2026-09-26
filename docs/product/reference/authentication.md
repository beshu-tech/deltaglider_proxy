# Authentication and access

Reference for the proxy's authentication modes, the bootstrap password, SigV4 verification, backend credentials, client configuration, anonymous access, and error responses.

## Authentication modes

| Mode | Activated by | What is verified |
|------|--------------|------------------|
| **Bootstrap** | A single credential pair in `access.access_key_id` / `access.secret_access_key` (env: `DGP_ACCESS_KEY_ID` / `DGP_SECRET_ACCESS_KEY`). Default on a fresh install. | SigV4 signature against the shared secret. Admin GUI access requires the bootstrap password. |
| **IAM** | One or more IAM users in the encrypted config DB (`deltaglider_config.db`). Activates when the first user is created — via admin GUI, declarative YAML, or OAuth auto-provisioning. | SigV4 signature against the per-user secret looked up by access key ID, then ABAC permission evaluation. Admin GUI access is permission-based. |
| **OAuth/OIDC** | A configured provider (Google or any OIDC-compliant issuer). | The provider's JWT: algorithm from header, audience, issuer, nonce; the flow uses PKCE and a state parameter. Applies to browser sessions only — the S3 API remains SigV4. Logged-in users are provisioned as IAM users; permissions come from group mapping rules. |
| **Open access** | `access.authentication: none` (env: `DGP_AUTHENTICATION=none`). | Nothing. No SigV4 verification. Development only. |

The proxy refuses to start without authentication credentials unless `authentication: none` is set explicitly. Bootstrap and IAM coexist: the request's access key is tried against both the bootstrap pair and the IAM table. When the first IAM user is created, the bootstrap credentials are carried over as a `legacy-admin` user.

```yaml
# validate
access:
  access_key_id: dgp-shared-key
  secret_access_key: dgp-shared-secret
```

The orthogonal `access.iam_mode` selector (`gui`, default, or `declarative`) controls where IAM state lives — the encrypted DB or the YAML file. In `declarative` mode, admin-API IAM mutation routes return `403 { "error": "iam_declarative" }` and the YAML is reconciled into the DB on every config apply. See [Declarative IAM](declarative-iam.md).

OAuth providers appear as buttons on the `/_/` login page:

![OAuth login with Google SSO](/_/screenshots/oauth_login.jpg)

## Bootstrap password

One infrastructure secret with two roles:

| Role | Mechanism |
|------|-----------|
| Signs admin session cookies | HMAC-based session authentication for the admin GUI |
| Gates admin GUI access | Required to open settings in bootstrap mode (before IAM users exist) |

The bootstrap password does **not** encrypt the config database. The database has its own key, which the next section describes. So a change of the bootstrap password never makes the IAM database unreadable.

Generation and reset facts:

- **Auto-generated** on first run when not set. The plaintext is printed to stderr only when stderr is a TTY. In containers and CI, the proxy does not print the password, because captured logs are kept. The hash is saved to `.deltaglider_bootstrap_hash` (mode 0600). To get a password that you know, run `--set-bootstrap-password`, or set `DGP_BOOTSTRAP_PASSWORD_HASH` before the first start.
- **Set explicitly** via `DGP_BOOTSTRAP_PASSWORD_HASH` (bcrypt, or base64-encoded bcrypt to avoid `$` escaping in Docker). YAML: `advanced.bootstrap_password_hash`. Legacy alias: `DGP_ADMIN_PASSWORD_HASH`.
- **Reset** via the `--set-bootstrap-password` CLI flag, which reads the new plaintext from stdin. The IAM database keeps all of its users, OAuth providers, and group mappings. If the database is still encrypted with the old hash (a database from a release before the config DB key), the flag first re-encrypts it with the config DB key, and only then writes the new hash.
- **Change at runtime**: `PUT /_/api/admin/password` verifies the current password and writes the new hash to `.deltaglider_bootstrap_hash`. When `DGP_BOOTSTRAP_PASSWORD_HASH` is set, this request fails with `409`, because the variable sets the hash again at every start and the change would be lost at the next start. In that case, run `--set-bootstrap-password` to get the new hash, set the variable to it on every instance, and restart.

## Config database key

The encrypted config database (`deltaglider_config.db`, SQLCipher) holds IAM users, groups, OAuth providers, and group mapping rules. The proxy takes its key from the first of these sources:

| Source | When to use it |
|--------|----------------|
| `DGP_CONFIG_DB_KEY` | Required when `config_sync_bucket` is set, and it must have the same value on every instance. Use at least 32 characters, for example the output of `openssl rand -hex 32`. |
| Key file `deltaglider_config.db.key` | The default for a single instance. The proxy generates a random key into this file (mode 0600) on the first start, next to the database. |

Facts about the key:

- **Back up the key with the database.** A copy of `deltaglider_config.db` is useless without its key. If you lose the key, the IAM data cannot be recovered.
- **An empty or unreadable key file stops the start.** The proxy never replaces an existing key file, because a new key would make the database unreadable.
- **Upgrade from a release before the config DB key**: those releases encrypted the database with the bootstrap password hash. On the first start, the proxy opens the database with that hash and re-encrypts it with the new key. The re-encryption works on a copy, and the copy replaces the original only after it opens with the new key. If a step fails, the original database stays unchanged and the next start tries again.
- **Move from the key file to `DGP_CONFIG_DB_KEY`**: set the variable and restart. The proxy opens the database with the key file and re-encrypts it with the variable's value.
- **Rotate the key**: set `DGP_CONFIG_DB_KEY` to the new key and `DGP_CONFIG_DB_KEY_PREVIOUS` to the old one, then restart. The proxy opens the database with the previous key and re-encrypts it, and the sync merge base next to it, with the new key. The steps for one instance:
  1. Generate a new key, for example with `openssl rand -hex 32`.
  2. Set `DGP_CONFIG_DB_KEY_PREVIOUS` to the current key: the old value of `DGP_CONFIG_DB_KEY`, or the content of `deltaglider_config.db.key` when the variable was unset.
  3. Set `DGP_CONFIG_DB_KEY` to the new key, and restart.
  4. Check the log line `re-encrypted with DGP_CONFIG_DB_KEY`. Then remove `DGP_CONFIG_DB_KEY_PREVIOUS`, restart, and store the new key with your database backups.

  For several instances with a sync bucket, see [How to run multiple instances](../how-to/run-multiple-instances.md#rotate-the-config-db-key).
- **A key that opens nothing**: when no key opens the database, the proxy keeps the database as `deltaglider_config.db.bak`, starts with an empty database, and locks the S3 API (`503`) until you restore the right key and restart. The admin GUI has a recovery wizard that tells you whether a candidate key opens the preserved database.
- **A synced copy opens only with a real key.** A database that an instance downloads from `config_sync_bucket` must open with `DGP_CONFIG_DB_KEY` or `DGP_CONFIG_DB_KEY_PREVIOUS`. The bootstrap password hash opens a synced copy only when `DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC=true`, which is meant for the rolling upgrade from a release before the config DB key. The hash is in configuration files and backups, so it must not open a database that the instances trust.
- **The key is never printed** and never leaves the node: the synced copy in `config_sync_bucket` is encrypted with it.

## SigV4 verification

The proxy verifies SigV4 signatures from two sources:

| Path | Source | Use |
|------|--------|-----|
| Header auth | `Authorization: AWS4-HMAC-SHA256 ...` | Standard S3 SDK calls |
| Presigned URL | `X-Amz-Algorithm` + `X-Amz-Signature` query parameters | Browser downloads, shareable links |

Both paths extract the access key ID, resolve the user (bootstrap pair or IAM lookup), and verify the HMAC-SHA256 signature against the secret key using constant-time comparison. Any region is accepted in the credential scope. Presigned URLs expire after at most 7 days (604,800 s) and carry the signing user's permissions — Deny rules apply to presigned requests too.

### Verify, then re-sign

SigV4 signatures are bound to the Host header and URI path, so the client's signature cannot be forwarded. The proxy verifies it, discards it, and issues its own authenticated requests to the backend using the backend credentials via the AWS SDK.

### Replay detection

The signatures of verified mutating requests (PUT/POST/DELETE) are cached, and a duplicate signature within the replay window is rejected with 400. The window defaults to the clock-skew tolerance (`DGP_CLOCK_SKEW_SECONDS`, default 900 s), so a captured mutating request is refused for as long as its signature is valid; `DGP_REPLAY_WINDOW_SECS` sets another window, and `0` disables the check. Idempotent reads (GET/HEAD) are not cached, and a duplicate read is served normally. A duplicate PUT or DELETE that arrives less than one second after the first copy is also served: SigV4 timestamps have 1-second granularity, so an SDK that retries inside the second in which it signed the request (for example after a load balancer lost the response) sends the same signature. The proxy measures that second on its own clock from the first copy, so client clock skew cannot stretch it. A duplicate that arrives later is rejected, and POST requests stay strict. Only a request that succeeds (2xx or 3xx) keeps its signature in the cache, so an SDK retry of a failed request (for example a `503 SlowDown`) is not treated as a replay. Replay rejections do not count toward the auth-failure lockout. The cache is per instance. Full table: [Rate limits and concurrency](rate-limits.md).

## Backend credentials

Two independent credential sets exist: client-to-proxy credentials (above) and proxy-to-backend credentials. For an S3 backend:

```bash
export DGP_BE_AWS_ACCESS_KEY_ID=hetzner-backend-key
export DGP_BE_AWS_SECRET_ACCESS_KEY=hetzner-backend-secret
export DGP_S3_ENDPOINT=https://fsn1.your-objectstorage.com
```

YAML equivalents are `storage.access_key_id` / `storage.secret_access_key` (shorthand) or `storage.backend.*` (canonical). With multiple named backends (for example `hetzner-fsn1`, `aws-dr`), each entry in `storage.backends[]` carries its own `access_key_id` / `secret_access_key`. Filesystem backends such as `local-disk` require no credentials. See [Configuration](configuration.md) for the full field table.

## Client configuration

Any SigV4 client (aws CLI, boto3, Terraform, rclone, Cyberduck) works against the proxy with an endpoint override and proxy credentials.

aws CLI:

```bash
export AWS_ACCESS_KEY_ID=ci-uploader-key
export AWS_SECRET_ACCESS_KEY=ci-uploader-secret
export AWS_ENDPOINT_URL=http://localhost:9000

aws s3 cp fw-2.4.0.tar s3://releases/firmware/widget-3000/fw-2.4.0.tar
aws s3 ls s3://releases/firmware/widget-3000/
aws s3 presign s3://releases/firmware/widget-3000/fw-2.4.0.tar --expires-in 3600
```

boto3:

```python
import boto3

s3 = boto3.client(
    "s3",
    endpoint_url="https://s3.acme.example",
    aws_access_key_id="ci-uploader-key",
    aws_secret_access_key="ci-uploader-secret",
    region_name="us-east-1",
)

s3.upload_file("dump.sql.gz", "db-archive", "nightly/dump.sql.gz")
```

## Public prefixes

Per-bucket configuration grants anonymous read-only access to specific prefixes:

```yaml
storage:
  buckets:
    downloads:
      public_prefixes: ["public/"]
    docs-site:
      public: true        # shorthand for public_prefixes: [""]
```

Semantics for requests without credentials:

- **Allowed**: GET and HEAD on objects under a public prefix; LIST with a `prefix` parameter inside the public prefix. LIST results are scoped to the public prefix — never the whole bucket.
- **Denied**: PUT, DELETE, COPY, and multipart uploads, always.
- **Identity**: anonymous requests run as a built-in `$anonymous` user with scoped read+list permissions (including `s3:prefix` conditions for LIST). All anonymous access is audit-logged as `user=$anonymous`.
- **Credentials win**: a request carrying valid SigV4 credentials gets full IAM evaluation regardless of public-prefix configuration.
- **Matching**: a trailing `/` is significant — `public/` matches `public/...` but not `publicity/`. The empty prefix `""` makes the entire bucket public (logged as a startup warning). Prefixes containing `..`, null bytes, or `//` are rejected.

The proxy creates a request rule named `public-prefix:*` for each public prefix and checks these rules after your own `admission.blocks[]` rules (see [Configuration](configuration.md#admission-chain)).

## Error responses

S3-path errors are returned as standard S3 XML error documents:

| Error code | HTTP status | Cause |
|------------|-------------|-------|
| `AccessDenied` | 403 | No credentials on a non-public path (anonymous), or valid credentials without a matching Allow / with a matching Deny (denied) — the response shape is identical in both cases |
| `SignatureDoesNotMatch` | 403 | Signature verification failed |
| `RequestTimeTooSkewed` | 403 | Request timestamp outside the clock-skew tolerance |
| `InvalidArgument` | 400 | Malformed Authorization header, unparseable date/expiry, or replayed mutating signature |
| `SlowDown` | 429 | Per-IP auth rate limit exceeded |

Admin-API errors are JSON, not XML: IAM mutations in declarative mode return `403 { "error": "iam_declarative" }`; admin endpoints without an AdminGui session return `403 { "error": "admin_session_required" }`.

## Related

- [About authentication and access control](../explanation/security-model.md)
- [IAM permissions and conditions](iam-permissions.md)
- [How to create IAM users and groups](../how-to/create-iam-users.md)
