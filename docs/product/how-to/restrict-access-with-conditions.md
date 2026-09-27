# How to restrict access by IP and prefix

*Add conditions to IAM rules so a leaked credential is useless outside your network, and users cannot list each other's files.*

A condition limits a rule to some requests, for example only requests from one CIDR or only requests on one prefix. Conditions attach to any Allow or Deny rule, in the same permission editor (**Settings → Access → Users** or **Groups** → edit → permissions).

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

If the proxy sits behind a load balancer or reverse proxy, the direct connection IP is the balancer's. Set `DGP_TRUST_PROXY_HEADERS=true`, set `DGP_TRUSTED_PROXY_CIDRS` to the balancer's network (the proxy reads the header only on a connection from that network), and make the balancer forward the real client IP:

```nginx
proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
```

If the proxy is exposed directly to the internet, leave `DGP_TRUST_PROXY_HEADERS` at its default (`false`). Otherwise clients can spoof their IP with a forged `X-Forwarded-For` header and get past every IP condition.

## 2. Restrict listing by prefix

`aws:SourceIp` works on every request. `s3:prefix` works on LIST requests and matches the `prefix` query parameter. Use it to stop users from browsing outside their own part of a shared bucket.

Give `dana` her own prefix in `db-archive`, and deny any LIST that is not scoped to it:

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

A LIST with `prefix=home/dana/reports/` passes, because the Deny's condition does not match. The proxy denies a LIST with `prefix=home/` or with no prefix at all. To reuse the rule for all users, write `"home/${iam:username}/*"`. The proxy expands it per user at index-build time. Template rules live in the [IAM permissions reference](../reference/iam-permissions.md#permission-templates).

To refuse every LIST whose `prefix` starts with a dot (for example `prefix=.config/`), add a bucket-wide Deny:

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

This rule refuses the request with `AccessDenied`. It does not hide dot-prefixed keys from a LIST with a wider prefix, because the `s3:prefix` condition compares the requested prefix, not the keys. To hide keys from a listing, write a Deny on `list` whose `resources` pattern matches those keys. The proxy then leaves each matching key out of the result.

## 3. Combine conditions

Conditions inside one rule are ANDed: all must hold for the rule to apply. Multiple values for one key are ORed. Separate rules evaluate independently, with Deny taking precedence over every Allow. So "read from anywhere, write only from the office" is two rules, not one rule with two conditions:

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
3. Members' effective permissions are the union of direct and group rules. A Deny in either wins.

## 5. Test with presigned URLs and the CLI

Presigned URLs carry the signing user's permissions, conditions included. You can use them to test a user without configuring a full profile:

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

**Office-only writes.** Reads from anywhere, mutations only from `203.0.113.0/24`. This is the two-rule shape from step 3. Add `{ "effect": "Deny", "actions": ["delete"], "resources": ["*"] }` if deletes should never happen at all.

**Per-team prefix isolation.** One group per team; each group gets Allow `*` on `db-archive/team-a/*` plus Deny `list` on `db-archive/*` with `StringNotLike s3:prefix "team-a/*"`. Teams cannot see each other's key names.

**Minimal CI.** `ci-uploader` gets write+list on `releases/firmware/*` IP-pinned to the build network, Deny `delete` on `*` (artifacts are append-only), and nothing on any other bucket. A compromised CI token can overwrite tomorrow's build, and it can do no other damage.

## Verify

Exercise both sides of every condition:

1. Run an allowed request from a matching IP or prefix. Expect success.
2. Run the same request from a non-matching IP (or a wider LIST prefix). Expect `AccessDenied`.
3. Check **Settings → Observability → Audit log**: the denial appears with the user, action, and path.

## Related

- [IAM permissions reference](../reference/iam-permissions.md): operator tables, condition keys, identity templates, LIST post-filtering.
- [How to create IAM users and groups](create-iam-users.md): the rules these conditions attach to.
- [How to gate requests before authentication](gate-requests-with-admission-rules.md): IP blocking *before* signature verification.
- [About authentication and access control](../explanation/security-model.md): where conditions sit in the evaluation order.
