# Authentication

DeltaGlider Proxy supports AWS Signature Version 4 (SigV4) authentication. When configured, all S3 API requests must be signed with the proxy's credentials.

## How It Works

The proxy maintains its own credential pair, independent of any backend storage credentials. Clients sign requests with the proxy's access key and secret key using the standard SigV4 algorithm. The proxy verifies the signature before processing the request.

- **Filesystem backend**: Proxy credentials are the only auth layer
- **S3 backend**: Proxy credentials authenticate clients to the proxy; separate AWS credentials authenticate the proxy to the upstream S3 service (these may or may not be the same)

When no credentials are configured, the proxy operates in open-access mode (no authentication required).

## Configuration

### Environment Variables

```bash
export DELTAGLIDER_PROXY_ACCESS_KEY_ID=myaccesskey
export DELTAGLIDER_PROXY_SECRET_ACCESS_KEY=mysecretkey
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

1. **Proxy credentials** (`DELTAGLIDER_PROXY_ACCESS_KEY_ID` / `DELTAGLIDER_PROXY_SECRET_ACCESS_KEY`) — for client-to-proxy auth
2. **Backend credentials** (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`) — for proxy-to-S3 auth

```bash
# Proxy auth (what clients use)
export DELTAGLIDER_PROXY_ACCESS_KEY_ID=proxy-key
export DELTAGLIDER_PROXY_SECRET_ACCESS_KEY=proxy-secret

# Backend auth (what the proxy uses to talk to S3/MinIO)
export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export DELTAGLIDER_PROXY_S3_BUCKET=deltaglider-data
export DELTAGLIDER_PROXY_S3_ENDPOINT=http://localhost:9000
```

## Using with S3 Tools

### aws-cli

```bash
export AWS_ACCESS_KEY_ID=myaccesskey
export AWS_SECRET_ACCESS_KEY=mysecretkey
export AWS_DEFAULT_REGION=us-east-1

# Upload
aws --endpoint-url http://localhost:9000 s3 cp file.zip s3://default/file.zip

# Download
aws --endpoint-url http://localhost:9000 s3 cp s3://default/file.zip ./

# List
aws --endpoint-url http://localhost:9000 s3 ls s3://default/
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

s3.upload_file('file.zip', 'default', 'file.zip')
s3.download_file('default', 'file.zip', 'downloaded.zip')
```

### curl (unsigned — will be rejected when auth is enabled)

```bash
# This will return 403 AccessDenied when auth is enabled
curl http://localhost:9000/default/file.zip
```

When auth is enabled, all requests must be SigV4-signed. Use aws-cli, boto3, or another S3 SDK that handles signing automatically.

## Signed Request Flow

```
1. Client sends request with SigV4 Authorization header
2. Proxy extracts and parses the Authorization header
3. Proxy verifies the access key matches its configured key
4. Proxy reconstructs the canonical request from the HTTP request
5. Proxy derives the signing key from its secret access key
6. Proxy computes the expected signature and compares with the provided one
7. On match: request proceeds to the handler
   On mismatch: 403 SignatureDoesNotMatch returned
```

For GET/HEAD/DELETE requests, the payload hash is `UNSIGNED-PAYLOAD` or the SHA-256 of the empty string — verification is header-only and inexpensive.

For PUT requests, the `x-amz-content-sha256` header value is used as the payload hash in signature verification. The actual body integrity is verified downstream by the engine's SHA-256 checksum.

## Why the Proxy Verifies Signatures Itself

SigV4 signatures are bound to the **Host header** and **URI path** of the request. The proxy's host/port differs from upstream S3, and internal storage paths (deltaspaces, references, deltas) differ from the logical keys clients use. A client's signature for `GET /default/releases/v2.zip` at `proxy:9000` would be invalid for `GET /deltaspace_id/v2.zip.delta` at `s3.amazonaws.com`. The proxy must verify signatures itself, then make its own authenticated requests to the backend.

## Error Responses

| Error Code | HTTP Status | Cause |
|---|---|---|
| `AccessDenied` | 403 | No `Authorization` header provided |
| `InvalidAccessKeyId` | 403 | Access key doesn't match configured key |
| `SignatureDoesNotMatch` | 403 | Signature verification failed |

All errors are returned as standard S3 XML error responses.

## Security Considerations

- **Use HTTPS in production**: SigV4 authenticates requests but does not encrypt the transport. Use a TLS-terminating reverse proxy (nginx, Caddy, ALB) in front of DeltaGlider Proxy for production deployments.
- **Credential management**: Store credentials securely. Use environment variables or a secrets manager rather than config files in version control.
- **Presigned URLs**: Not currently supported. All requests must include the `Authorization` header.
- **Region**: The proxy accepts any region in the credential scope. Standard S3 tools default to `us-east-1`.
