# SigV4 and IAM users

*Enable per-user access with scoped permissions. Standard S3 clients (`aws-cli`, boto3, Terraform, rclone) continue to work — they just need valid credentials.*

DeltaGlider Proxy supports two SigV4 modes:

- **Bootstrap** — a single access-key / secret pair set in config. Good for a single-tenant service. No per-user audit.
- **IAM** — multiple users, each with their own access key, scoped by ABAC permissions (Allow/Deny, actions, resources, conditions). This is the mode you want anywhere real.

The two modes coexist: bootstrap creds keep working while IAM users exist. The proxy tries the request's access key against both the bootstrap and IAM tables.

Conceptual reference and full policy syntax: [reference/authentication.md](../reference/authentication.md#iam-permissions-abac). This page is the task-oriented walkthrough.

## Step 1: Enable bootstrap SigV4 (baseline)

If you haven't already, set bootstrap credentials in YAML:

```yaml
# validate
access:
  access_key_id: my-proxy-key
  secret_access_key: my-super-secret-key-change-me
```

Or via env (Docker-friendly — no `$` escaping):

```bash
DGP_ACCESS_KEY_ID=my-proxy-key
DGP_SECRET_ACCESS_KEY=my-super-secret-key-change-me
```

Verify:

```bash
# unauthenticated — should fail with AccessDenied
aws --endpoint-url https://dgp.example.com s3 ls

# with creds — should succeed
AWS_ACCESS_KEY_ID=my-proxy-key AWS_SECRET_ACCESS_KEY=my-super-secret-key-change-me \
  aws --endpoint-url https://dgp.example.com s3 ls
```

Every SigV4-signed request is replay-protected (5-second window) and clock-skew-checked (`DGP_CLOCK_SKEW_SECONDS`, default 300s).

## Step 2: Create IAM users

Admin Settings → **Access** → **Users** → **+ Add user**.

| Field | Meaning |
|---|---|
| Name | Display name (audit log + UI) |
| Access key ID | 20-char key — given to the user; can be auto-generated |
| Secret access key | 40+ char secret — shown once on create, hash stored after |
| Permissions | ABAC rules — see step 3 |
| Groups | Inherit permissions from groups (easier than per-user) |
| Enabled | Uncheck to suspend without deleting |

The UI shows the secret once at the "Created" toast. If the user loses it, rotate the key (per-user → right-menu → Rotate access keys).

### Rotate keys (no downtime)

The proxy supports dual-key rotation out of the box. Rotate via UI or:

```bash
curl -b /tmp/admin.cookies -X POST \
  https://dgp.example.com/_/api/admin/users/42/rotate-keys
```

You get the new key; the old one continues to work until you explicitly remove it.

## Step 3: Write IAM permissions

Each permission is an ABAC rule with four fields:

```json
{
  "effect": "Allow",
  "actions": ["read", "list"],
  "resources": ["my-bucket/public/*"],
  "conditions": { "aws:SourceIp": "203.0.113.0/24" }
}
```

**Actions** — high-level verbs matched against the S3 operation's category:

| Action | Matches |
|---|---|
| `read` | GetObject, HeadObject, GetObjectAcl |
| `write` | PutObject, CopyObject, multipart upload |
| `delete` | DeleteObject, DeleteObjects (batch) |
| `list` | ListBucket, ListObjectsV2 |
| `admin` | CreateBucket, DeleteBucket, PutBucketAcl |
| `*` | Everything |

**Resources** — `bucket` or `bucket/prefix/*`:

| Pattern | Matches |
|---|---|
| `*` | Everything (dangerous, use sparingly) |
| `my-bucket` | Bucket-level ops only (list, create) |
| `my-bucket/*` | Every object in the bucket |
| `my-bucket/releases/*` | Objects under the `releases/` prefix |
| `my-bucket/releases/v2.*` | Glob on the object key |

**Effect** — `Allow` (default) or `Deny`. Deny is absolute: if any Deny rule matches, the request fails even if other Allow rules match.

**Conditions** — optional; see [IAM conditions](32-iam-conditions.md).

### Common policy shapes

**Read-only for a specific prefix:**

```json
{
  "effect": "Allow",
  "actions": ["read", "list"],
  "resources": ["builds/ror/libs/*"]
}
```

**CI user — full access to one bucket:**

```json
[
  { "effect": "Allow", "actions": ["*"], "resources": ["ci-artifacts"] },
  { "effect": "Allow", "actions": ["*"], "resources": ["ci-artifacts/*"] }
]
```

Bucket-level ops (`list`, `create`) match on `bucket` (without `/*`); object ops match on `bucket/*`. Both rules are needed for a "can do everything in this bucket" user.

**Admin user — everything:**

```json
{ "effect": "Allow", "actions": ["*"], "resources": ["*"] }
```

This is what the built-in Administrators group carries.

### Canned policy templates

Admin Settings → **Access** → **Users** → **Apply template**:

- **Read-only** — `read, list` on every resource.
- **Developer** — `read, write, list` on every resource.
- **Admin** — full access.
- **Bucket owner** — full access to one named bucket (pick the bucket on apply).

Templates are just starting points; you can edit the generated permissions before saving.

## Step 4: Use groups for shared policies

Rather than giving every developer an identical 5-permission ABAC policy, create a group and add users to it:

1. Access → **Groups** → **+ Add group**.
2. Set permissions on the group (same schema as user permissions).
3. Access → **Users** → edit user → **Groups** → tick the group.

The user inherits the group's permissions *in addition to* their direct rules. Deny in any permission (direct or inherited) still wins.

## Step 5: Switch existing AWS clients

No client-side changes needed — just the endpoint and credentials:

```bash
aws --endpoint-url https://dgp.example.com --region us-east-1 s3 ls
# ~/.aws/credentials: [default] aws_access_key_id=...  aws_secret_access_key=...
```

Python (boto3):

```python
import boto3
s3 = boto3.client(
    "s3",
    endpoint_url="https://dgp.example.com",
    aws_access_key_id="my-key",
    aws_secret_access_key="my-secret",
    region_name="us-east-1",
)
s3.list_buckets()
```

Terraform, rclone, Cyberduck, Transmit, and the rest of the ecosystem all work the same way — they're vanilla SigV4 clients.

## Step 6: Presigned URLs

Presigned URLs work out of the box. Max expiry is 7 days (604,800 s), matching the AWS S3 limit.

```bash
aws --endpoint-url https://dgp.example.com s3 presign s3://my-bucket/report.pdf --expires-in 3600
```

The presigned URL is signed with the IAM user's key and carries their permissions. Deny rules apply to presigned URLs too.

## Verification

From a client with no credentials:

```bash
aws --endpoint-url https://dgp.example.com s3 ls
# Expected: AccessDenied
```

From an unprivileged IAM user trying a privileged op:

```bash
# List a bucket they only have "read" on — should succeed
aws s3 ls s3://public-reads

# Write to it — should fail with AccessDenied
aws s3 cp /tmp/x.txt s3://public-reads/x.txt
```

Check the audit log (`/_/admin/diagnostics/audit`) — every denial lands here with the user name, action, bucket, and path.

## Related

- [Reference: authentication](../reference/authentication.md) — full conceptual model and error codes.
- [IAM conditions](32-iam-conditions.md) — source-IP restrictions, prefix-scoped lists.
- [Rate limiting](33-rate-limiting.md) — the `/_/api/admin/login` and `/_/api/admin/login-as` endpoints are rate-limited per-IP.
- [OAuth setup](30-oauth-setup.md) — the alternative auth mode (not mutually exclusive with IAM).
