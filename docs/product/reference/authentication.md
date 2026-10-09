# Authentication and access

Reference for the proxy's authentication modes, the bootstrap password, SigV4 verification, backend credentials, client configuration, anonymous access, and error responses.

## Authentication modes

| Mode | Activated by | What is verified |
|------|--------------|------------------|
| **Bootstrap** | A single credential pair in `access.access_key_id` / `access.secret_access_key` (env: `DGP_ACCESS_KEY_ID` / `DGP_SECRET_ACCESS_KEY`). Default on a fresh install. | SigV4 signature against the shared secret. Admin GUI access requires the bootstrap password. |
| **IAM** | One or more IAM users in the encrypted config DB (`deltaglider_config.db`). Activates when the first user is created. The first user can come from the admin GUI, declarative YAML, or OAuth auto-provisioning. | SigV4 signature against the per-user secret looked up by access key ID, then ABAC permission evaluation. Admin GUI access is permission-based. |
| **OAuth/OIDC** | A configured provider of type `oidc` (any OpenID Connect issuer, for example Google, Okta, or Azure AD). | The provider's JWT: algorithm from header, audience, issuer, nonce; the flow uses PKCE and a state parameter. Applies to browser sessions only. The S3 API still uses SigV4. Logged-in users are provisioned as IAM users; permissions come from group mapping rules. Only a user with admin permissions gets an admin session. Every other user gets a browser-only session that opens the file browser and refuses the admin API. The one exception is the bulk copy, move, delete, and ZIP actions of the file browser (`/_/api/admin/objects/*`): they accept a browser-only session and check every key against the user's own IAM permissions, as the S3 API does. |
| **Open access** | `access.authentication: none` (env: `DGP_AUTHENTICATION=none`). | No identity. An unsigned request is served. A signed request is served only when its secret key is the same as its access key (for example `dummy` / `dummy`), because the proxy still checks the signature to read signed and chunked uploads; any other pair gets `403 SignatureDoesNotMatch`. Development only. |

The proxy refuses to start without authentication credentials unless `authentication: none` is set explicitly. IAM users count as credentials: a config with declarative `access.iam_users`, or a config DB that already holds IAM users, starts without a bootstrap SigV4 pair. When IAM users exist, the proxy checks an S3 request only against the IAM users, so the bootstrap pair on its own no longer works. In `gui` mode, the creation of the first IAM user carries the bootstrap pair over as a `legacy-admin` IAM user, so the pair keeps working. In `declarative` mode the YAML file lists every IAM user, so the bootstrap pair works only when one of the `iam_users` carries it. The bootstrap password still opens the admin GUI in both modes. The file browser then gets the bootstrap pair only when the S3 API accepts it; otherwise it asks for S3 credentials.

When IAM users exist, the sign-in page asks for the access key ID and the secret access key of an IAM user:

![The sign-in page of a proxy with IAM users asks for an access key ID and a secret access key; the box marks the two fields.](/_/screenshots/login-iam.webp)

The **Access → Credentials & mode** page (`/_/admin/access/credentials`) shows the configured bootstrap access key ID, because an access key ID is not a secret. The secret is never shown. An empty field on that page keeps the current value, so clearing the fields does not remove the pair. To remove the pair, use **Remove bootstrap credentials** on the same page (`DELETE /_/api/admin/config/bootstrap-credentials`), or delete both keys from the YAML config. The proxy refuses the removal (`409`) while no IAM user exists, because the proxy would then have no credentials to check S3 requests against. When `DGP_ACCESS_KEY_ID` sets the key, the page does not offer the removal, because the environment variable would keep the key in place.

```yaml
# validate
access:
  access_key_id: dgp-shared-key
  secret_access_key: dgp-shared-secret
```

The orthogonal `access.iam_mode` selector (`gui`, default, or `declarative`) controls where IAM state lives: the encrypted DB or the YAML file. In `declarative` mode, admin-API IAM mutation routes return `403 { "error": "iam_declarative" }` and the YAML is reconciled into the DB on every config apply. See [Declarative IAM](declarative-iam.md).

OAuth providers appear as buttons on the `/_/` login page:

![The sign-in page of a proxy with the Okta provider; the arrow points at the Sign in with Okta button above the link to sign in with credentials.](/_/screenshots/sso-login-button.webp)

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
- **An empty or unreadable key file stops the start** when the key file is the only key source. The proxy never replaces an existing key file, because a new key would make the database unreadable. When `DGP_CONFIG_DB_KEY` is set, the key file is only a fallback for the move from the file to the variable, so the proxy logs a warning, leaves the file alone, and starts with the variable.
- **Upgrade from a release before the config DB key**: those releases encrypted the database with the bootstrap password hash. On the first start, the proxy opens the database with that hash and re-encrypts it with the new key. The re-encryption works on a copy, and the copy replaces the original only after it opens with the new key. If a step fails, the original database stays unchanged and the next start tries again.
- **Move from the key file to `DGP_CONFIG_DB_KEY`**: set the variable and restart. The proxy opens the database with the key file and re-encrypts it with the variable's value.
- **Rotate the key**: `DGP_CONFIG_DB_KEY_PREVIOUS` holds the old key during a rotation. A database that opens only with it is re-encrypted with `DGP_CONFIG_DB_KEY` at start, together with the sync merge base next to it. The key exists only in the environment or the key file, so the admin UI and the YAML file cannot rotate it. The steps for one instance and for a fleet are in [How to rotate the config DB key](../how-to/rotate-the-config-db-key.md).
- **A key that opens nothing**: when no key opens the database, the proxy keeps the database as `deltaglider_config.db.bak`, starts with an empty database, and locks the S3 API (`503`) until you restore the right key and restart. The admin GUI has a recovery wizard that tells you whether a candidate key opens the preserved database.
- **A synced copy opens only with a real key.** A database that an instance downloads from `config_sync_bucket` must open with `DGP_CONFIG_DB_KEY` or `DGP_CONFIG_DB_KEY_PREVIOUS`. The bootstrap password hash opens a synced copy only when `DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC=true`, which is meant for the rolling upgrade from a release before the config DB key. The hash is in configuration files and backups, so it must not open a database that the instances trust.
- **The key is never printed** and never leaves the node: the synced copy in `config_sync_bucket` is encrypted with it.

## SigV4 verification

The proxy verifies SigV4 signatures from two sources:

| Path | Source | Use |
|------|--------|-----|
| Header auth | `Authorization: AWS4-HMAC-SHA256 ...` | Standard S3 SDK requests |
| Presigned URL | `X-Amz-Algorithm` + `X-Amz-Signature` query parameters | Browser downloads, shareable links |

Both paths extract the access key ID, resolve the user (bootstrap pair or IAM lookup), and verify the HMAC-SHA256 signature against the secret key using constant-time comparison. Any region is accepted in the credential scope. Presigned URLs expire after at most 7 days (604,800 s) and carry the signing user's permissions. Deny rules apply to presigned requests too.

### Verify, then re-sign

SigV4 signatures are bound to the Host header and URI path, so the client's signature cannot be forwarded. The proxy verifies it, discards it, and issues its own authenticated requests to the backend using the backend credentials via the AWS SDK.

### Replay detection

The signatures of verified mutating requests (PUT/POST/DELETE) are cached, and a duplicate signature within the replay window is rejected with 400. The window defaults to twice the clock-skew tolerance (`DGP_CLOCK_SKEW_SECONDS`, default 900 s). The proxy accepts a signature that is dated up to the skew in the future, so a signature stays valid for up to twice the skew after its first use, and the default window refuses a captured mutating request for that whole time; `DGP_REPLAY_WINDOW_SECS` sets another window, and `0` disables the check. Idempotent reads (GET/HEAD) are not cached, and a duplicate read is served normally. A duplicate PUT or DELETE that arrives less than one second after the first copy is also served: SigV4 timestamps have 1-second granularity, so an SDK that retries inside the second in which it signed the request (for example after a load balancer lost the response) sends the same signature. The proxy measures that second on its own clock from the first copy, so client clock skew cannot stretch it. A duplicate that arrives later is rejected, and POST requests stay strict. Only a request that succeeds (2xx or 3xx) keeps its signature in the cache, so an SDK retry of a failed request (for example a `503 SlowDown`) is not treated as a replay. Replay rejections do not count toward the auth-failure lockout. The cache is per instance. Full table: [Rate limits and concurrency](rate-limits.md).

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
# validate
storage:
  buckets:
    downloads:
      public_prefixes: ["public/"]
    docs-site:
      public: true        # shorthand for public_prefixes: [""]
```

Semantics for requests without credentials:

- **Allowed**: GET and HEAD on objects under a public prefix; LIST with a `prefix` parameter inside the public prefix. LIST results stay inside the public prefix and do not show the rest of the bucket.
- **Denied**: PUT, DELETE, COPY, and multipart uploads, always.
- **Identity**: anonymous requests run as a built-in `$anonymous` user with scoped read+list permissions (including `s3:prefix` conditions for LIST). Each anonymous request writes a proxy log line with `action=public_read` and `user=$anonymous`. These lines do not go into the admin GUI's audit log.
- **No build metadata**: a HEAD, GET, or `metadata=true` LIST from an anonymous caller does not return `x-amz-meta-dg-tool`, because that value names the proxy version. Authenticated callers, and every caller in open-access mode, still receive it.
- **Lockouts**: the failed-sign-in lockout does not block public reads. A client address that the proxy locked out still gets the objects under a public prefix.
- **Credentials win**: a request carrying valid SigV4 credentials gets full IAM evaluation regardless of public-prefix configuration.
- **Matching**: a trailing `/` is significant: `public/` matches `public/...` but not `publicity/`. The empty prefix `""` makes the entire bucket public (logged as a startup warning). Prefixes containing `..`, null bytes, or `//` are rejected.

The proxy creates one request rule named `public-prefix:<bucket>` for each bucket that has public prefixes, and checks these rules after your own `admission.blocks[]` rules (see [Configuration](configuration.md#admission-chain)). The coordination bucket (`config_sync_bucket`) cannot be public, and a config that tries to make it public is refused.

## What an unauthenticated caller can see

Nothing that the proxy serves without a login names the running build:

- `GET /_/api/whoami` returns `version` and `build_time` only to a live session. Without one, it returns only the auth mode and the list of sign-in providers, which the login page needs.
- `GET /_/health` answers without a version.
- The `version` label of `deltaglider_build_info` on `/_/metrics` is empty, unless `DGP_METRICS_EXPOSE_VERSION=true`. `/_/metrics` itself is public, unless `DGP_METRICS_BEARER_TOKEN` is set: then it needs `Authorization: Bearer <token>` or an admin session.
- The product docs are served by `GET /_/api/docs` to a live session only, and the UI bundle holds no version string.
- Anonymous reads of a public prefix get no `x-amz-meta-dg-tool` (see [Public prefixes](#public-prefixes)).

## Error responses

S3-path errors are returned as standard S3 XML error documents:

| Error code | HTTP status | Cause |
|------------|-------------|-------|
| `AccessDenied` | 403 | No credentials on a non-public path (anonymous), or valid credentials without a matching Allow / with a matching Deny (denied). The response shape is the same in both cases. |
| `SignatureDoesNotMatch` | 403 | Signature verification failed |
| `RequestTimeTooSkewed` | 403 | Request timestamp outside the clock-skew tolerance |
| `InvalidArgument` | 400 | Malformed Authorization header, unparseable date/expiry, or replayed mutating signature |
| `SlowDown` | 503 | The client address is locked out after too many refused signatures. The response carries `Retry-After`, and the message names the wait. |

The proxy answers `403 AccessDenied` to every S3 request to the coordination bucket (`config_sync_bucket`), for every identity, because that bucket holds the synced IAM database, the replication leases, and the reference locks. The bucket is also left out of ListBuckets.

The S3 lockout counts only a request whose signature the proxy checked and refused. A request without credentials, with a malformed `Authorization` header, or with an unknown access key does not count, so anonymous traffic behind a shared load-balancer address cannot lock that address out.

Admin-API errors are JSON, not XML: IAM mutations in declarative mode return `403 { "error": "iam_declarative" }`; admin endpoints without an AdminGui session return `403 { "error": "admin_session_required" }`. A locked-out admin sign-in (password, IAM keys, browser connect) answers `429` with `Retry-After` and `{ "error": "too_many_attempts", "message": "… Try again in N min.", "retry_after_secs": N }`. The single sign-on authorize and callback pages answer `429` with `Retry-After` and an error page that names the wait.

## Related

- [About authentication and access control](../explanation/security-model.md)
- [IAM permissions and conditions](iam-permissions.md)
- [How to create IAM users and groups](../how-to/create-iam-users.md)
