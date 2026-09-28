# How to manage IAM as code (GitOps)

*Make YAML the source of truth for users, groups, and OAuth providers. You review changes in pull requests, and the proxy reconciles the YAML into the encrypted DB on every apply.*

This guide shows you how to move IAM from the admin UI into `deltaglider_proxy.yaml`. Switch to declarative mode when IAM changes should go through code review: every grant is then in `git log`, and all replicas converge from one file. In declarative mode the Users, Groups and External authentication pages of the admin UI are read-only, and the admin API refuses IAM changes with `403 {"error": "iam_declarative"}`, so the database cannot drift from the file. Stay in `gui` mode if you manage IAM in the admin UI every day. See [GUI-managed vs declarative IAM](../explanation/security-model.md#gui-managed-vs-declarative-iam) for the trade-off.

The admin UI cannot author the IAM YAML, so this guide uses the YAML file, the CLI and the admin API. The admin UI has two parts in it: the export of the current state (step 1) and the read-only view after the switch (Verify). The **IAM mode** card on **Access → Credentials & mode** also has a switch. A switch alone carries no users or groups, and the proxy refuses a switch to `declarative` with no users or groups in the YAML (step 2), so the card disables the **Declarative** option while the YAML has none. Switch the mode in the same document that lists the users.

## 1. Export the current state

Seed the file from the live database.

In the admin UI:

1. Open the account menu in the top-right corner, and click **Export full IAM (YAML)**. The dialog **Export full IAM as YAML** shows the users, groups, providers and mapping rules as an `access:` section with `iam_mode: declarative` already set. **Download .yaml** saves it, and **Copy to clipboard** copies it.

   ![The account menu is open; the box marks Export full IAM (YAML), which downloads the users, groups, providers and mapping rules with their live secrets.](/_/screenshots/iam-code-export-menu.webp)

This export includes the **live secrets** of users and providers. Handle the file like a password file, and move the secrets into environment references (step 4) before you commit it.

To export without secrets, use the admin API instead:

```bash
curl -s -c /tmp/dgp.cookies -X POST https://s3.acme.example/_/api/admin/login \
  -H 'Content-Type: application/json' -d '{"password": "<bootstrap-password>"}'

curl -s -b /tmp/dgp.cookies \
  https://s3.acme.example/_/api/admin/config/declarative-iam-export > iam.yaml
```

This export redacts secrets (`secret_access_key: ""`, `client_secret: null`). Put them back before you apply. The proxy rejects an apply with an empty secret for a user or provider that does **not** already exist, because that identity could never authenticate. For an *existing* entity, the proxy keeps the secret from the DB, so you can re-apply a redacted export as an idempotent no-op. Entities reference each other by name, never by DB id.

Merge the `access:` section of the export into `deltaglider_proxy.yaml`. The result is a complete configuration document like this one:

```yaml
# validate
access:
  iam_mode: declarative
  iam_groups:
    - name: Engineering
      permissions:
        - effect: Allow
          actions: ["read", "list"]
          resources: ["releases/*", "downloads/*"]
  iam_users:
    - name: ci-uploader
      access_key_id: AKIACIUP00001
      secret_access_key: ci-uploader-secret-of-40-plus-characters-00
      enabled: true
      groups: ["Engineering"]
      permissions:
        - effect: Allow
          actions: ["write"]
          resources: ["releases/firmware/*"]
```

If you start from an empty file instead, write this shape by hand. The full wire format is in the [declarative IAM reference](../reference/declarative-iam.md).

**Import full IAM (YAML)** in the same account menu does the opposite: it reconciles a pasted IAM document into the database. It is for `gui` mode, for example to copy the IAM state to another instance. In declarative mode the menu item is disabled, because the YAML config owns IAM there, and the admin API refuses the import with `403`.

## 2. Preview the diff

Dry-run before every apply. `POST /_/api/admin/config/section/access/validate` (with the same body as the PUT in step 3) runs the same diff as the live apply, with zero DB writes, and returns a preview line in `warnings`:

```
declarative IAM preview: users(+1/~2/-0) groups(+0/~1/-0) providers(+0/~0/-0) mapping_rules=keep
```

The line says how many users the apply will create, update and **delete** before anything changes.

If you switch from `gui` to `declarative` with no `iam_users` or `iam_groups` in the YAML, the preview warns that the live apply will **refuse**. This empty-YAML gate stops a careless switch from wiping a populated database. Add your IAM state to the YAML, and validate again.

Validation is all-or-nothing: duplicate access keys, unknown group references, invalid permissions, or a provider whose `provider_type` is not `oidc` fail the whole apply with zero state change.

## 3. Apply

From CI or your workstation, send the whole document with the CLI ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)):

```bash
export DGP_BOOTSTRAP_PASSWORD=...   # env var, not a flag — argv leaks via ps
deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example
```

Exit code `0` means that the proxy applied **and** persisted the document. The CLI prints the reconcile summary (`declarative IAM reconciled: ...`) to stderr, and every change goes into the audit log as `iam_reconcile_*`. A restart of the proxy with the file also reconciles, but a startup reconcile that would delete users, groups or providers stops the startup instead, because a destructive change must go through an attended apply.

To apply only the access section over the API, send `PUT /_/api/admin/config/section/access` with the section body (RFC 7396 merge-patch: omitted keys are preserved, `null` deletes).

Because the diff matches entities by name, an edited `access_key_id` on an existing user is an UPDATE that preserves the DB row. OAuth identity bindings therefore survive key rotations.

## 4. Keep secrets out of git

Use `${env:NAME}` references in the committed file:

```yaml
# fragment
access:
  iam_users:
    - name: ci-uploader
      access_key_id: AKIACIUP00001
      secret_access_key: "${env:CI_UPLOADER_SECRET}"
```

`config apply` expands `${env:NAME}` against the environment of the *operator* before it sends the document, so the server receives the values. The server expands the config file from disk against its own environment at startup. A document that arrives over the admin API (a body sent to `/config/apply`, or a section PUT from the GUI) is admin input, so the server resolves a reference in it only when the config file loaded at startup already uses that name, or when the name is listed in `DGP_CONFIG_ENV_ALLOWLIST`. Any other reference takes its `:-default`, or the request fails. In a section PUT, only a field whose whole value is one `${env:NAME}` reference is resolved; a reference in the middle of a string stays literal. `config lint` fails with an error on an unset variable with no default. Run it in CI to catch missing secrets before the apply.

The references round-trip. The proxy records which values came from `${env:NAME}` references. When a GUI change persists the config to disk, and when you download `GET /config/export`, the proxy writes those values as `${env:NAME}` again. It does not write the secret values, and it does not redact the references. This supports a loop: provision a template without secrets, change settings in the GUI as needed, export, and commit the export back into IaC. The proxy keeps secrets that did not come from a reference on disk and redacts them in exports. A reference that expanded into a *non-string* field (a number, a boolean) does not round-trip. The proxy persists it as its literal value.

## 5. Switch back to GUI mode

Set `access.iam_mode: gui` and apply. The switch changes nothing in the database, so all state stays, and the IAM pages of the admin UI and the admin-API IAM routes unlock again. Mode transitions are audit-logged.

## Verify

1. Re-apply the unchanged file: the preview reports `no IAM changes (idempotent apply)`, and no `iam_reconcile_*` audit entries appear. This shows that your YAML and the DB agree.
2. Open **Access → Users** (`/_/admin/access/users`). The note at the top says that your YAML config owns the users, the list has no **New** button, and the form of each user starts with **View:** and has no **Save** button.
3. Send an IAM change to the admin API, for example `POST /_/api/admin/users`. Expect `403` with `{"error": "iam_declarative"}`.
4. Sign a request as a YAML-defined user (`ci-uploader`). Expect normal IAM evaluation.

## Related

- [Declarative IAM reference](../reference/declarative-iam.md): wire shape, diff semantics, the empty-YAML gate, adversarial edges.
- [Configuration reference](../reference/configuration.md): `${env:NAME}` expansion and the sectioned YAML format.
- [CLI reference](../reference/cli.md): `config apply` / `config lint` exit codes.
- [How to create IAM users and groups](create-iam-users.md): the GUI-mode equivalent.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): why IAM is the exception to "the UI and the file are one configuration".
