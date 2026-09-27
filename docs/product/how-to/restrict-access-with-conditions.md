# How to restrict access by IP and prefix

*Add conditions to IAM rules so a leaked credential is useless outside your network, and users can't list each other's files.*

Conditions answer "...but only if": only from this CIDR, only on this prefix. They attach to any Allow or Deny rule, in the same permission editor (**Settings → Access → Users** or **Groups** → edit → permissions).

## 1. Restrict by source IP

Pin `ci-uploader`'s write access to Acme's office network, `203.0.113.0/24`. If the credential leaks, requests from anywhere else fail even with a valid signature:

```json
{
  "effect": "Allow",
  "actions": ["write", "list"],
  "resources": ["releases/firmware/*"],
  "conditions": {
    "IpAddress": { "aws:SourceIp": "203.0.113.0/24" }
  }
}
```

Multiple CIDRs are ORed: `"aws:SourceIp": ["203.0.113.0/24", "10.0.0.0/8"]`.

If the proxy sits behind a load balancer or reverse proxy, the direct connection IP is the balancer's — set `DGP_TRUST_PROXY_HEADERS=true`, set `DGP_TRUSTED_PROXY_CIDRS` to the balancer's network (the proxy reads the header only on a connection from that network), and make the balancer forward the real client IP:

```nginx
proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
```

If the proxy is exposed directly to the internet, leave `DGP_TRUST_PROXY_HEADERS` at its default (`false`) — otherwise clients can spoof their IP with a forged `X-Forwarded-For` header and walk straight past every IP condition.

## 2. Restrict listing by prefix

`aws:SourceIp` works on every request; `s3:prefix` works on LIST requests and matches the `prefix` query parameter. Use it to stop users browsing outside their own corner of a shared bucket.

Give `dana` her own prefix in `db-archive`, and deny any LIST that isn't scoped to it:

```json
[
  {
    "effect": "Allow",
    "actions": ["read", "write", "list", "delete"],
    "resources": ["db-archive/home/dana/*"]
  },
  {
    "effect": "Deny",
    "actions": ["list"],
    "resources": ["db-archive/*"],
    "conditions": {
      "StringNotLike": { "s3:prefix": "home/dana/*" }
    }
  }
]
```

A LIST with `prefix=home/dana/reports/` passes (the Deny's condition doesn't match); a LIST with `prefix=home/` or no prefix at all is denied. To make the rule reusable across users, write `"home/${iam:username}/*"` — it expands per user at index-build time. Template rules live in the [IAM permissions reference](../reference/iam-permissions.md#permission-templates).

If you want to hide dot-prefixed keys from everyone, add a bucket-wide Deny:

```json
{
  "effect": "Deny",
  "actions": ["list"],
  "resources": ["*"],
  "conditions": {
    "StringLike": { "s3:prefix": ".*" }
  }
}
```

## 3. Combine conditions

Conditions inside one rule are ANDed — all must hold for the rule to apply. Multiple values for one key are ORed. Separate rules evaluate independently, with Deny taking precedence over every Allow. So "read from anywhere, write only from the office" is two rules, not one rule with two conditions:

```json
[
  { "effect": "Allow", "actions": ["read", "list"], "resources": ["releases/*"] },
  {
    "effect": "Allow",
    "actions": ["write"],
    "resources": ["releases/*"],
    "conditions": { "IpAddress": { "aws:SourceIp": "203.0.113.0/24" } }
  }
]
```

The full operator and condition-key tables (`StringEquals`, `StringNotLike`, `IpAddress`, `NotIpAddress`, ...) are in the [IAM permissions reference](../reference/iam-permissions.md#conditions).

## 4. Share conditioned rules via a group

Conditions work on group permissions exactly as on user permissions. Put the office-network restriction on the `Engineering` group once, and every member inherits it:

1. **Settings → Access → Groups** → edit `Engineering`.
2. Add the conditioned rule from step 1 (adjust resources to the group's scope).
3. Members' effective permissions are the union of direct + group rules; a Deny in either wins.

## 5. Test with presigned URLs and the CLI

Presigned URLs carry the signing user's permissions, conditions included — handy for testing a user without configuring a full profile:

```bash
AWS_ACCESS_KEY_ID=ci-uploader-key AWS_SECRET_ACCESS_KEY=ci-uploader-secret \
aws --endpoint-url https://s3.acme.example \
  s3 presign s3://releases/firmware/firmware-v2.4.0.bin --expires-in 600

curl -o /dev/null -sw "%{http_code}\n" "<presigned-url>"
# 200 from the office CIDR, 403 from anywhere else
```

Direct CLI tests work the same way:

```bash
# From outside 203.0.113.0/24 — expect AccessDenied
aws --endpoint-url https://s3.acme.example s3 cp build.bin s3://releases/firmware/build.bin
```

## Three patterns worth copying

**Office-only writes.** Reads from anywhere, mutations only from `203.0.113.0/24` — the two-rule shape from step 3. Add `{ "effect": "Deny", "actions": ["delete"], "resources": ["*"] }` if deletes should never happen at all.

**Per-team prefix isolation.** One group per team; each group gets Allow `*` on `db-archive/team-a/*` plus Deny `list` on `db-archive/*` with `StringNotLike s3:prefix "team-a/*"`. Teams can't even see each other's key names.

**Minimal CI.** `ci-uploader` gets write+list on `releases/firmware/*` IP-pinned to the build network, Deny `delete` on `*` (artifacts are append-only), and nothing on any other bucket. A compromised CI token can overwrite tomorrow's build — and that's the whole blast radius.

## Verify

Exercise both sides of every condition:

1. Run an allowed request from a matching IP / prefix — expect success.
2. Run the same request from a non-matching IP (or a wider LIST prefix) — expect `AccessDenied`.
3. Check **Settings → Observability → Audit**: the denial appears with the user, action, and path.

## Related

- [IAM permissions reference](../reference/iam-permissions.md) — operator tables, condition keys, identity templates, LIST post-filtering.
- [How to create IAM users and groups](create-iam-users.md) — the rules these conditions attach to.
- [How to gate requests before authentication](gate-requests-with-admission-rules.md) — IP blocking *before* signature verification.
- [About authentication and access control](../explanation/security-model.md) — where conditions sit in the evaluation order.
