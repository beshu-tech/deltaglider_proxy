# How to deploy with Docker Compose

This guide shows you how to run DeltaGlider Proxy with Docker Compose using a secret-free, commit-safe config. The files are also in the repo under [`examples/docker-compose/`](https://github.com/beshu-tech/deltaglider_proxy/tree/main/examples/docker-compose).

The container reads its configuration when it starts, so you write the configuration as files before the first `docker compose up`, and the admin UI plays no part in this guide. The compose file mounts `deltaglider_proxy.yaml` read-only (`:ro`). A change that you make later in the admin UI therefore works in the running container, but the proxy cannot write it into the file, and the change is lost when the container restarts. Settings that need a restart (the listener, TLS, the blocking thread pool and the sync bucket) never take effect from the UI at all, and a backup restore that includes the config answers `409 config_file_read_only`. The admin UI shows a warning banner about the read-only file. To keep a change, edit `deltaglider_proxy.yaml` (or export the YAML from the UI and commit it) and restart the container. [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md) explains how the file and the UI relate.

The proxy expands `${env:NAME}` and `${env:NAME:-default}` references inside its config file, in-process, at load time. This replaces an external `envsubst` step. You ship a secret-free `deltaglider_proxy.yaml` with placeholders, you supply the real values as environment variables, and the proxy fills them in at startup. When a placeholder is unset and has no default, the proxy stops with an error, so it never starts with a blank secret.

> The `env:` prefix is required. It keeps load-time config placeholders distinct from the request-time IAM permission templates (`${iam:username}`, `${iam:access_key_id}`). A bare `${...}` is left untouched.

> When a placeholder is the whole value of a field, you do not need to quote it. The proxy gives the value the type of its field, so an AES key that has only digits stays a string, and `force_path_style: ${env:S3_PATH_STYLE:-false}` still becomes a boolean. When a placeholder is only part of a value (for example `https://${env:HOST}:9000`), the proxy inserts the value as text.

> The proxy does not expand a placeholder on a comment line, that is, a line whose first character other than a space is `#`. For this reason, a commented-out template line with an unset variable does not stop the proxy.

You need three files:

- `deltaglider_proxy.yaml`: the config. The secret values in it are `${env:...}` placeholders. Commit it.
- `secrets.env`: your real secret values (`KEY=value`). **Never commit it.**
- `docker-compose.yml`: runs the proxy with the config mounted and the secrets supplied through `env_file`.

## 1. Write the config

The block uses `${env:...}` references. The proxy expands them from the environment of the container when it loads the file, so the file holds no secret. A reference can also be the whole value of a field that is not text, such as the boolean `force_path_style`. The proxy gives the value the type of the field, and the JSON Schema of the admin UI editor accepts the reference in that field. To lint the file, load the secrets first, as the Verify section shows.

```yaml
# validate
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
# not-proxy-config: docker-compose
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
      - dgp-data:/data                       # objects, if you use the filesystem backend
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:9000/_/health"]
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 20s
    stop_grace_period: 60s                 # a stop can take up to 40 s
    restart: unless-stopped

volumes:
  dgp-config:
  dgp-data:
```

## 4. Set the admin password

`DGP_BOOTSTRAP_PASSWORD_HASH` is a bcrypt hash, not plaintext. Generate it:

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

If you have the binary locally, validate the config before you ship it. The lint catches a missing `${env:...}` early:

```bash
deltaglider_proxy config lint deltaglider_proxy.yaml
```

## Filesystem backend instead of S3

For a self-contained deployment with no external object store, replace the `storage.backends` block with a filesystem backend pointed at the data volume:

```yaml
# fragment
storage:
  filesystem: /data
```

## Managing users as code

To manage IAM users and groups from the YAML instead of the admin GUI, set `access.iam_mode: declarative`. The YAML then becomes the source of truth, and the proxy reconciles it into the encrypted DB on every apply. See [How to manage IAM as code](manage-iam-as-code.md).

## Update without two processes on one volume

The proxy keeps a lock on its state directory, the folder of `deltaglider_config.db`, because two processes on the same folder would corrupt the job state. When you change the compose file and run `docker compose up -d`, Compose stops the old container and then starts the new one, so the lock is free in time. A deploy tool that starts the new container first and removes the old one a short time later (a rolling update, as Coolify does it on a shared `/data` volume) works as long as the overlap is shorter than `DGP_STATE_LOCK_WAIT_SECS`: the new container waits for the lock, and takes it when the old container stops. The default wait is 60 seconds, and Coolify removes the old container about 5 seconds after the new one starts. When the old container stays longer than the wait, the new container exits with an error that names the state directory and the process that holds it. In that case raise `DGP_STATE_LOCK_WAIT_SECS`, or make the platform stop the old container before it starts the new one. Give the container a stop grace period of at least 60 seconds (`stop_grace_period: 60s`), because a stop takes up to 40 seconds.

## Persistence and backups

The `dgp-config` volume holds the encrypted config DB (`deltaglider_config.db`) with your IAM users, groups, and OAuth providers. Next to it, the volume holds the key file `deltaglider_config.db.key` that the proxy generates on the first start. The database is useless without that key. Back up both (see [How to back up and restore](back-up-and-restore.md)). With an S3 backend, object data lives in the backend, not in these volumes, and the `dgp-data` volume holds objects only when you use the filesystem backend. The proxy writes its temporary files (upload bodies larger than 16 MiB, delta encoding and reconstruction of large objects, multipart parts) to the spool directory, which is `/tmp/dgp-spool` in the container unless you set `DGP_SPOOL_DIR`. Concurrent large uploads each hold about their size there until they are stored, so point `DGP_SPOOL_DIR` at a volume, for example `/data/spool`, if the container file system is small, and set `DGP_SPOOL_MAX_BYTES` (default 16 GiB) below the free space of that volume. The proxy makes the directory private to its user (mode `0700`) and does not start when another user owns it.

The compose file sets `stop_grace_period: 60s`. When the proxy gets the stop signal, it waits up to 10 seconds each for the running requests, a running maintenance job, the event outbox and the work on its blocking threads, so a stop can take up to 40 seconds. Docker kills a container 10 seconds after the stop signal by default, which would cut these steps short.

## Verify

```bash
# Health
curl -fsS http://localhost:9000/_/health

# S3 API answers signed requests
AWS_ACCESS_KEY_ID=$PROXY_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY=$PROXY_SECRET_ACCESS_KEY \
  aws --endpoint-url http://localhost:9000 s3 ls
```

Then open the admin GUI at `http://localhost:9000/_/` and log in with the bootstrap password. The S3 API and the GUI share port 9000. On a fresh proxy, the setup wizard at `/_/admin/setup` walks you through the first backend, bucket and user; remember that its changes, like every UI change, last only until the next restart while the config file is mounted read-only.

Lint the config with the secrets loaded, so that every `${env:...}` placeholder resolves:

```bash
set -a; . ./secrets.env; set +a
deltaglider_proxy config lint deltaglider_proxy.yaml
```

## Related

- [How to take a proxy to production](go-to-production.md): the rest of the production checklist
- [How to serve TLS](serve-tls.md): put HTTPS in front of this stack
- [Configuration reference](../reference/configuration.md): every YAML field and `DGP_*` env var
