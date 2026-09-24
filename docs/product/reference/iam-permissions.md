# IAM permissions and conditions

Reference for the ABAC permission model: rule shape, actions, resource patterns, identity templates, condition operators and keys, LIST scoping, and group resolution.

## Permission shape

Each IAM user carries one or more permission rules, evaluated per request after SigV4 verification:

```json
{
  "effect": "Allow",
  "actions": ["read", "write", "list"],
  "resources": ["releases/firmware/*"],
  "conditions": {
    "IpAddress": { "aws:SourceIp": "203.0.113.0/24" }
  }
}
```

`effect`, `actions`, and `resources` are required; `conditions` is optional.

### Effect

| Value | Meaning |
|-------|---------|
| `Allow` | Grants access when actions, resources, and conditions all match |
| `Deny` | Blocks access when it matches — overrides every Allow, whether the Deny comes from the user's direct rules or an inherited group |

A request with no matching Allow is implicitly denied.

### Actions

| Action | S3 operations |
|--------|---------------|
| `read` | GetObject, HeadObject |
| `write` | PutObject, CopyObject, CreateMultipartUpload, UploadPart, CompleteMultipartUpload |
| `delete` | DeleteObject, DeleteObjects |
| `list` | ListBuckets, ListObjectsV2, ListMultipartUploads, ListParts |
| `admin` | CreateBucket, DeleteBucket |
| `*` | All actions |

A user is an **admin** (admin GUI access, config changes) when at least one Allow rule has actions containing `*` or `admin` AND resources containing `*`.

### Resources

Glob patterns matched against `bucket/key`:

| Pattern | Matches |
|---------|---------|
| `*` | Every bucket and key |
| `releases` | Bucket-level operations only (list, create) |
| `releases/*` | Every object in `releases` |
| `releases/firmware/*` | Objects under the `firmware/` prefix |
| `releases/firmware/fw-2.*` | Glob on the object key |

Bucket-level operations match the bare bucket name (no `/*`); object operations match `bucket/*`. Full access to one bucket therefore requires both rules:

```json
[
  { "effect": "Allow", "actions": ["*"], "resources": ["releases"] },
  { "effect": "Allow", "actions": ["*"], "resources": ["releases/*"] }
]
```

## Permission templates

Resource strings and string condition values accept identity templates:

| Template | Expands to |
|----------|------------|
| `${iam:username}` | The authenticated user's `name` |
| `${iam:access_key_id}` | The authenticated user's access key ID |

Template facts:

- The `iam:` prefix is mandatory; a bare `${username}` is **not** substituted. The prefix distinguishes request-time identity substitution from the `${env:NAME}` load-time config expansion. A stale bare `${username}` leaves a literal, unmatchable resource pattern — so the rule matches nothing and the user is **silently denied**. The save-time config advisories flag this; see [Config advisories](configuration.md#config-advisories).
- Templates are stored raw in the DB/YAML and expanded when the in-memory IAM index is built, after group permissions are merged into each member user.
- Identity values are inserted as they are, because the proxy compares policies against the decoded object key. The user `dana@corp.com` therefore matches the key `home/dana@corp.com/report.pdf`. A value that contains `/`, `*`, `?`, `$`, `{`, `}` or `%` cannot be inserted safely, because it would add a path level or a wildcard. When a user's name or access key contains one of these characters and one of the user's effective permissions uses the matching template, the proxy gives that user no permissions at all and logs a warning.
- Unknown templates are rejected by user/group API validation and by declarative IAM apply.
- User names are unique, so `${iam:username}` gives each user a prefix that no other user shares. The admin API refuses to create a user, clone a user, or rename a user to a name that another user already has, and answers with HTTP 409. When an OAuth or OIDC login creates a new user, the proxy takes the name from the identity provider. Many identity providers let their users change that name, so the proxy adds a suffix when the name is already in use: a second `dana` becomes `dana-2`. A backup import renames such a user with the same suffix rule and lists the rename in its result.
- The isolation holds when a `/` follows the template, as in `home/${iam:username}/*`. Without the `/`, the pattern `home/${iam:username}*` for the user `dana` also matches the prefix `home/dana-2/` of the user `dana-2`.
- A name of `.` or `..` cannot be inserted either, because it is a whole path segment. An OAuth login never creates a user with such a name.
- In declarative mode, a YAML user with its own access key cannot take over a user with the same name that an OAuth login created. The apply fails with an error, and you rename one of the two users. A YAML entry that keeps the access key of the OAuth-created user still manages that user, for example to rotate its secret.
- When the proxy upgrades a database that already holds two users with the same name, it keeps the name for one user and renames each other user with the same suffix rule. A local user keeps the name before a user that an OAuth login created, and otherwise the older user keeps it. It logs a warning for each rename. A renamed user's `${iam:username}` prefix changes with the name.

Example — a per-user home prefix in `db-archive`, shared via the `Engineering` group:

```json
{
  "effect": "Allow",
  "actions": ["read", "write", "list"],
  "resources": ["db-archive/home/${iam:username}/*"]
}
```

For `dana` this expands to `db-archive/home/dana/*`.

## Conditions

Conditions within a single rule are ANDed — all must match for the rule to apply. Multiple values for the same key are ORed.

### Condition operators

| Operator | Description | Example |
|----------|-------------|---------|
| `StringEquals` | Exact string match | `s3:prefix` = `"firmware/"` |
| `StringNotEquals` | Exact string non-match | `s3:prefix` != `"internal/"` |
| `StringLike` | Glob pattern match | `s3:prefix` LIKE `"home/dana/*"` |
| `StringNotLike` | Glob pattern non-match | `s3:prefix` NOT LIKE `".*"` |
| `IpAddress` | CIDR range match | `aws:SourceIp` in `203.0.113.0/24` |
| `NotIpAddress` | CIDR range non-match | `aws:SourceIp` NOT in `203.0.113.0/24` |

### Condition keys

| Key | Type | Available on | Value |
|-----|------|--------------|-------|
| `aws:SourceIp` | IP address (CIDR) | All requests | Client IP — from the direct connection, or from `X-Forwarded-For` / `X-Real-IP` when `DGP_TRUST_PROXY_HEADERS=true` |
| `s3:prefix` | String | LIST requests | The `prefix` query parameter |

With `DGP_TRUST_PROXY_HEADERS=true` on a proxy exposed directly to the internet, clients can spoof `aws:SourceIp` via a forged `X-Forwarded-For` header.

`s3:prefix` string values accept the identity templates above, with the same storage, expansion, and character rules as resource patterns.

### JSON format

```json
{
  "IpAddress": {
    "aws:SourceIp": ["203.0.113.0/24"]
  },
  "StringNotLike": {
    "s3:prefix": "internal/*"
  }
}
```

## ListBucket prefix scoping

When a user with prefix-scoped permissions (for example `{ "resources": ["db-archive/home/dana/*"] }`) issues a LIST with an empty prefix, or a prefix wider than the policy covers, the proxy admits the request and post-filters the result:

- Each returned key and CommonPrefix is checked against the user's policy; only keys with `read` or `list` permission are returned.
- Filtered pages carry the response header `x-amz-meta-dg-list-filtered: true`.
- `is_truncated` and the continuation token reflect the engine-level cursor, not the filtered count; the client's `max_keys` acts as a server-side inspection cap, so a returned page may be smaller than requested.
- Users whose policy covers the full requested scope receive the engine page unchanged, with no filtering cost.

## Workflow-bypass prevention

A PUT to a non-existent bucket returns `404 NoSuchBucket` on every backend — including the filesystem backend, where the underlying FS could create the parent directory. Bucket creation requires the `admin` action; it cannot occur as a side effect of a write.

## Canned policy templates

The admin GUI (**Access → Users → Apply template**) offers four starting-point policies. Generated permissions are editable before saving.

| Template | Permissions |
|----------|-------------|
| Read-only | `read`, `list` on every resource |
| Developer | `read`, `write`, `list` on every resource |
| Admin | All actions on every resource |
| Bucket owner | All actions on one named bucket (selected on apply) |

## Group resolution

- A user's effective permissions are the union of their direct rules and the rules of every group they belong to (for example, `dana`'s direct rules plus the `Engineering` group's rules).
- Group permissions are merged into each member at IAM index build time; identity templates expand after this merge.
- Deny precedence applies across the union: a Deny in any source — direct or inherited — overrides Allows from all sources.
- OAuth group mapping rules add group memberships on each login; memberships are merged, never replaced, so manual assignments persist.
- The built-in Administrators group carries `{ "effect": "Allow", "actions": ["*"], "resources": ["*"] }`.

## Related

- [How to create IAM users and groups](../how-to/create-iam-users.md)
- [How to restrict access with conditions](../how-to/restrict-access-with-conditions.md)
- [Authentication and access](authentication.md)
