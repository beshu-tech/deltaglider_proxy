# How to restrict access by IP and prefix

*Add conditions to IAM rules so a leaked credential is useless outside your network, and users cannot list each other's files.*

A condition limits a rule to some requests, for example only requests from one network or only listings of one prefix. Conditions attach to any Allow or Deny rule of a user or a group. In the admin UI, the **Conditions** button of a rule opens two fields: **List prefix** (the `StringLike` operator on `s3:prefix`) and **IP restriction** (the `IpAddress` operator on `aws:SourceIp`). The other operators (`StringEquals`, `StringNotLike`, `NotIpAddress`, and the rest) are not in the admin UI. For them, use the admin API, or declare the user in YAML in declarative IAM mode (see [The same change in YAML](#the-same-change-in-yaml)).

## 1. Restrict by source IP

Pin the write access of `backup-bot` to Acme's office network, `203.0.113.0/24`. If the credential leaks, requests from anywhere else fail even with a valid signature.

In the admin UI:

1. In the sidebar, open **Access → Users** (`/_/admin/access/users`), and click the `backup-bot` row.
2. Click **Conditions** on the rule that allows **List** and **Write** on `db-archive/*`.
3. In **IP restriction**, type `203.0.113.0/24`. To allow more than one network, separate them with commas, for example `203.0.113.0/24, 10.0.0.0/8`. The rule then matches a request from any of them.
4. Click **Save**.

   ![The rule of the backup-bot user with its conditions open; callout 1 marks the backup-bot row, callout 2 the Conditions button, callout 3 the IP restriction field with 203.0.113.0/24, and callout 4 the Save button.](/_/screenshots/user-ip-condition.webp)

While a rule has a condition, its **Optional filters** stay open, and the **Conditions** button is disabled. Clear the fields to close them.

If the proxy sits behind a load balancer or reverse proxy, the IP address of the direct connection is the address of the balancer. The admin UI cannot change this setting, so you set it in the environment of the proxy: set `DGP_TRUST_PROXY_HEADERS=true`, set `DGP_TRUSTED_PROXY_CIDRS` to the network of the balancer (the proxy reads the header only on a connection from that network), and make the balancer forward the real client IP:

```nginx
proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
```

If the proxy is exposed directly to the internet, leave `DGP_TRUST_PROXY_HEADERS` at its default (`false`). Otherwise clients can spoof their IP with a forged `X-Forwarded-For` header and get past every IP condition.

## 2. Restrict listing by prefix

`aws:SourceIp` works on every request. `s3:prefix` works on LIST requests and matches the `prefix` query parameter. A rule with **List prefix** therefore applies only to listings whose requested prefix matches the pattern.

As an example, refuse every listing of `dana` whose prefix starts with a dot (for example `prefix=.config/`).

In the admin UI:

1. On **Access → Users**, click the `dana` row, and click **Add Permission Rule** below her rules.
2. Select **Deny** at the top of the new rule. Type `*` under **WHERE**, and turn on **List** under **CAN DO**.
3. Click **Conditions**, and type `.*` in **List prefix**.
4. Click **Save**.

   ![A second rule of the dana user denies List on every bucket when the listed prefix matches .*; callout 1 marks Add Permission Rule, callout 2 the Deny setting, callout 3 the List prefix field, and callout 4 the Save button.](/_/screenshots/iam-conditions-list-prefix.webp)

This rule refuses the request with `AccessDenied`. It does not hide dot-prefixed keys from a listing with a wider prefix, because the `s3:prefix` condition compares the requested prefix, not the keys. To hide keys from a listing, write a Deny on **List** whose resource pattern matches those keys. The proxy then leaves each matching key out of the result.

To keep users out of each other's part of a shared bucket, you often need no condition at all. Give `dana` an Allow on `db-archive/home/dana/*` and nothing wider in `db-archive`: a listing with a wider prefix then succeeds, but the proxy returns only the keys under `home/dana/`, because it checks every returned key against her rules. To refuse such a listing outright instead, add a Deny on `list` with the `StringNotLike` operator (`home/dana/*`). The admin UI cannot write `StringNotLike`, so use the admin API or YAML for that rule. The permission editor then shows the condition read-only under **Other conditions**. The pattern `home/${iam:username}/*` makes one rule work for every user, because the proxy expands `${iam:username}` for each user when it builds the permission index. Template rules are in the [IAM permissions reference](../reference/iam-permissions.md#permission-templates).

## 3. Combine conditions

Conditions inside one rule are ANDed: all of them must hold for the rule to apply. Several values for one key are ORed. Separate rules are evaluated independently, and a Deny wins over every Allow. So "read from anywhere, write only from the office" is two rules, not one rule with two conditions: one Allow rule with **List** and **Read** on `releases/*` and no condition, and one Allow rule with **Write** on `releases/*` and the **IP restriction** `203.0.113.0/24`. In the admin API and in YAML, the same two rules have this shape:

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

The full operator and condition-key tables are in the [IAM permissions reference](../reference/iam-permissions.md#conditions).

## 4. Share conditioned rules through a group

Conditions work on group rules exactly as on user rules. Put the office-network restriction on the `Engineering` group once, and every member inherits it.

In the admin UI:

1. In the sidebar, open **Access → Groups** (`/_/admin/access/groups`), click the `Engineering` row, and click **Conditions** on its rule.
2. In **IP restriction**, type `203.0.113.0/24`.
3. Click **Save**.

   ![The rule of the Engineering group with its conditions open; callout 1 marks the Conditions button, callout 2 the IP restriction field with 203.0.113.0/24, and callout 3 the Save button.](/_/screenshots/iam-conditions-group-ip.webp)

The effective permissions of a member are the union of the direct rules and the group rules, and a Deny from either source wins. The user form shows them under **Groups & Inherited Access**.

## The same change in YAML

In the default IAM mode (`gui`), users and groups live in the encrypted config database, so the admin UI and the admin API are the only ways to change them, and there is no YAML for the steps above. In declarative IAM mode, the file owns users and groups ([why IAM is the exception](../explanation/two-ways-to-configure.md#iam-is-the-exception)), and YAML can also use the operators that the admin UI does not have. This `access` section declares the IP-pinned `backup-bot`, and a `dana` who may list only her own prefix:

```yaml
# validate
access:
  iam_mode: declarative
  iam_users:
    - name: backup-bot
      access_key_id: AKIABACKUPBOT01
      secret_access_key: backup-bot-secret-of-40-plus-characters-000
      permissions:
        - effect: Allow
          actions: ["write", "list"]
          resources: ["db-archive/*"]
          conditions:
            IpAddress:
              aws:SourceIp: "203.0.113.0/24"
    - name: dana
      access_key_id: AKIADANA00001
      secret_access_key: dana-secret-of-40-plus-characters-000000000
      permissions:
        - effect: Allow
          actions: ["read", "write", "list", "delete"]
          resources: ["db-archive/home/dana/*"]
        - effect: Deny
          actions: ["list"]
          resources: ["db-archive/*"]
          conditions:
            StringNotLike:
              s3:prefix: "home/dana/*"
```

A listing with `prefix=home/dana/reports/` passes, because the condition of the Deny does not match. A listing with `prefix=home/` or with no prefix is refused. Apply the file with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`, as in [How to manage IAM as code](manage-iam-as-code.md). In `gui` mode, send the same `permissions` list to `PUT /_/api/admin/users/:id` (see the [admin API reference](../reference/admin-api.md)).

## Three patterns worth copying

**Office-only writes.** Reads from anywhere, changes only from `203.0.113.0/24`. This is the two-rule shape of section 3. Add a Deny on **Delete** for `*` if deletes should never happen at all.

**Per-team prefix isolation.** One group per team. Each group gets an Allow on `db-archive/team-a/*` and a Deny on `list` for `db-archive/*` with `StringNotLike` on `s3:prefix` `"team-a/*"`. Teams cannot see the key names of other teams.

**Minimal CI.** `ci-uploader` gets **Write** and **List** on `releases/firmware/*` with an **IP restriction** to the build network, a Deny on **Delete** for `*` (artifacts are append-only), and nothing on any other bucket. A stolen CI token can overwrite tomorrow's build, and it can do no other damage.

## Verify

Test both sides of every condition. Presigned URLs carry the permissions of the signing user, conditions included, so they work for the test as well. For `backup-bot`, which may write but not read, an upload is the simplest test:

```bash
export AWS_ACCESS_KEY_ID=backup-bot-key AWS_SECRET_ACCESS_KEY=backup-bot-secret

# From a host in 203.0.113.0/24: expect "upload: ..."
aws --endpoint-url https://s3.acme.example s3 cp 2026-09-28.dump s3://db-archive/nightly/2026-09-28.dump

# From any other host: expect AccessDenied
aws --endpoint-url https://s3.acme.example s3 cp 2026-09-28.dump s3://db-archive/nightly/2026-09-28.dump
```

1. Send an allowed request from a matching IP or with a matching prefix. Expect success.
2. Send the same request from a non-matching IP, or a listing with a wider prefix. Expect `AccessDenied`.
3. Check **Observability → Audit log** (`/_/admin/diagnostics/audit`): the denial appears with the user, the action and the path.

## Related

- [IAM permissions reference](../reference/iam-permissions.md): operator tables, condition keys, identity templates, LIST post-filtering.
- [How to create IAM users and groups](create-iam-users.md): the rules these conditions attach to.
- [How to gate requests before authentication](gate-requests-with-admission-rules.md): IP blocking *before* signature verification.
- [About authentication and access control](../explanation/security-model.md): where conditions sit in the evaluation order.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate.
