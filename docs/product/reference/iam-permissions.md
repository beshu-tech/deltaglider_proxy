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
| `Deny` | Blocks access when it matches. It overrides every Allow, whether the Deny comes from the user's direct rules or an inherited group |

A request with no matching Allow is implicitly denied.

### Actions

| Action | S3 operations |
|--------|---------------|
| `read` | GetObject, HeadObject, ListParts |
| `write` | PutObject, CopyObject, CreateMultipartUpload, UploadPart, CompleteMultipartUpload |
| `delete` | DeleteObject, DeleteObjects, AbortMultipartUpload |
| `list` | ListBuckets, ListObjectsV2, ListMultipartUploads, HeadBucket |
| `admin` | CreateBucket, DeleteBucket, and every other bucket-level `PUT` or `DELETE` |
| `*` | All actions |

The proxy derives the action from the method and the path: a `GET` or `HEAD` of a key is `read`, and a `GET` or `HEAD` of a bucket is `list`. ListParts is a `GET` of the upload's key, so it needs `read`.

A user is an **admin** (admin GUI access, config changes) when at least one Allow rule has actions containing `*` or `admin` AND resources containing `*`. A Deny rule of the same shape (actions `*` or `admin`, resources `*`) removes the admin status. Direct rules and group rules count together.

The conditions of that Allow rule must hold for the admin request. An `aws:SourceIp` condition therefore admits the admin GUI and the admin API only from that address range, at sign-in and on every later request. An admin request carries no other condition key, so a condition on any other key never holds for it. A Deny rule of the admin shape removes the admin status whatever its conditions.

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

- The `iam:` prefix is mandatory; a bare `${username}` is **not** substituted. The prefix distinguishes request-time identity substitution from the `${env:NAME}` load-time config expansion. A stale bare `${username}` leaves a literal, unmatchable resource pattern. The rule then matches nothing, so the user is **silently denied**. The save-time config advisories flag this; see [Config advisories](configuration.md#config-advisories).
- Templates are stored raw in the DB/YAML and expanded when the in-memory IAM index is built, after group permissions are merged into each member user.
- Identity values are inserted as they are, because the proxy compares policies against the decoded object key. The user `dana@corp.com` therefore matches the key `home/dana@corp.com/report.pdf`. A value that contains `/`, `*`, `?`, `$`, `{`, `}` or `%` cannot be inserted safely, because it would add a path level or a wildcard. When a user's name or access key contains one of these characters and one of the user's effective permissions uses the matching template, the proxy gives that user no permissions at all and logs a warning.
- Unknown templates are rejected by user/group API validation and by declarative IAM apply.
- User names are unique, so `${iam:username}` gives each user a prefix that no other user shares. The admin API refuses to create a user, clone a user, or rename a user to a name that another user already has, and answers with HTTP 409. When an OAuth or OIDC login creates a new user, the proxy takes the name from the identity provider. Many identity providers let their users change that name, so the proxy adds a suffix when the name is already in use: a second `dana` becomes `dana-2`. A backup import renames such a user with the same suffix rule and lists the rename in its result.
- The isolation holds when a `/` follows the template, as in `home/${iam:username}/*`. Without the `/`, the pattern `home/${iam:username}*` for the user `dana` also matches the prefix `home/dana-2/` of the user `dana-2`.
- A name of `.` or `..` cannot be inserted either, because it is a whole path segment. An OAuth login never creates a user with such a name.
- In declarative mode, a YAML user with its own access key cannot take over a user with the same name that an OAuth login created. The apply fails with an error, and you rename one of the two users. A YAML entry that keeps the access key of the OAuth-created user still manages that user, for example to rotate its secret.
- When the proxy upgrades a database that already holds two users with the same name, it keeps the name for one user and renames each other user with the same suffix rule. A local user keeps the name before a user that an OAuth login created, and otherwise the older user keeps it. It logs a warning for each rename. A renamed user's `${iam:username}` prefix changes with the name.

Example: a per-user home prefix in `db-archive`, shared through the `Engineering` group:

```json
{
  "effect": "Allow",
  "actions": ["read", "write", "list"],
  "resources": ["db-archive/home/${iam:username}/*"]
}
```

For `dana` this expands to `db-archive/home/dana/*`.

## Conditions

Conditions within a single rule are ANDed: all must match for the rule to apply. For a positive operator (`StringLike`, `IpAddress`, …), several values for one key are ORed: one match is enough. A negated operator (`StringNotLike`, `NotIpAddress`, …) takes one value only, because the policy engine would read several values as "misses at least one", which is true for nearly every request. To exclude several values, use the opposite rule: a Deny with `StringLike` and the list, in place of an Allow with `StringNotLike`.

The proxy checks every condition when you save the rule, and refuses one that its policy engine cannot evaluate: a value of the wrong type for its operator (`Null` takes `true` or `false` without quotes; a String or Arn operator takes text; a date operator takes a date as text), an empty value list, or an operator that cannot compare the key (an IP or number operator on `s3:prefix`). A `${iam:…}` variable works only in a String or Arn operator. A rule that is already stored and that the proxy cannot evaluate (synced from an older instance, for example) is contained: an Allow rule grants nothing, and a Deny rule applies without its condition. A declarative YAML file with such a rule still starts the proxy, which logs an error that names the rule and contains it. `config lint` and a config apply refuse the rule.

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
| `aws:SourceIp` | IP address (CIDR) | All requests | Client IP: the address of the TCP connection, or the client that `X-Forwarded-For` names when the connection comes from a network in `DGP_TRUSTED_PROXY_CIDRS` |
| `s3:prefix` | String | LIST requests | The `prefix` query parameter (empty when the request has none) |
| `s3:delimiter` | String | LIST requests | The `delimiter` query parameter |
| `s3:max-keys` | Number | LIST requests | The `max-keys` query parameter |

A client can write any `X-Forwarded-For` value, so the proxy reads that header for `aws:SourceIp` only when `DGP_TRUST_PROXY_HEADERS=true` and the connection comes from a reverse proxy that `DGP_TRUSTED_PROXY_CIDRS` lists. The proxy refuses to start with `DGP_TRUST_PROXY_HEADERS=true` and no `DGP_TRUSTED_PROXY_CIDRS`.

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

- The proxy reads only the prefixes that the user's `Allow` rules with `read` or `list` can reach. A rule that grants only `write` or `delete` shows no key, so its prefix is not read. The proxy derives the prefixes from the resource patterns (the text before the first `*` or `?`) and, for a rule with a `StringLike` or `StringEquals` condition on `s3:prefix`, from the condition values. It then merges these listings in key order. The keys outside these prefixes cost nothing, so the cost of a listing follows what the user can see, not the size of the bucket.
- Each returned key and CommonPrefix is checked against the user's policy; only keys with `read` or `list` permission are returned. A `Deny` on `list` for a key hides that key, even when an `Allow` on the whole bucket admits the listing. A `Deny` on `list` that covers a folder hides the folder name in a delimiter listing too. A `Deny` on `read` alone hides no name, as in AWS: the user can see the key and the folder, but cannot read the object.
- Each page holds up to `max_keys` visible entries. The continuation token is the last visible entry of the page, never a key that the user cannot see.
- A rule that the proxy cannot narrow to prefixes, such as an `Allow` on the whole bucket with `Deny` exceptions, makes the proxy list the requested prefix and skip the hidden keys. That scan reads at most 50 backend pages per request (`advanced.filtered_list_max_engine_pages`). When the scan finds no visible key within that limit, the request fails with `InvalidRequest`; list a narrower prefix instead.
- Users whose policy covers the full requested scope with `read` or `list`, and who have no `Deny` rule that can match a key under the requested prefix, receive the engine page unchanged, with no filtering cost.

## Workflow-bypass prevention

A PUT to a non-existent bucket returns `404 NoSuchBucket` on every backend. This includes the filesystem backend, where the underlying FS could create the parent directory. Bucket creation requires the `admin` action; it cannot occur as a side effect of a write.

## Canned policy templates

The user form (**Access → Users**, `/_/admin/access/users`) shows four preset buttons above the permission editor. A click replaces the permissions in the form, after a confirmation when the form already has permissions. You can edit them before you save. A new user starts with `read` and `list` on every resource.

| Preset | Permissions |
|----------|-------------|
| Read Only | `read`, `list` on every resource |
| Read/Write (no delete) | `read`, `write`, `list` on every resource |
| Read/Write/Delete | `read`, `write`, `delete`, `list` on every resource |
| Full Access (admin) | All actions on every resource |

## Group resolution

- A user's effective permissions are the union of their direct rules and the rules of every group they belong to (for example, `dana`'s direct rules plus the `Engineering` group's rules).
- Group permissions are merged into each member at IAM index build time; identity templates expand after this merge.
- Deny precedence applies across the union: a Deny in any source (direct or inherited) overrides Allows from all sources.
- OAuth group mapping rules add group memberships on each login; memberships are merged, never replaced, so manual assignments persist.
- The proxy creates no group by itself. When the first IAM user is created in `gui` mode, the bootstrap pair becomes the user `legacy-admin` with the direct rule `{ "effect": "Allow", "actions": ["*"], "resources": ["*"] }`.

## Related

- [How to create IAM users and groups](../how-to/create-iam-users.md)
- [How to restrict access with conditions](../how-to/restrict-access-with-conditions.md)
- [Authentication and access](authentication.md)
