# DeltaGlider Proxy — production IaC config

Generated from the live prod configuration (admin config export). **Secret-free
and committable.**

## Files
- `deltaglider_proxy.yaml` — the full prod config, IAM in `declarative` mode.
  Secrets are `${VAR}` placeholders or omitted (injected via env).
- `secrets.env.template` — every secret the deployment needs. Copy to
  `secrets.env`, fill from your secret manager, **never commit the filled copy**
  (`.gitignore` blocks it).

## Deploy
```bash
# 1. Pull secrets into the env (from Vault/SOPS/sealed-secrets/CI store).
cp secrets.env.template secrets.env && <fill secrets.env>
set -a && . ./secrets.env && set +a

# 2. Render the ${VAR} placeholders in the YAML (Group-2 secrets).
envsubst < deltaglider_proxy.yaml > config.yaml   # gitignored

# 3. Validate before shipping (wire this into CI).
deltaglider_proxy config lint config.yaml

# 4. Run. DGP_* vars (Group 1) are read natively; DGP_CONFIG points at the file.
DGP_CONFIG=/etc/deltaglider_proxy/config.yaml deltaglider_proxy
```

Helm/Kustomize users: skip `envsubst` and use your native secret-ref injection
for the Group-2 `${VAR}` values; keep the Group-1 `DGP_*` values as container env.

## Two secret mechanisms (DGP does NOT expand `${VAR}` itself)
- **Group 1 — `DGP_*` env:** read natively by DGP, overlaid on top of the YAML.
  Backend S3 creds, bootstrap password, listen addr, config path.
- **Group 2 — `${VAR}` in YAML:** must be substituted by your tooling BEFORE
  DGP reads the file (per-IAM-user secret keys, OAuth client secret). An
  unexpanded placeholder fails loudly at load.

## Review before first prod apply
- `test` user (full `debug/*`) — likely a leftover; drop unless needed.
- `legacy-admin` (access_key `admin`, wildcard `*`/`*`) — consider folding into
  the `Administrators` group or removing.
- The Google OAuth `client_id` is committed (semi-public); the `client_secret`
  is a `${VAR}`.
- `advanced.listen_addr` was `127.0.0.1:9000` in the export (local dev). Set the
  real prod bind via `${DGP_LISTEN_ADDR}` (default `0.0.0.0:9000`).
