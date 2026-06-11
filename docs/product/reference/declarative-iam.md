# Declarative IAM

GitOps-shaped IAM: YAML is the source of truth; the encrypted config DB is reconciled from YAML on every apply.

In **declarative** mode (`access.iam_mode: declarative`), `access.iam_users`, `access.iam_groups`, `access.auth_providers`, and `access.group_mapping_rules` are authoritative. The reconciler diffs YAML against the encrypted config DB on every `/config/apply` (or section-PUT on `access`) and applies the necessary creates, updates, and deletes atomically — in a single SQLite transaction. Admin-API IAM mutation endpoints (POST/PUT/PATCH/DELETE on `/users`, `/groups`, `/ext-auth/*`, `/migrate`, backup import) return `403 { "error": "iam_declarative" }`, so runtime drift cannot occur. Read endpoints stay accessible.

The encrypted DB still holds the state at runtime (the IAM index on the hot path reads from it for O(1) lookups). The reconciler ensures it matches what YAML says. Nothing changes about how SigV4 auth, group membership, or OAuth mapping resolves at request time — they all read the same DB.

`GET /_/api/admin/config/declarative-iam-export` projects the current DB into a self-contained `access:` fragment with `iam_mode: declarative` set and secrets redacted. Re-applying that export with secrets re-injected is an idempotent no-op (the reconciler reports `is_noop` and the audit ring is not bumped).

`POST /_/api/admin/config/section/access/validate` (dry-run, same body shape as PUT) runs the same `diff_iam` the live apply does and returns a preview line in `warnings`:

```
declarative IAM preview: users(+1/~2/-0) groups(+0/~1/-0) providers(+0/~0/-0) mapping_rules=keep
```

An idempotent validate reports `declarative IAM preview: no IAM changes (idempotent apply)`. A flip to declarative with empty IAM yields a warning that the live apply would refuse (see "The empty-YAML gate").

## Wire shape

```yaml
access:
  iam_mode: declarative

  iam_groups:
    - name: admins
      description: "Full access"
      permissions:
        - effect: Allow
          actions: ["*"]
          resources: ["*"]
    - name: readers
      description: "Read-only releases/"
      permissions:
        - effect: Allow
          actions: ["read", "list"]
          resources: ["releases/*"]

  iam_users:
    - name: dana
      access_key_id: AKIADANA00001
      secret_access_key: "replace-with-secret-before-apply"
      enabled: true
      groups: ["admins"]        # by NAME, not DB id
      permissions: []           # direct perms on top of group-inherited

    - name: ci-uploader
      access_key_id: AKIACIUP00001
      secret_access_key: "replace-with-secret-before-apply"
      enabled: true
      groups: ["readers"]
      permissions:
        - effect: Allow
          actions: ["write"]
          resources: ["releases/*"]

  auth_providers:
    - name: google-corp
      provider_type: oidc
      enabled: true
      priority: 10
      display_name: "Google Workspace"
      client_id: "11111.apps.googleusercontent.com"
      client_secret: "replace-with-secret-before-apply"
      issuer_url: "https://accounts.google.com"
      scopes: "openid email profile"

  group_mapping_rules:
    - provider: google-corp     # by NAME (null/absent = all providers)
      priority: 10
      match_type: email_domain
      match_field: email
      match_value: corp.example
      group: admins              # by NAME
```

**Names, not IDs.** Users reference groups by name. Mapping rules reference providers and groups by name. DB row IDs are ephemeral autoincrement values and never appear in YAML. The reconciler resolves names → IDs at apply time.

## Diff semantics

Per entity type (users, groups, providers, mapping rules), by NAME:

| YAML | DB | Action |
|---|---|---|
| present | present + all fields equal | no-op (idempotent path) |
| present | present + any field differs | **UPDATE** — DB row id preserved |
| present | missing | CREATE |
| missing | present | DELETE (cascades via FKs) |

Mapping rules are wipe-and-rebuild (no stable per-row identity beyond the tuple of fields; replacing is identical in observable effect).

**Validation is separate from side effects.** Every YAML-only error (duplicate names, duplicate access keys, unknown group refs, invalid permissions, `$`-prefixed reserved names) surfaces before any DB write. A single error means zero state change.

**Permission templates.** `resources` and string condition values may contain `${iam:username}` and `${iam:access_key_id}` (the `iam:` prefix is required — it distinguishes these request-time identity substitutions from `${env:NAME}` load-time config expansion). The reconciler stores those templates literally in the DB; runtime IAM index rebuild expands them per user after group permissions are merged. Identity values are percent-encoded before substitution, so user names and access keys cannot inject `/` or `*`. Unknown `${...}` variables (including a bare, unprefixed `${username}`) fail validation before any reconcile write.

**ID preservation.** When a user exists in both YAML and DB by name, the row stays (UPDATE), never DELETE+INSERT. `external_identities` reference `user_id`, so rotating an access key via YAML preserves the OAuth linkage.

## External identities

External identities (runtime OAuth byproducts — a user's Google identity binding, for instance) are **not reconciled** from YAML. They are created at runtime by the OAuth callback flow and live only in the DB.

The reconciler's contract:

- `external_identities` are preserved through user UPDATEs (same DB id → same bindings).
- `external_identities` are cascade-deleted when a YAML-authoritative delete removes the user or provider they reference — the user is gone, so the binding is meaningless.

If an OAuth callback is in-flight when a reconcile fires, the callback inserts the external identity into a user row that the reconcile may then delete (if YAML doesn't list that user). The callback flow fails; the next login creates a fresh external user (if auto-provisioning is enabled and matching mapping rules exist).

## Secrets in exports

The canonical exporter redacts every secret on the way out — a YAML pulled from `/config/export` has:

- `iam_users[*].secret_access_key` → `""`
- `auth_providers[*].client_secret` → `null`

Secrets in the YAML file are stored verbatim; there is no env-var substitution for IAM secrets (any placeholder-looking string in `secret_access_key` or `client_secret` becomes the secret). The persist-variant serializer keeps whatever YAML carries on disk across admin-API round-trips.

## The empty-YAML gate

A flip from `gui` to `declarative` with empty `iam_users` AND empty `iam_groups` is refused:

```
Refusing to flip to iam_mode: declarative with empty IAM in YAML —
this would wipe the existing users/groups in the encrypted config DB.
Add access.iam_users / access.iam_groups to the YAML first, or keep
iam_mode: gui to preserve the DB as source of truth.
```

The gate fires only on the `gui→declarative` transition. Declarative-to-declarative with empty YAML is allowed (deliberately clearing all IAM). Gui-to-gui is a no-op.

## Mode transitions

- `gui → declarative` with non-empty YAML: reconcile runs. DB converges to YAML.
- `declarative → declarative`: reconcile always runs.
- `declarative → gui`: no-op on the DB. State preserved; admin-API IAM mutations unlock.
- `gui → gui`: no-op.

## Audit trail

Every mutation the reconciler performs emits an audit ring entry tagged `declarative`:

- `iam_reconcile_user_create` / `_update` / `_delete`
- `iam_reconcile_group_create` / `_update` / `_delete`
- `iam_reconcile_provider_create` / `_update` / `_delete`
- `iam_reconcile_mapping_rules_replaced`

The mode transition itself (`iam_mode` field change) is audited at WARN level by `apply_config_transition`. The `/config/apply` response warnings summarize what the reconciler did; zero activity produces no warning (idempotency signal):

```
declarative IAM reconciled: 5 users (+1/~1/-0), 3 groups (+0/~2/-0),
                             2 providers (+0/~0/-0), 7 mapping rules replaced
```

## Adversarial edges

| Input | Outcome |
|---|---|
| Two YAML users with same `access_key_id` | Validation rejects, zero DB writes |
| User's `groups:` references an unknown group | Validation rejects with the specific missing group name |
| Mapping rule references missing provider/group | Validation rejects with the specific missing name |
| User names starting with `$` (reserved) | Validation rejects (`$anonymous`, `$bootstrap` are synthetic principals) |
| User in YAML has same access_key as a to-be-deleted DB user | Validation rejects (prevents mid-transaction UNIQUE violation) |
| YAML user has permissions with invalid shape | Validation rejects, per-entity error message |
| YAML has zero rules, DB has some | Reconciler clears the mapping_rules table |
| Idempotent re-apply (YAML unchanged) | No DB writes, `diff.is_empty()` |
