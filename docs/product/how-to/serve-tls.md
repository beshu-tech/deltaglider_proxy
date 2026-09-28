# How to serve TLS

This guide shows you how to put HTTPS in front of DeltaGlider Proxy. You can terminate TLS at the proxy itself or at a reverse proxy in front of it. The guide also shows how to avoid the reverse-proxy timeout that breaks large uploads.

S3 clients expect HTTPS. The UI (`/_/*`) and the S3 API (`/`) share one listener, so whichever option you pick, route the whole host. Do not write per-path rules.

## Option A: terminate TLS at the proxy

If you run the proxy directly on the edge, enable native TLS with your PEM pair. The proxy binds its listener once at startup, so a TLS change takes effect only after a restart.

In the admin UI:

1. In the sidebar, open **System → System** (`/_/admin/system`).
2. In the **TLS** card, turn on **Enable TLS**.

   ![The TLS card of the System page with TLS off; callout 2 marks the Enable TLS switch.](/_/screenshots/tls-enable.webp)

3. Type the path of your certificate in **Certificate path** and the path of its key in **Private key path**. Both files must be PEM files that the proxy process can read. If you leave both fields empty, the proxy generates a self-signed certificate at startup. That certificate is fine for testing, but clients that verify certificates refuse it.
4. Click **Review & apply** in the bar above the cards.

   ![TLS is on and both certificate paths are filled in; callout 3 marks the Certificate path and Private key path fields, and callout 4 marks Review & apply in the bar above the cards.](/_/screenshots/tls-paths.webp)

5. Check the diff in the dialog, and then click **Apply and Persist**. The dialog says that a restart is required.

   ![The review dialog shows the TLS change and says that a restart is required; the arrow points at Apply and Persist.](/_/screenshots/tls-apply.webp)

6. Restart the proxy. After the restart, the listener speaks HTTPS on the same port.

The proxy writes the change into its config file, and the restart reads it from there. When the config file is mounted read-only, as the Compose example, the Helm chart and the Kubernetes operator mount it, the proxy cannot save the change. The restart then starts without TLS. In that case, set TLS in the YAML of your deployment instead. The admin UI shows a warning banner when the config file is read-only.

### The same change in YAML

The steps above write this configuration into the `advanced` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)):

```yaml
# validate
advanced:
  tls:
    enabled: true
    cert_path: /etc/ssl/certs/proxy.pem
    key_path: /etc/ssl/private/proxy-key.pem
```

The environment variables `DGP_TLS_ENABLED=true`, `DGP_TLS_CERT` and `DGP_TLS_KEY` override these fields. When one of them is set, the admin UI shows the field as read-only with a from env badge. After you edit the file, restart the proxy. An apply through `POST /_/api/admin/config/apply` or `deltaglider_proxy config apply` stores the change, but the listener still needs the restart.

With TLS at the proxy, the admin session cookies carry the `Secure` flag automatically. This is true whether you enable TLS in the YAML file, in the admin UI, or with `DGP_TLS_ENABLED`.

When the proxy faces the internet directly, keep `DGP_TRUST_PROXY_HEADERS=false` (the default). Otherwise clients can spoof `X-Forwarded-For` and bypass rate limiting. `DGP_TRUST_PROXY_HEADERS`, `DGP_TRUSTED_PROXY_CIDRS` and `DGP_SECURE_COOKIES` exist only as environment variables. The YAML file and the admin UI cannot set them.

## Option B: terminate TLS at a reverse proxy

If you terminate TLS at nginx or Caddy on the same host, bind the proxy to `127.0.0.1:9000` and forward over the loopback. To bind it, set **Listen address** in the **HTTP listener** card of **System → System** to `127.0.0.1:9000`, or set `advanced.listen_addr: "127.0.0.1:9000"` in the YAML file, and restart the proxy. The container image sets `DGP_LISTEN_ADDR=0.0.0.0:9000`, so in a container the field is read-only and the variable decides. If Traefik runs in Docker, it reaches the proxy container over the Docker network instead.

**Traefik** (Docker Compose labels):

```yaml
# not-proxy-config: docker-compose service
deltaglider_proxy:
  image: beshultd/deltaglider_proxy:latest
  environment:
    DGP_TRUST_PROXY_HEADERS: "true"
    DGP_TRUSTED_PROXY_CIDRS: "172.16.0.0/12"  # the Docker networks: Traefik connects from its container address
  labels:
    traefik.enable: "true"
    traefik.http.routers.dgp.rule: "Host(`s3.acme.example`)"
    traefik.http.routers.dgp.entrypoints: "websecure"
    traefik.http.routers.dgp.tls.certresolver: "letsencrypt"
    traefik.http.services.dgp.loadbalancer.server.port: "9000"
```

**nginx**:

```nginx
server {
    listen 443 ssl;
    server_name s3.acme.example;
    ssl_certificate /etc/ssl/certs/proxy.pem;
    ssl_certificate_key /etc/ssl/private/proxy-key.pem;
    client_body_timeout 30m;

    location / {
        proxy_pass http://127.0.0.1:9000;
        proxy_read_timeout 30m;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header Host $host;
    }
}
```

**Caddy** (automatic TLS):

```
s3.acme.example {
    reverse_proxy localhost:9000
}
```

Set three env vars on the proxy when a reverse proxy is in front:

| Variable | Value | Why |
|---|---|---|
| `DGP_TRUST_PROXY_HEADERS` | `true` | Accept `X-Forwarded-For` / `X-Real-IP` for rate limiting, the IP binding of admin sessions, and IAM IP conditions. Also accept `X-Forwarded-Host` and `X-Forwarded-Proto` for the same-origin check of admin requests, the OAuth callback address, and the `Secure` cookie flag. Set it to `true` **only** when a reverse proxy is really in front. Otherwise clients can spoof IPs. |
| `DGP_TRUSTED_PROXY_CIDRS` | the reverse proxy's address, for example `127.0.0.1/32` | The proxy reads those headers only on a connection from these networks. It is required with `DGP_TRUST_PROXY_HEADERS=true`: without it, the proxy refuses to start. |
| `DGP_SECURE_COOKIES` | `true` | The listener is plain HTTP behind the reverse proxy, so the proxy cannot see the TLS itself. It sets the `Secure` flag on its own only when a trusted `X-Forwarded-Proto: https` header arrives. Setting this variable to `true` makes the admin session cookies HTTPS-only for every request, and `false` never sets the flag. Unset means automatic. |

## Raise the reverse-proxy read timeout — mandatory for large uploads

If you terminate TLS at a reverse proxy, you must raise its request read-timeout. Most reverse proxies default to 60 seconds. A 16 MB multipart part over a typical home uplink (1 to 5 MB/s, shared between concurrent parts) takes longer than that. The reverse proxy then closes the upstream connection in the middle of the body, and the client sees `502` (Traefik) or `504` (nginx). This affects any object over about 50 MB and every multipart upload.

| Reverse proxy | Default | Setting | Recommended |
|---|---|---|---|
| Traefik 3.x | 60 s | `entryPoints.<name>.transport.respondingTimeouts.readTimeout` | `30m` or `0` (no limit) |
| Caddy 2.x | 0 (no limit) | `read_timeout` in `servers` block | leave at default |
| nginx | 60 s | `client_body_timeout` + `proxy_read_timeout` | `30m` |
| AWS ALB | 60 s `idle_timeout` | target-group attribute | `4000` (max) |
| HAProxy | 60 s `timeout client` | global / frontend | `30m` |

Traefik static config:

```yaml
# not-proxy-config: traefik static config
entryPoints:
  websecure:
    address: ":443"
    transport:
      respondingTimeouts:
        readTimeout: "30m"
        writeTimeout: "30m"
        idleTimeout: "180s"
```

You can also set them as CLI flags on the Traefik container:

```yaml
# not-proxy-config: docker-compose command
command:
  - '--entrypoints.websecure.transport.respondingTimeouts.readTimeout=30m'
  - '--entrypoints.websecure.transport.respondingTimeouts.writeTimeout=30m'
```

The proxy's own request timeout (`DGP_REQUEST_TIMEOUT_SECS`) defaults to 300 s and is a separate timer. Both timeouts must be long enough for large uploads to succeed.

## Verify

```bash
# TLS answers and the health endpoint is reachable
curl -s https://s3.acme.example/_/health

# A signed S3 call works over HTTPS
aws s3 ls --endpoint-url https://s3.acme.example

# Large-upload path survives the timeout (anything > 50 MB)
dd if=/dev/urandom of=/tmp/big.bin bs=1M count=100
aws s3 cp /tmp/big.bin s3://releases/ --endpoint-url https://s3.acme.example
```

If the large upload fails with 502/504 and the proxy log shows a request finishing at exactly 60 000 ms with status 400, the reverse-proxy timeout is still in effect. See [Troubleshooting](troubleshooting.md#502-bad-gateway--504-gateway-timeout-on-large-uploads).

## Related

- [How to take a proxy to production](go-to-production.md): the full checklist
- [Security model](../explanation/security-model.md): where TLS sits among the layers
- [Configuration reference](../reference/configuration.md): TLS and listener fields
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate
