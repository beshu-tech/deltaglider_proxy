# DeltaGlider Proxy — production IaC config

Generated from the live prod configuration (admin config export). **Secret-free
and committable.**

## Files
- `deltaglider_proxy.yaml` — the full prod config, IAM in `declarative` mode.
  Secrets are `${env:NAME}` placeholders or omitted (injected via env).
- `secrets.env.template` — every secret the deployment needs. Copy to
  `secrets.env`, fill from your secret manager, **never commit the filled copy**
  (`.gitignore` blocks it).
- `backup-zip-to-secrets-env.sh` — converts a prod backup zip into a filled
  `secrets.env` (run it yourself; values stay local). See "Getting secrets".
- `docker-compose.yml` — one-command deploy. The proxy expands `${env:...}`
  itself at load, so the raw YAML is mounted as-is — no render step.

## How secrets reach the config

DGP expands `${env:NAME}` and `${env:NAME:-default}` **in-process** when it loads
the config (the in-program replacement for an external `envsubst`). The `env:`
prefix is mandatory and deliberate — it keeps env placeholders distinct from
DGP's runtime IAM permission templates (`${iam:username}`,
`${iam:access_key_id}`), which are left untouched for the auth layer. An unset
`${env:NAME}` (with no `:-default`) **fails loudly at load**.

So there are two ways a secret reaches the proxy:
- **Native `DGP_*` env** — read directly by DGP, overlaid on top of the YAML
  (e.g. `DGP_BOOTSTRAP_PASSWORD_HASH`, `DGP_CONFIG`).
- **`${env:NAME}` in the YAML** — expanded from the same environment at load
  (named-backend creds, bootstrap SigV4 pair, per-user secret keys, OAuth secret).

Both come from one `secrets.env`. To write a literal dollar-brace in a YAML
comment, double the dollar (`$${...}`) so the expander leaves it alone.

## Getting secrets (`secrets.env`)

```bash
# EASY — from a fresh prod backup zip (current config + all real secrets).
# Pull the zip yourself (your prod admin session); values never leave your box:
curl -fsS -b prod-cookies.txt https://<PROD>/_/api/admin/backup -o prod-backup.zip
./backup-zip-to-secrets-env.sh prod-backup.zip secrets.env

# OR — wire each value from your secret manager (Vault/SOPS/1Password/CI):
cp secrets.env.template secrets.env && <fill secrets.env>
```

> The backup zip is the single source of truth for **current** prod config +
> secrets. The committed `deltaglider_proxy.yaml` is the *structure* (kept in
> sync with prod); the zip fills the *secret values*. If the committed YAML and
> a fresh backup's `iam.json` ever disagree on users/groups, prod drifted —
> regenerate the YAML from the export.

## Deploy with Docker Compose

```bash
docker compose up -d                       # uses the pinned image tag
DGP_IMAGE_TAG=latest docker compose up -d  # or override it
docker compose logs -f dgp                 # admin GUI + S3 API on :9000
```

The raw `deltaglider_proxy.yaml` is mounted read-only; DGP expands its
`${env:...}` placeholders in memory at load (the file on disk stays a secret-free
template). A tiny `init-perms` step makes the config volume writable by the
proxy's non-root `dg` user (999) for the encrypted IAM DB. `secrets.env` is read
via `env_file:` (literal, no shell expansion — safe for the `$`-laden bcrypt
hash). The `dgp-config` volume holds the encrypted IAM DB — **back it up**.

## Deploy (manual / other orchestrators / CI)

```bash
set -a && . ./secrets.env && set +a            # load secrets into the env
deltaglider_proxy config lint deltaglider_proxy.yaml   # validates the EXPANDED
                                                       # config; fails on a missing var
DGP_CONFIG=$PWD/deltaglider_proxy.yaml deltaglider_proxy
```

No `envsubst` / render step — `config lint` and the server both expand
`${env:...}` from the environment. Helm/Kustomize users: inject `secrets.env`'s
values as container env (Secret refs); the proxy does the substitution.

## First-boot note (declarative IAM)

On a **fresh** DB the proxy comes up in bootstrap mode using the bootstrap SigV4
pair (`access.access_key_id`/`secret_access_key`, here `${env:BOOTSTRAP_*}`).
Declarative `iam_users`/groups are reconciled into the DB on a `config apply`
(admin API / GUI), not automatically at cold boot — so after first `up`, push the
config once to populate IAM.

## Review before first prod apply
- `legacy-admin` (access_key `admin`, wildcard `*`/`*`) — consider folding into
  the `Administrators` group or removing.
- The Google OAuth `client_id` is committed (semi-public); the `client_secret`
  is `${env:GOOGLE_OAUTH_CLIENT_SECRET}`.
- `advanced.listen_addr` is `${env:DGP_LISTEN_ADDR}` (the export's local
  `127.0.0.1:9000` was a dev override) — set the real prod bind, e.g.
  `0.0.0.0:9000` behind your TLS-terminating ingress.
