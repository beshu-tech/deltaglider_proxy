# How to manage IAM as code (GitOps)

*Make YAML the source of truth for users, groups, and OAuth providers. You review changes in pull requests, and the proxy reconciles the YAML into the encrypted DB on every apply.*

Switch to declarative mode when IAM changes should go through code review. Every grant is then in `git log`, and all replicas converge from one file. The proxy locks out admin-API IAM mutations (they return `403 {"error": "iam_declarative"}`), so runtime drift cannot happen. Stay in GUI mode if you manage IAM in the admin UI every day. See [GUI-managed vs declarative IAM](../explanation/security-model.md#gui-managed-vs-declarative-iam) for the trade-off.

## 1. Export the current state

Seed your GitOps file from the live DB. The dedicated export endpoint projects current users, groups, providers, and mapping rules into a ready-to-paste `access:` fragment with `iam_mode: declarative` already set:

```bash
curl -s -c /tmp/dgp.cookies -X POST https://s3.acme.example/_/api/admin/login \
  -H 'Content-Type: application/json' -d '{"password": "<bootstrap-password>"}'

curl -s -b /tmp/dgp.cookies \
  https://s3.acme.example/_/api/admin/config/declarative-iam-export > iam.yaml
```

The export redacts secrets (`secret_access_key: ""`, `client_secret: null`). Put them back before you apply (step 4). The proxy rejects an apply with an empty secret for a user or provider that does **not** already exist, because that identity could never authenticate. For an *existing* entity, the proxy keeps the secret from the DB, so you can re-apply a redacted export as an idempotent no-op. Entities reference each other by name, never by DB id:

```yaml
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
      secret_access_key: ""        # redacted — re-inject before apply
      enabled: true
      groups: ["Engineering"]
      permissions:
        - effect: Allow
          actions: ["write"]
          resources: ["releases/firmware/*"]
```

If you start from an empty file instead, write this shape by hand. The full wire format is in the [declarative IAM reference](../reference/declarative-iam.md).

## 2. Preview the diff

Dry-run before every apply. `POST /_/api/admin/config/section/access/validate` (same body as the PUT) runs the same diff the live apply would, with zero DB writes, and returns a preview line in `warnings`:

```
declarative IAM preview: users(+1/~2/-0) groups(+0/~1/-0) providers(+0/~0/-0) mapping_rules=keep
```

In the admin UI, the ApplyDialog shows the same line under Warnings. You see how many users the apply will create, update, and **delete** before you click Apply:

![Declarative IAM diff preview in the ApplyDialog](/_/screenshots/declarative-iam-diff.jpg)

If you flip from `gui` to `declarative` with no `iam_users`/`iam_groups` in the YAML, the preview warns that the live apply will **refuse**. The empty-YAML gate stops a careless toggle from wiping a populated DB. Add your IAM state to the YAML and validate again.

Validation is all-or-nothing: duplicate access keys, unknown group references, invalid permissions, or a provider whose `provider_type` is not `oidc` fail the whole apply with zero state change.

## 3. Apply

From CI or your workstation, push the full document with the CLI:

```bash
export DGP_BOOTSTRAP_PASSWORD=...   # env var, not a flag — argv leaks via ps
deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example
```

Exit `0` means that the proxy applied **and** persisted the document. The CLI prints the reconcile summary (`declarative IAM reconciled: ...`) to stderr, and every mutation goes into the audit log as `iam_reconcile_*`.

To apply just the access section over the API instead, `PUT /_/api/admin/config/section/access` with the section body (RFC 7396 merge-patch: omitted keys are preserved, `null` deletes).

Because the diff matches entities by name, an edited `access_key_id` on an existing user is an UPDATE that preserves the DB row. OAuth identity bindings therefore survive key rotations.

## 4. Keep secrets out of git

Use `${env:NAME}` references in the committed file:

```yaml
  iam_users:
    - name: ci-uploader
      access_key_id: AKIACIUP00001
      secret_access_key: "${env:CI_UPLOADER_SECRET}"
```

`config apply` expands `${env:NAME}` against the *operator's* environment before sending, so the server receives the values. The server expands the config file from disk against its own environment at startup. A document that arrives over the admin API (a body POSTed to `/config/apply`, or a section PUT from the GUI) is admin input, so the server resolves a reference in it only when the config file loaded at startup already uses that name, or when the name is listed in `DGP_CONFIG_ENV_ALLOWLIST`. Any other reference takes its `:-default`, or the request fails. In a section PUT, only a field whose whole value is one `${env:NAME}` reference is resolved; a reference in the middle of a string stays literal. `config lint` fails with an error on an unset variable with no default. Run it in CI to catch missing secrets before the apply.

The references round-trip. The proxy records which values came from `${env:NAME}` refs. When a GUI change persists the config to disk, and when you download `GET /config/export`, the proxy writes those values as `${env:NAME}` again. It does not write the secret values, and it does not redact the references. This supports a loop: provision a template without secrets, change settings in the GUI as needed, export, and commit the export back into IaC. The proxy keeps secrets that did not come from a ref on disk and redacts them in exports. A ref that expanded into a *non-string* field (a number, a boolean) does not round-trip. The proxy persists it as its literal value.

## 5. Switch back to GUI mode

Set `access.iam_mode: gui` and apply. The flip is a no-op on the DB, so all state stays, and admin-API IAM mutations unlock again. Mode transitions are audit-logged.

## Verify

1. Re-apply the unchanged file: the preview reports `no IAM changes (idempotent apply)` and no `iam_reconcile_*` audit entries appear. This shows that your YAML and the DB agree.
2. Try a GUI mutation (**Settings → Access → Users** → edit): expect `403 iam_declarative`.
3. Sign a request as a YAML-defined user (`ci-uploader`). Expect normal IAM evaluation.

## Related

- [Declarative IAM reference](../reference/declarative-iam.md): wire shape, diff semantics, the empty-YAML gate, adversarial edges.
- [Configuration reference](../reference/configuration.md): `${env:NAME}` expansion and the sectioned YAML format.
- [CLI reference](../reference/cli.md): `config apply` / `config lint` exit codes.
- [How to create IAM users and groups](create-iam-users.md): the GUI-mode equivalent.
