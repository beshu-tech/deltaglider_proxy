# Authentication

DeltaGlider Proxy supports AWS Signature Version 4 (SigV4) authentication. When configured, all S3 API requests must be signed with the proxy's credentials — either via the standard `Authorization` header or via presigned URL query parameters.

## How It Works

The proxy maintains its own credential pair, independent of any backend storage credentials. Clients sign requests using the standard SigV4 algorithm. The proxy verifies the signature, then makes its own separately-authenticated requests to the backend.

- **Filesystem backend**: Proxy credentials are the only auth layer
- **S3 backend**: Proxy credentials authenticate clients to the proxy; separate AWS credentials authenticate the proxy to the upstream S3 service

When no credentials are configured, the proxy operates in open-access mode (no authentication required).

### Two Authentication Paths

The proxy accepts SigV4 credentials from two sources, unified internally into a single verification pipeline:

| Path | Source | Use case |
|------|--------|----------|
| **Header auth** | `Authorization: AWS4-HMAC-SHA256 ...` header + `x-amz-date` + `x-amz-content-sha256` headers | Standard S3 SDK calls (aws-cli, boto3, etc.) |
| **Presigned URL** | `X-Amz-Algorithm`, `X-Amz-Credential`, `X-Amz-Signature`, `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-SignedHeaders` query parameters | Browser downloads, shareable links, `aws s3 presign` |

Both paths extract parameters into the same internal representation, then run identical signature verification logic — reconstruct canonical request, derive signing key, compare HMAC-SHA256 signatures.

## Configuration

### Environment Variables

```bash
export DGP_ACCESS_KEY_ID=myaccesskey
export DGP_SECRET_ACCESS_KEY=mysecretkey
```

Both must be set together. If only one is set, auth remains disabled.

### Config File

```toml
access_key_id = "myaccesskey"
secret_access_key = "mysecretkey"

[backend]
type = "filesystem"
path = "./data"
```

### S3 Backend with Separate Credentials

When using the S3 backend, the proxy needs two sets of credentials:

1. **Proxy credentials** (`DGP_ACCESS_KEY_ID` / `DGP_SECRET_ACCESS_KEY`) — for client-to-proxy auth
2. **Backend credentials** (`DGP_BE_AWS_ACCESS_KEY_ID` / `DGP_BE_AWS_SECRET_ACCESS_KEY`) — for proxy-to-S3 auth

```bash
# Proxy auth (what clients use)
export DGP_ACCESS_KEY_ID=proxy-key
export DGP_SECRET_ACCESS_KEY=proxy-secret

# Backend auth (what the proxy uses to talk to S3/MinIO)
export DGP_BE_AWS_ACCESS_KEY_ID=minioadmin
export DGP_BE_AWS_SECRET_ACCESS_KEY=minioadmin
export DGP_S3_ENDPOINT=http://localhost:9000
```

## Using with S3 Tools

### aws-cli

```bash
export AWS_ACCESS_KEY_ID=myaccesskey
export AWS_SECRET_ACCESS_KEY=mysecretkey
export AWS_DEFAULT_REGION=us-east-1

# Upload
aws --endpoint-url http://localhost:9000 s3 cp file.zip s3://mybucket/file.zip

# Download
aws --endpoint-url http://localhost:9000 s3 cp s3://mybucket/file.zip ./

# List
aws --endpoint-url http://localhost:9000 s3 ls s3://mybucket/

# Generate a presigned download URL (valid 1 hour)
aws --endpoint-url http://localhost:9000 s3 presign s3://mybucket/file.zip --expires-in 3600
```

### boto3 (Python)

```python
import boto3

s3 = boto3.client(
    's3',
    endpoint_url='http://localhost:9000',
    aws_access_key_id='myaccesskey',
    aws_secret_access_key='mysecretkey',
    region_name='us-east-1',
)

s3.upload_file('file.zip', 'mybucket', 'file.zip')
s3.download_file('mybucket', 'file.zip', 'downloaded.zip')

# Generate a presigned download URL
url = s3.generate_presigned_url(
    'get_object',
    Params={'Bucket': 'mybucket', 'Key': 'file.zip'},
    ExpiresIn=3600,
)
```

### curl (unsigned — will be rejected when auth is enabled)

```bash
# This will return 403 AccessDenied when auth is enabled
curl http://localhost:9000/mybucket/file.zip
```

When auth is enabled, all requests must be SigV4-signed. Use aws-cli, boto3, or another S3 SDK that handles signing automatically.

## Why the Proxy Verifies (and Re-signs) Itself

SigV4 signatures are bound to the **Host header** and **URI path** of the request. The proxy's host/port differs from upstream S3, and internal storage paths (deltaspaces, references, deltas) differ from the logical keys clients use.

A client's signature for `GET /mybucket/releases/v2.zip` at `proxy:9000` would be invalid for `GET /releases/v2.zip.delta` at `s3.amazonaws.com`. The proxy cannot forward the client's signature — it must verify it, discard it, and make its own authenticated requests to the backend using the AWS SDK (which signs automatically with the backend credentials).

This is true for both header-auth and presigned URL requests. The presigned URL query parameters are never forwarded upstream.

## Presigned URL Flow

Presigned URLs carry SigV4 credentials in query parameters instead of headers. The proxy verifies them the same way, then makes a fresh SDK-signed request to the backend.

```
  ┌────────┐                  ┌─────────────────┐                ┌──────────┐
  │ Client │                  │ DeltaGlider     │                │ Backend  │
  │        │                  │ Proxy           │                │ S3       │
  └───┬────┘                  └───────┬─────────┘                └────┬─────┘
      │                               │                               │
      │  GET /bucket/key              │                               │
      │  ?X-Amz-Algorithm=...        │                               │
      │  &X-Amz-Credential=PROXY_AK  │                               │
      │  &X-Amz-Signature=abc123     │                               │
      │  &X-Amz-Date=20260215T...    │                               │
      │  &X-Amz-Expires=3600         │                               │
      │ ─────────────────────────────>│                               │
      │                               │                               │
      │                    ┌──────────┴──────────┐                    │
      │                    │ 1. Detect presigned  │                    │
      │                    │    (X-Amz-Algorithm  │                    │
      │                    │    in query params)  │                    │
      │                    │ 2. Parse credentials │                    │
      │                    │ 3. Check expiration  │                    │
      │                    │ 4. Rebuild canonical │                    │
      │                    │    request (without  │                    │
      │                    │    X-Amz-Signature)  │                    │
      │                    │ 5. Derive signing key│                    │
      │                    │    from PROXY secret │                    │
      │                    │ 6. Verify signature  │                    │
      │                    └──────────┬──────────┘                    │
      │                               │                               │
      │                               │  GET /internal/storage/path   │
      │                               │  Authorization: AWS4-HMAC-... │
      │                               │  (signed by AWS SDK with      │
      │                               │   BACKEND credentials)        │
      │                               │ ─────────────────────────────>│
      │                               │                               │
      │                               │  200 OK + stored data         │
      │                               │ <─────────────────────────────│
      │                               │                               │
      │  200 OK + reconstructed file  │                               │
      │  (delta patched if needed)    │                               │
      │ <─────────────────────────────│                               │
      │                               │                               │
```

Key points:
- The client signs against the **proxy's** credentials and host
- The proxy verifies once, then discards all SigV4 artifacts
- The upstream request uses **backend** credentials, possibly targeting different storage paths (deltas, references)
- The two signature contexts are completely independent
- Expiration (`X-Amz-Expires`) is enforced — expired presigned URLs are rejected immediately
- Unparseable `X-Amz-Expires` or `X-Amz-Date` values are rejected (hard failure, not silently ignored)

## Header Auth Flow

Standard header-based auth follows the same verify-then-re-sign pattern:

```
1. Client sends request with Authorization: AWS4-HMAC-SHA256 ... header
2. Proxy extracts access key, credential scope, signed headers, and signature
3. Proxy verifies the access key matches its configured key
4. Proxy reconstructs the canonical request from the HTTP request
5. Proxy derives the signing key from its secret access key
6. Proxy computes the expected signature and compares
7. On match: request proceeds to the handler
   On mismatch: 403 SignatureDoesNotMatch returned
8. Handler makes backend requests via AWS SDK (re-signed with backend credentials)
```

For GET/HEAD/DELETE requests, the payload hash is `UNSIGNED-PAYLOAD` or the SHA-256 of the empty string — verification is header-only and inexpensive.

For PUT requests, the `x-amz-content-sha256` header value is used as the payload hash in signature verification. The actual body integrity is verified downstream by the engine's SHA-256 checksum.

## Error Responses

| Error Code | HTTP Status | Cause |
|---|---|---|
| `AccessDenied` | 403 | Missing credentials, access key mismatch, or expired presigned URL |
| `SignatureDoesNotMatch` | 403 | Signature verification failed |
| `InvalidArgument` | 400 | Malformed Authorization header, unparseable `X-Amz-Expires`, or unparseable `X-Amz-Date` |

All errors are returned as standard S3 XML error responses.

## Security Considerations

- **Use HTTPS in production**: SigV4 authenticates requests but does not encrypt the transport. Use a TLS-terminating reverse proxy (nginx, Caddy, ALB) in front of DeltaGlider Proxy for production deployments.
- **Credential management**: Store credentials securely. Use environment variables or a secrets manager rather than config files in version control.
- **Presigned URL expiration**: Presigned URLs are validated against `X-Amz-Expires`. Generate short-lived URLs when possible.
- **Region**: The proxy accepts any region in the credential scope. Standard S3 tools default to `us-east-1`.
