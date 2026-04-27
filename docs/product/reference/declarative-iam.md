# Declarative IAM (`iam_mode: declarative`)

*GitOps-shaped IAM: YAML is the source of truth; the encrypted config DB is reconciled from YAML on every apply.*

In **declarative** mode, `access.iam_users`, `access.iam_groups`, `access.auth_providers`, and `access.group_mapping_rules` in your YAML are authoritative. The reconciler diffs YAML against the encrypted config DB on every `/config/apply` (or section-PUT on `access`) and applies the necessary creates, updates, and deletes atomically — in a single SQLite transaction. Admin-API IAM mutation endpoints (POST/PUT/PATCH/DELETE on `/users`, `/groups`, `/ext-auth/*`, `/migrate`, backup import) return `403 { "error": "iam_declarative" }` so runtime drift can't happen.

The encrypted DB still holds the state at runtime (the IAM index on the hot path reads from it for O(1) lookups). The reconciler just ensures it matches what YAML says. **Nothing** changes about how SigV4 auth, group membership, or OAuth mapping resolves at request time — they all read the same DB.

## When to use it

- You manage IAM through git (every user change is a PR).
- You run multiple replicas and want every instance converged from the same YAML.
- Your compliance needs a diff on the YAML file to see who granted what, not an audit trail inside a database.

## When NOT to use it

- You rely on the admin UI for day-to-day user management. GUI IAM mutations return 403 in declarative mode by design.
- OAuth `external_identities` are your primary IAM surface (those stay DB-only; see "External identities" below).

## Turning it on

The flip from `gui` to `declarative` is guarded. If the incoming YAML has no `iam_users` / `iam_groups`, the apply fails loudly — otherwise a careless toggle would delete every DB user. Two workflows:

### Workflow A: already-populated DB + GitOps

Use `GET /_/api/admin/config/declarative-iam-export` — a dedicated endpoint that projects the current DB into a ready-to-paste `access:` fragment with `iam_mode: declarative` already set:

```bash
curl -b cookies https://dgp.example.com/_/api/admin/config/declarative-iam-export > iam.yaml
```

The response is a self-contained YAML:

```yaml
access:
  iam_mode: declarative
  iam_users:
    - name: alice
      access_key_id: AKIAALICE0001
      secret_access_key: ""        # redacted — materialise before apply
      enabled: true
      groups: [admins]
      permissions: []
  iam_groups:
    - name: admins
      ...
  auth_providers: ...
  group_mapping_rules: ...
```

Paste it into your GitOps YAML, wire secrets via env vars, then apply. **Roundtrip contract**: pasting the output with secrets re-injected back into a live PUT is an idempotent no-op — the reconciler reports `is_noop` and doesn't bump the audit ring (integration-tested as `export_declarative_iam_round_trips_as_noop`).

### Workflow B: fresh IAM state from YAML

1. Author your full IAM state in YAML directly.
2. Set `access.iam_mode: declarative`.
3. Apply. The reconciler creates every user, group, provider, and mapping rule.

### Preview before applying

`POST /_/api/admin/config/section/access/validate` (dry-run, same body shape as PUT) returns the response with a preview line in `warnings` showing what the live apply would do:

```
declarative IAM preview: users(+1/~2/-0) groups(+0/~1/-0) providers(+0/~0/-0) mapping_rules=keep
```

The admin-UI's ApplyDialog surfaces this under Warnings — you see exactly how many users will be created, updated, and deleted before you click Apply. An idempotent validate reports `declarative IAM preview: no IAM changes (idempotent apply)`. The preview runs the same `diff_iam` the live apply does, so it can't lie about what will happen.

A flip to declarative with empty IAM yields a warning saying the live apply would REFUSE (see "The empty-YAML gate" below) — catching this at dry-run time avoids an apply that returns 422.

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
    - name: alice
      access_key_id: AKIAALICE0001
      secret_access_key: "replace-with-secret-before-apply"
      enabled: true
      groups: ["admins"]        # by NAME, not DB id
      permissions: []           # direct perms on top of group-inherited

    - name: bob
      access_key_id: AKIABOB000001
      secret_access_key: "replace-with-secret-before-apply"
      enabled: true
      groups: ["readers"]
      permissions:
        - effect: Allow
          actions: ["write"]
          resources: ["uploads/*"]

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

**Names, not IDs.** Users reference groups by name. Mapping rules reference providers and groups by name. DB row IDs are ephemeral autoincrement values and must never appear in YAML. The reconciler resolves names → IDs at apply time.

## What the diff does

Per entity type (users, groups, providers, mapping rules), by NAME:

| YAML | DB | Action |
|---|---|---|
| present | present + all fields equal | no-op (idempotent path) |
| present | present + any field differs | **UPDATE** — DB row id preserved |
| present | missing | CREATE |
| missing | present | DELETE (cascades via FKs) |

Mapping rules are wipe-and-rebuild (no stable per-row identity beyond the tuple of fields; replacing is identical in observable effect).

**Validation is separate from side effects.** Every YAML-only error (duplicate names, duplicate access keys, unknown group refs, invalid permissions, `$`-prefixed reserved names) surfaces BEFORE any DB write. A single typo means zero state change.

**ID preservation.** When a user exists in both YAML and DB by name, the row stays (UPDATE), never gets DELETE+INSERT. This matters because `external_identities` reference `user_id` — rotating an access key via YAML preserves the OAuth linkage.

## External identities

External identities (runtime OAuth byproducts — a user's Google identity binding, for instance) are **not reconciled** from YAML. They are created at runtime by the OAuth callback flow and live only in the DB.

The reconciler's promise:

- `external_identities` are preserved through user UPDATEs (same DB id → same bindings).
- `external_identities` are cascade-deleted when a YAML-authoritative delete removes the user or provider they reference. This is intentional — the user is gone, so the binding is meaningless.

If an OAuth callback is in-flight when a reconcile fires, the callback inserts the external identity into a user row that the reconcile may then delete (if YAML doesn't list that user). The callback flow fails; the next login creates a fresh external user (if auto-provisioning is enabled and matching mapping rules exist).

## Secrets

The canonical exporter redacts every secret on the way out — so a YAML pulled from `/config/export` has:

- `iam_users[*].secret_access_key` → `""`
- `auth_providers[*].client_secret` → `null`

**Today**: secrets in the YAML file must be **plaintext** (or you need to keep the file out of git and materialise it from a secret manager into the container's filesystem at deploy time). An env-var / `${env:...}` substitution syntax is on the roadmap but not implemented yet — any placeholder-looking string in `secret_access_key` or `client_secret` is stored verbatim as the secret.

**Persistence**: the persist-variant serializer keeps whatever YAML carries on disk across admin-API round-trips (admin clicks that persist the file don't strip the secrets the operator put in).

**Infra hygiene**: if you must put plaintext secrets in the YAML, restrict filesystem permissions (`chmod 0600`, owned by the proxy user), keep the file out of git, and treat the container image it lands in as sensitive.

## The empty-YAML gate

A flip from `gui` to `declarative` with empty `iam_users` AND empty `iam_groups` is refused:

```
Refusing to flip to iam_mode: declarative with empty IAM in YAML —
this would wipe the existing users/groups in the encrypted config DB.
Add access.iam_users / access.iam_groups to the YAML first, or keep
iam_mode: gui to preserve the DB as source of truth.
```

The gate ONLY fires on the `gui→declarative` transition. Declarative-to-declarative with empty YAML is allowed (operator deliberately clearing all IAM). Gui-to-gui is a no-op as before.

## Mode transitions

- `gui → declarative` with non-empty YAML: reconcile runs. DB converges to YAML.
- `declarative → declarative`: reconcile always runs. YAML may have new content that wasn't there before.
- `declarative → gui`: no-op on the DB. State preserved; admin-API IAM mutations unlock.
- `gui → gui`: no-op.

## Audit trail

Every mutation the reconciler performs emits an audit ring entry tagged `declarative`:

- `iam_reconcile_user_create` / `_update` / `_delete`
- `iam_reconcile_group_create` / `_update` / `_delete`
- `iam_reconcile_provider_create` / `_update` / `_delete`
- `iam_reconcile_mapping_rules_replaced`

The mode transition itself (`iam_mode` field change) is audited at WARN level by `apply_config_transition` — auditors see it distinctly from routine applies.

## Adversarial edges (and what the reconciler does)

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

## Also note

The `/config/apply` response warnings summarise what the reconciler did:

```
declarative IAM reconciled: 5 users (+1/~1/-0), 3 groups (+0/~2/-0),
                             2 providers (+0/~0/-0), 7 mapping rules replaced
```

Zero activity → no warning (idempotency signal).

The admin-UI `ApplyDialog` renders the warnings below the config diff; operators see the reconcile summary on every live apply.
