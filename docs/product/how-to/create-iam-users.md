# How to create IAM users and groups

*Give every client its own scoped credential. The example is a CI pipeline (`ci-uploader`) that can write firmware builds and nothing else.*

This guide shows you how to turn on authentication, create a user with its own keys and permissions, rotate its keys, and share rules through a group. In the default IAM mode (`access.iam_mode: gui`), users and groups live in the encrypted config database, and you create them in the admin UI. To keep them in YAML instead, see [How to manage IAM as code](manage-iam-as-code.md). For a guided walkthrough from an open proxy to a secured one, see the [Secure your proxy tutorial](../tutorials/secure-your-proxy.md).

## 1. Turn on bootstrap SigV4

IAM users need authentication to be on. If the proxy still runs open, give it a bootstrap credential pair first.

In the admin UI:

1. In the sidebar, open **Access → Credentials & mode** (`/_/admin/access/credentials`), and select **Auto-detect (recommended)** under **S3 authentication mode**.
2. In the **Bootstrap SigV4 credentials** card, type the key in **Access key ID** and the secret in **Secret access key**.

   ![The Credentials & mode page; callout 1 marks Auto-detect (recommended) under S3 authentication mode, and callout 2 marks the Bootstrap SigV4 credentials card, which holds the access key ID of the proxy.](/_/screenshots/secure-credentials-mode.webp)

3. Click **Review & apply** in the bar at the bottom of the page, check the diff, and then click **Apply and Persist**.

When the environment variable `DGP_AUTHENTICATION` is set, the **S3 authentication mode** choice is read-only and shows a note that names the variable. Remove the variable and restart the proxy to change the mode.

### The same change in YAML

The steps above write this configuration into the `access` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)):

```yaml
# validate
access:
  access_key_id: acme-bootstrap-key
  secret_access_key: a-long-random-secret-of-40-plus-characters
```

The environment variables `DGP_ACCESS_KEY_ID` and `DGP_SECRET_ACCESS_KEY` override these two fields. When they are set, the admin UI shows the fields as read-only with a from env badge. After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`.

## 2. Create the user

Users live in the encrypted config database in `gui` mode, so the admin UI (or the admin API) is the only place to create one. There is no YAML for this step.

In the admin UI:

1. In the sidebar, open **Access → Users** (`/_/admin/access/users`).
2. Click **New** above the user list.
3. In **Name**, type `ci-uploader`. Leave **Access Key ID** and **Secret Access Key** empty, so that the proxy generates them.
4. Set the permissions under **Permissions**. A new user starts with one rule that allows **List** and **Read** on every resource (`*`). For `ci-uploader`, keep **Allow**, type `releases/firmware/*` under **WHERE**, and turn on **List**, **Read** and **Write** under **CAN DO**.
5. Click **Create User**.

   ![The Users page with the form of the new ci-uploader user; callout 1 marks Users in the sidebar, callout 2 the New button, callout 3 the Name field, callout 4 the rule that allows List, Read and Write on releases/firmware/*, and callout 5 the Create User button.](/_/screenshots/iam-users-new-form.webp)

The proxy shows the secret **once**, in the dialog **User created: save these credentials**. Copy it now. If you lose it, rotate the keys (section 4).

The **Enabled** switch in the form turns the user off without deleting it. A disabled user keeps its row, its groups and its permissions, but the proxy refuses its requests.

To script the same step, send `POST /_/api/admin/users` with a `name`. The proxy generates `access_key_id` and `secret_access_key` when you omit them, `enabled` defaults to `true`, and `permissions` defaults to empty. Group membership is a separate request: `POST /_/api/admin/groups/:id/members`. All admin routes need a session cookie from `POST /_/api/admin/login`. See the [admin API reference](../reference/admin-api.md).

## 3. Choose the permissions

Each rule in the permission editor has an effect (**Allow** or **Deny**), a list of resources under **WHERE**, and a set of actions under **CAN DO** (**List**, **Read**, **Write**, **Delete** and **Admin**). The buttons above the rules (**Read Only**, **Read/Write (no delete)**, **Read/Write/Delete** and **Full Access (admin)**) replace all rules with a preset. **Add Permission Rule** adds a rule. The admin API stores the same rules in this shape, and these are the shapes that you will use most often.

A CI pipeline that may upload firmware builds and list its bucket, and do nothing else:

```json
[
  { "effect": "Allow", "actions": ["write"], "resources": ["releases/firmware/*"] },
  { "effect": "Allow", "actions": ["list"],  "resources": ["releases"] }
]
```

Download access to one folder:

```json
{ "effect": "Allow", "actions": ["read", "list"], "resources": ["downloads/public/*"] }
```

Full access to one bucket needs two rules, because bucket-level operations match the bare bucket name and object operations match `bucket/*`:

```json
[
  { "effect": "Allow", "actions": ["*"], "resources": ["db-archive"] },
  { "effect": "Allow", "actions": ["*"], "resources": ["db-archive/*"] }
]
```

A Deny rule always wins. If any Deny rule matches, whether it is a direct rule or a group rule, the request fails whatever the Allow rules say. For the full grammar (the mapping of actions to S3 operations, the glob rules, and the `${iam:username}` identity templates), see the [IAM permissions reference](../reference/iam-permissions.md).

## 4. Rotate keys

A key rotation in place swaps the credential at once: the old key stops working as soon as the proxy saves the new one.

In the admin UI:

1. On **Access → Users**, click the `ci-uploader` row. Click **Generate random secret** next to **Secret Access Key**, or type a new secret. To change the key ID as well, type it in **Access Key ID**.
2. Click **Save**, and confirm **Update credentials** in the dialog.

   ![The form of the ci-uploader user after a click on Generate random secret; callout 1 marks the Generate random secret button, and callout 2 marks the Save button.](/_/screenshots/iam-users-rotate.webp)

The dialog **Credentials updated: save them now** shows the new secret once. The admin API does the same with `POST /_/api/admin/users/:id/rotate-keys`.

If a client cannot accept a short period of `403` responses while you roll out the new secret, overlap two credentials instead:

1. Click **Duplicate user with fresh credentials** (the copy icon) on the row of the user. The copy gets fresh keys, the same permissions, and the same groups, and the dialog shows its secret once. The admin API does the same with `POST /_/api/admin/users/:id/clone`.

   ![The list of users; the box marks the Duplicate user with fresh credentials button on the ci-uploader row.](/_/screenshots/iam-users-duplicate.webp)

2. Roll the new credentials out to every client.
3. Delete the original user with **Delete User** once nothing signs with it. The audit log at **Observability → Audit log** (`/_/admin/diagnostics/audit`) shows which user sent each request.

## 5. Put shared rules in a group

Put rules that several users share on a group, and do not copy them onto each user. Acme's `Engineering` group carries read access to `releases`. Groups, like users, live in the config database in `gui` mode.

In the admin UI:

1. In the sidebar, open **Access → Groups** (`/_/admin/access/groups`), and click **New**.
2. In **Name**, type `Engineering`. A **Description** is optional.
3. Under **Permissions**, type `releases/*` under **WHERE**, and turn on **List** and **Read** under **CAN DO**.
4. Under **Members**, tick `dana` and `backup-bot`.
5. Click **Create Group**.

   ![The form of a new Engineering group; callout 1 marks the New button, callout 2 the Name field, callout 3 the rule that allows List and Read on releases/*, callout 4 the members dana and backup-bot, and callout 5 the Create Group button.](/_/screenshots/iam-groups-new-form.webp)

Members get the rules of the group in addition to their direct rules, and a Deny from either source wins. The user form of `dana` then lists `Engineering` under **Groups & Inherited Access**, next to the effective permissions. If you use SSO, mapping rules can add people to `Engineering` automatically when they sign in. See [How to set up OAuth/OIDC single sign-on](set-up-sso.md).

### The same users and groups in YAML

Users and groups are in YAML only in declarative IAM mode, where the file owns them and the IAM pages of the admin UI are read-only ([why IAM is the exception](../explanation/two-ways-to-configure.md#iam-is-the-exception)). In that mode, this `access` section declares the same user and group:

```yaml
# validate
access:
  iam_mode: declarative
  iam_groups:
    - name: Engineering
      description: Firmware and platform engineers
      permissions:
        - effect: Allow
          actions: ["read", "list"]
          resources: ["releases/*"]
  iam_users:
    - name: ci-uploader
      access_key_id: AKIACIUP00001
      secret_access_key: ci-uploader-secret-of-40-plus-characters-00
      permissions:
        - effect: Allow
          actions: ["read", "write", "list"]
          resources: ["releases/firmware/*"]
    - name: dana
      access_key_id: AKIADANA00001
      secret_access_key: dana-secret-of-40-plus-characters-000000000
      groups: ["Engineering"]
      permissions:
        - effect: Allow
          actions: ["read", "list"]
          resources: ["*"]
```

The switch to declarative mode and the apply are in [How to manage IAM as code](manage-iam-as-code.md).

## 6. Switch an AWS client over

The client code does not change. Only the endpoint and the credentials change:

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

boto3, Terraform, rclone, and the rest of the SigV4 ecosystem work the same way, with an endpoint override and proxy credentials. See [client configuration](../reference/authentication.md#client-configuration).

## 7. Presigned URLs

`aws s3 presign` works with no extra setup. Presigned URLs expire after at most 7 days and carry the permissions of the signing user. Deny rules apply to presigned requests too.

## Verify

Test the allow and the deny. You have not verified a policy until you see it return a 403:

```bash
# Allowed: write inside the granted prefix
aws --profile acme-proxy s3 cp build.bin s3://releases/firmware/build.bin
# upload: ./build.bin to s3://releases/firmware/build.bin

# Denied: read outside the grant
aws --profile acme-proxy s3 cp s3://db-archive/nightly/2026-09-27.dump .
# fatal error: An error occurred (AccessDenied)
```

Every denial lands in the audit log (**Observability → Audit log**) with the user, the action, the bucket and the path.

## Related

- [IAM permissions reference](../reference/iam-permissions.md): full grammar, identity templates, canned templates.
- [How to restrict access by IP and prefix](restrict-access-with-conditions.md): add conditions to these rules.
- [How to set up OAuth/OIDC single sign-on](set-up-sso.md): add people to groups automatically.
- [How to manage IAM as code](manage-iam-as-code.md): keep users and groups in YAML.
- [About authentication and access control](../explanation/security-model.md): why the layers stack the way they do.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate.
