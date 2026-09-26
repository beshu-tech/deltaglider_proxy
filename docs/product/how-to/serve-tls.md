# How to serve TLS

This guide shows you how to put HTTPS in front of DeltaGlider Proxy — either by terminating TLS at the proxy itself or at a reverse proxy in front — and how to avoid the reverse-proxy timeout that breaks large uploads.

S3 clients expect HTTPS. The UI (`/_/*`) and the S3 API (`/`) share one listener, so whichever option you pick, route the whole host — no per-path rules.

## Option A: terminate TLS at the proxy

If you run the proxy directly on the edge, enable native TLS with your PEM pair:

```yaml
advanced:
  tls:
    enabled: true
    cert_path: /etc/ssl/certs/proxy.pem
    key_path: /etc/ssl/private/proxy-key.pem
```

Or via env vars: `DGP_TLS_ENABLED=true`, `DGP_TLS_CERT=...`, `DGP_TLS_KEY=...`. If you omit both paths, the proxy generates a self-signed certificate on startup — fine for testing, not for clients that verify certificates.

With TLS at the proxy, the admin session cookies carry the `Secure` flag automatically. This is true whether you enable TLS in the YAML file or with `DGP_TLS_ENABLED`.

When the proxy faces the internet directly, keep `DGP_TRUST_PROXY_HEADERS=false` (the default). Otherwise clients can spoof `X-Forwarded-For` and bypass rate limiting.

## Option B: terminate TLS at a reverse proxy

If you terminate TLS at Traefik, nginx, or Caddy, bind the proxy to `127.0.0.1:9000` and forward over the loopback.

**Traefik** (Docker Compose labels):

```yaml
deltaglider_proxy:
  image: beshultd/deltaglider_proxy:latest
  environment:
    DGP_TRUST_PROXY_HEADERS: "true"
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

Set two env vars on the proxy when a reverse proxy is in front:

| Variable | Value | Why |
|---|---|---|
| `DGP_TRUST_PROXY_HEADERS` | `true` | Accept `X-Forwarded-For` / `X-Real-IP` for rate limiting and IAM IP conditions. Flip it **only** when a reverse proxy is genuinely in front — otherwise clients can spoof IPs. |
| `DGP_SECURE_COOKIES` | `true` | The listener is plain HTTP behind the reverse proxy, so the proxy cannot see the TLS itself. It sets the `Secure` flag on its own only when a trusted `X-Forwarded-Proto: https` header arrives. Setting this variable to `true` makes the admin session cookies HTTPS-only for every request. |

## Raise the reverse-proxy read timeout — mandatory for large uploads

If you terminate TLS at a reverse proxy, you must raise its request read-timeout. Most default to 60 seconds; a 16 MB multipart part over a typical home uplink (1–5 MB/s, shared between concurrent parts) takes longer than that, so the reverse proxy closes the upstream connection mid-body and the client sees `502` (Traefik) or `504` (nginx). This bites any object over ~50 MB and every multipart upload.

| Reverse proxy | Default | Setting | Recommended |
|---|---|---|---|
| Traefik 3.x | 60 s | `entryPoints.<name>.transport.respondingTimeouts.readTimeout` | `30m` or `0` (no limit) |
| Caddy 2.x | 0 (no limit) | `read_timeout` in `servers` block | leave at default |
| nginx | 60 s | `client_body_timeout` + `proxy_read_timeout` | `30m` |
| AWS ALB | 60 s `idle_timeout` | target-group attribute | `4000` (max) |
| HAProxy | 60 s `timeout client` | global / frontend | `30m` |

Traefik static config:

```yaml
entryPoints:
  websecure:
    address: ":443"
    transport:
      respondingTimeouts:
        readTimeout: "30m"
        writeTimeout: "30m"
        idleTimeout: "180s"
```

…or as CLI flags on the Traefik container:

```yaml
command:
  - '--entrypoints.websecure.transport.respondingTimeouts.readTimeout=30m'
  - '--entrypoints.websecure.transport.respondingTimeouts.writeTimeout=30m'
```

The proxy's own request timeout (`DGP_REQUEST_TIMEOUT_SECS`) defaults to 300 s and is a separate timer — both must be generous for large uploads to succeed.

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

If the large upload fails with 502/504 and the proxy log shows a request finishing at exactly 60 000 ms with status 400, the reverse-proxy timeout is still in effect — see [Troubleshooting](troubleshooting.md#502-bad-gateway--504-gateway-timeout-on-large-uploads).

## Related

- [How to take a proxy to production](go-to-production.md) — the full checklist
- [Security model](../explanation/security-model.md) — where TLS sits among the layers
- [Configuration reference](../reference/configuration.md) — TLS and listener fields
