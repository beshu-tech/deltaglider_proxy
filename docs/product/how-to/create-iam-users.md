# How to create IAM users and groups

*Give every client its own scoped credential — here, a CI pipeline (`ci-uploader`) that can write firmware builds and nothing else.*

## 1. Enable bootstrap SigV4

IAM users require authentication to be on. If the proxy still runs open, set the bootstrap credential pair and restart:

```yaml
# validate
access:
  access_key_id: acme-bootstrap-key
  secret_access_key: a-long-random-secret-of-40-plus-characters
```

Or via env vars: `DGP_ACCESS_KEY_ID` + `DGP_SECRET_ACCESS_KEY`. For the guided zero-to-secured walkthrough, see the [Secure your proxy tutorial](../tutorials/secure-your-proxy.md).

## 2. Create the user

Go to **Settings → Access → Users** → **New**.

![IAM users panel](/_/screenshots/iam.jpg)

1. Name: `ci-uploader`.
2. Leave **Access key ID** and **Secret access key** blank to auto-generate them.
3. Set the permissions (next step). A new user starts with `read` and `list` on every resource. To give the user a group, add the user on the group's page (step 5).
4. Save. The secret is shown **once**, in the "User created: save these credentials" dialog — copy it now. If it's lost, rotate the keys (step 4).

If you script it instead, `POST /_/api/admin/users` needs `name`; `access_key_id` and `secret_access_key` are auto-generated when omitted, `enabled` defaults to `true`, and `permissions` defaults to empty. Group membership is a separate call: `POST /_/api/admin/groups/:id/members`. All admin routes require a session cookie from `POST /_/api/admin/login` — see the [admin API reference](../reference/admin-api.md).

## 3. Write a permission

Each permission is an ABAC rule: effect, actions, resources, optional conditions. The shapes you'll write most often:

**Write-only CI** — `ci-uploader` may upload firmware builds and list its bucket, nothing else:

```json
[
  { "effect": "Allow", "actions": ["write"], "resources": ["releases/firmware/*"] },
  { "effect": "Allow", "actions": ["list"],  "resources": ["releases"] }
]
```

**Read-only prefix** — download access to one folder:

```json
{ "effect": "Allow", "actions": ["read", "list"], "resources": ["downloads/public/*"] }
```

**Full access to one bucket** — note the two rules: bucket-level operations match the bare bucket name, object operations match `bucket/*`:

```json
[
  { "effect": "Allow", "actions": ["*"], "resources": ["db-archive"] },
  { "effect": "Allow", "actions": ["*"], "resources": ["db-archive/*"] }
]
```

Deny is absolute: if any Deny rule matches — direct or group-inherited — the request fails regardless of Allows. For the full grammar (action-to-S3-operation mapping, glob rules, `${iam:username}` identity templates) and the preset buttons above the permission editor in the user form, see the [IAM permissions reference](../reference/iam-permissions.md).

## 4. Rotate keys without downtime

`POST /_/api/admin/users/:id/rotate-keys` swaps the credential **atomically** — the old key stops working the moment the call returns. If a brief 403 window on the client is acceptable, rotate and redeploy.

To rotate with zero downtime, overlap two credentials:

1. Clone the user: the **Duplicate user with fresh credentials** button on the user's row, or `POST /_/api/admin/users/:id/clone`. The clone gets fresh keys and a copy of the permissions.
2. Roll the new credentials out to every client.
3. Delete the original user once nothing signs with it (check the audit log at `/_/admin/diagnostics/audit`).

If you only need to suspend a user, untick **Enabled** instead of deleting — the row, groups, and permissions survive.

## 5. Put shared policy in a group

Don't copy the same rules onto ten users. Acme's `Engineering` group carries read access to `releases` and `downloads`:

1. **Settings → Access → Groups** → **New**, name it `Engineering`.
2. Add permissions on the group (same JSON schema as user permissions):

```json
[
  { "effect": "Allow", "actions": ["read", "list"], "resources": ["releases", "releases/*"] },
  { "effect": "Allow", "actions": ["read", "list"], "resources": ["downloads", "downloads/*"] }
]
```

3. Add members: in the group form, tick `dana` under **Members**, then save the group. The user form of `dana` then lists `Engineering` under **Groups & Inherited Access**.

Members get the group's rules in addition to their direct rules; a Deny from either source wins. If you use SSO, mapping rules can add people to `Engineering` automatically on login — see [How to set up OAuth/OIDC single sign-on](set-up-sso.md).

## 6. Switch an AWS client over

No client-side code changes — only the endpoint and the credentials:

```bash
export AWS_ACCESS_KEY_ID=ci-uploader-key
export AWS_SECRET_ACCESS_KEY=ci-uploader-secret
export AWS_ENDPOINT_URL=https://s3.acme.example

aws s3 cp firmware-v2.4.0.bin s3://releases/firmware/firmware-v2.4.0.bin
```

Or as a profile in `~/.aws/credentials` + `~/.aws/config`:

```ini
# ~/.aws/credentials
[acme-proxy]
aws_access_key_id = ci-uploader-key
aws_secret_access_key = ci-uploader-secret

# ~/.aws/config
[profile acme-proxy]
endpoint_url = https://s3.acme.example
region = us-east-1
```

boto3, Terraform, rclone, and the rest of the SigV4 ecosystem work the same way — endpoint override plus proxy credentials. See [client configuration](../reference/authentication.md#client-configuration).

## 7. Presigned URLs

`aws s3 presign` works out of the box; URLs expire after at most 7 days and carry the signing user's permissions — Deny rules apply to presigned requests too.

## Verify

Test the allow **and** the deny — a policy that's never seen a 403 isn't verified:

```bash
# Allowed: write inside the granted prefix
aws --profile acme-proxy s3 cp build.bin s3://releases/firmware/build.bin
# upload: ./build.bin to s3://releases/firmware/build.bin

# Denied: read outside the grant
aws --profile acme-proxy s3 cp s3://db-archive/nightly/dump.sql.gz .
# fatal error: An error occurred (AccessDenied)
```

Every denial lands in the audit log (**Settings → Observability → Audit log**) with user, action, bucket, and path.

## Related

- [IAM permissions reference](../reference/iam-permissions.md) — full grammar, identity templates, canned templates.
- [How to restrict access by IP and prefix](restrict-access-with-conditions.md) — add conditions to these rules.
- [How to set up OAuth/OIDC single sign-on](set-up-sso.md) — auto-provision humans into groups.
- [About authentication and access control](../explanation/security-model.md) — why the layers stack the way they do.
