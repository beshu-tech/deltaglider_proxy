# How to deploy with Docker Compose

This guide shows you how to run DeltaGlider Proxy with Docker Compose using a secret-free, commit-safe config. The files are also in the repo under [`examples/docker-compose/`](https://github.com/beshu-tech/deltaglider_proxy/tree/main/examples/docker-compose).

The pattern: the proxy expands `${env:NAME}` and `${env:NAME:-default}` references inside its config file, in-process, at load time — an in-program replacement for an external `envsubst` step. You ship a secret-free `deltaglider_proxy.yaml` with placeholders, supply real values as environment variables, and the proxy fills them at startup. An unset placeholder with no default fails the proxy loudly instead of starting with a blank secret.

> The `env:` prefix is required. It keeps load-time config placeholders distinct from the request-time IAM permission templates (`${iam:username}`, `${iam:access_key_id}`). A bare `${...}` is left untouched.

> When a placeholder is the whole value of a field, you do not need to quote it. The proxy gives the value the type of its field, so an AES key that has only digits stays a string, and `force_path_style: ${env:S3_PATH_STYLE:-false}` still becomes a boolean. When a placeholder is only part of a value (for example `https://${env:HOST}:9000`), the proxy inserts the value as text.

You need three files:

- **`deltaglider_proxy.yaml`** — the config; secret values are `${env:...}` placeholders. Commit it.
- **`secrets.env`** — your real secret values (`KEY=value`). **Never commit it.**
- **`docker-compose.yml`** — runs the proxy with the config mounted and the secrets supplied via `env_file`.

## 1. Write the config

```yaml
# deltaglider_proxy.yaml — secret-free, safe to commit
storage:
  backends:
    - name: hetzner-fsn1
      type: s3
      endpoint: ${env:S3_ENDPOINT}
      region: ${env:S3_REGION:-us-east-1}
      force_path_style: ${env:S3_PATH_STYLE:-false}   # true for MinIO / non-AWS
      access_key_id: ${env:S3_ACCESS_KEY_ID}
      secret_access_key: ${env:S3_SECRET_ACCESS_KEY}
  default_backend: hetzner-fsn1

access:
  # A single bootstrap SigV4 pair to get started (add per-user IAM later
  # from the admin GUI, or manage users as code — see below).
  access_key_id: ${env:PROXY_ACCESS_KEY_ID}
  secret_access_key: ${env:PROXY_SECRET_ACCESS_KEY}

advanced:
  listen_addr: 0.0.0.0:9000
  max_object_size: 536870912   # 512 MiB
```

## 2. Write the secrets file

```bash
# secrets.env — your real values; gitignored, never committed.
# This is a docker-compose env_file: KEY=value, taken literally (no shell quoting).

DGP_BOOTSTRAP_PASSWORD_HASH=   # bcrypt hash; see "Set the admin password" below

S3_ENDPOINT=https://s3.eu-central-1.amazonaws.com
S3_REGION=eu-central-1
S3_PATH_STYLE=false
S3_ACCESS_KEY_ID=
S3_SECRET_ACCESS_KEY=

PROXY_ACCESS_KEY_ID=
PROXY_SECRET_ACCESS_KEY=
```

## 3. Write the compose file

```yaml
services:
  # One-time: make the config volume writable by the proxy's non-root user (999)
  # so it can create its encrypted config DB. (A fresh named volume is root-owned.)
  init-perms:
    image: beshultd/deltaglider_proxy:${DGP_IMAGE_TAG:-latest}
    user: "0:0"
    entrypoint: ["/bin/sh", "-c", "chown 999:999 /etc/deltaglider_proxy && chmod 750 /etc/deltaglider_proxy"]
    volumes:
      - dgp-config:/etc/deltaglider_proxy
    restart: "no"

  deltaglider:
    image: beshultd/deltaglider_proxy:${DGP_IMAGE_TAG:-latest}
    depends_on:
      init-perms:
        condition: service_completed_successfully
    env_file:
      - secrets.env
    environment:
      DGP_CONFIG: /etc/deltaglider_proxy/deltaglider_proxy.yaml
    ports:
      - "9000:9000"
    volumes:
      - ./deltaglider_proxy.yaml:/etc/deltaglider_proxy/deltaglider_proxy.yaml:ro
      - dgp-config:/etc/deltaglider_proxy    # encrypted config DB — back this up
      - dgp-data:/data                       # delta/reconstruction scratch
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:9000/_/health"]
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 20s
    restart: unless-stopped

volumes:
  dgp-config:
  dgp-data:
```

## 4. Set the admin password

`DGP_BOOTSTRAP_PASSWORD_HASH` is a **bcrypt hash**, not plaintext. Generate it:

```bash
# inside the image:
docker run --rm beshultd/deltaglider_proxy:latest --set-bootstrap-password
# or with htpasswd:
htpasswd -nbBC 12 "" 'your-password' | tr -d ':\n' | sed 's/^\$2y/\$2b/'
```

## 5. Run it

```bash
cp secrets.env.example secrets.env   # then fill secrets.env
docker compose up -d
docker compose logs -f deltaglider
```

If you have the binary locally, validate the config before shipping — it catches a missing `${env:...}` early:

```bash
deltaglider_proxy config lint deltaglider_proxy.yaml
```

## Filesystem backend instead of S3

For a self-contained deployment with no external object store, replace the `storage.backends` block with a filesystem backend pointed at the data volume:

```yaml
storage:
  filesystem: /data
```

## Managing users as code

To manage IAM users and groups from the YAML instead of the admin GUI, set `access.iam_mode: declarative` — the YAML becomes the source of truth and is reconciled into the encrypted DB on every apply. See [How to manage IAM as code](manage-iam-as-code.md).

## Persistence and backups

The `dgp-config` volume holds the **encrypted config DB** (`deltaglider_config.db`) — your IAM users, groups, and OAuth providers — and, next to it, the key file `deltaglider_config.db.key` that the proxy generates on the first start. The database is useless without that key. Back up both (see [How to back up and restore](back-up-and-restore.md)). The `dgp-data` volume is scratch (caches + delta-reconstruction buffers); object data itself lives in your S3 backend, not in these volumes.

## Verify

```bash
# Health
curl -fsS http://localhost:9000/_/health

# S3 API answers signed requests
AWS_ACCESS_KEY_ID=$PROXY_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY=$PROXY_SECRET_ACCESS_KEY \
  aws --endpoint-url http://localhost:9000 s3 ls
```

Then open the admin GUI at `http://localhost:9000/_/` and log in with the bootstrap password — the S3 API and the GUI share port 9000.

## Related

- [How to take a proxy to production](go-to-production.md) — the rest of the production checklist
- [How to serve TLS](serve-tls.md) — put HTTPS in front of this stack
- [Configuration reference](../reference/configuration.md) — every YAML field and `DGP_*` env var
