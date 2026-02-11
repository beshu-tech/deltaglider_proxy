# DeltaGlider Storage Savings Demo

This demo demonstrates storage savings when using DeltaGlider Proxy with S3-compatible storage. By uploading multiple versions of Elasticsearch JARs, we show how delta compression significantly reduces storage requirements.

> **Note**: The original plan was to integrate with JFrog Artifactory OSS, but we discovered that **S3 binarystore support is a PRO/Enterprise feature only** - it's not available in the OSS version. The demo now uses direct S3 uploads to demonstrate DeltaGlider's capabilities.

## Architecture

```
                                 ┌─────────────────────────┐
                                 │      Demo Script        │
                                 │   (run_simple_demo.sh)  │
                                 └───────────┬─────────────┘
                                             │
                    ┌────────────────────────┴────────────────────────┐
                    │                                                  │
                    ▼                                                  ▼
        ┌─────────────────────┐                          ┌─────────────────────┐
        │   BASELINE TEST     │                          │  DELTAGLIDER TEST   │
        │   (Direct MinIO)    │                          │   (via Proxy)       │
        └─────────┬───────────┘                          └─────────┬───────────┘
                  │                                                │
                  │                                                ▼
                  │                                    ┌─────────────────────┐
                  │                                    │   DeltaGlider       │
                  │                                    │   Proxy (:9012)     │
                  │                                    │   (xdelta3)         │
                  │                                    └─────────┬───────────┘
                  │                                              │
                  └──────────────────┬───────────────────────────┘
                                     │
                                     ▼
                         ┌─────────────────────┐
                         │       MinIO         │
                         │    Object Storage   │
                         │    (:9010/:9011)    │
                         └─────────────────────┘
```

## Prerequisites

Install the following tools:

- **Docker** & **Docker Compose**: https://docs.docker.com/get-docker/
- **MinIO Client (mc)**: `brew install minio/stable/mc` (macOS) or see [MinIO docs](https://min.io/docs/minio/linux/reference/minio-mc.html)
- **curl**: Usually pre-installed
- **AWS CLI** (optional): For advanced S3 testing

## Quick Start

### 1. Start Services

```bash
cd demo/jfrog_artifactory

# Build and start services (MinIO + DeltaGlider)
docker compose up -d --build minio minio-setup deltaglider

# Check status
docker compose ps

# View logs
docker compose logs -f deltaglider
```

### 2. Run the Demo

```bash
# Run the simple demo (direct S3 uploads)
./run_simple_demo.sh
```

This script will:
1. Download multiple versions of Elasticsearch JARs from Maven Central
2. Upload them to MinIO directly (baseline)
3. Upload them through DeltaGlider proxy (delta compressed)
4. Compare storage usage and generate a report

### 3. View Results

```bash
# View the generated report
cat results/report.md

# Or open in a markdown viewer
open results/report.md  # macOS
```

## Services

| Service | Port | Description |
|---------|------|-------------|
| MinIO API | 9010 | S3-compatible object storage |
| MinIO Console | 9011 | MinIO web UI |
| DeltaGlider Proxy | 9012 | Delta compression S3 proxy |

## Access Points

- **MinIO Console**: http://localhost:9011 (minio / minio123)
- **DeltaGlider Proxy**: http://localhost:9012

## How the Demo Works

### Test Artifacts

The demo downloads multiple versions of **Elasticsearch JARs** from Maven Central:
- 7.x family: 7.0.0 through 7.17.x (19 versions)
- 8.x family: 8.0.0 through 8.18.0 (19 versions)

These versions share significant common code, making them ideal for delta compression.

### Baseline Test (Direct MinIO)

1. Elasticsearch JARs uploaded directly to MinIO
2. Artifacts stored as-is, no compression
3. Storage measured in `artifacts` bucket

### DeltaGlider Test (via Proxy)

1. Same JARs uploaded through DeltaGlider Proxy
2. DeltaGlider applies xdelta3 delta compression
3. Only deltas stored for similar files
4. Storage measured in `deltaglider-data` bucket

### Expected Results

For versioned artifacts like Elasticsearch:
- **Typical savings**: 50-80%
- **Best case** (sequential minor versions): Up to 90%

## File Structure

```
demo/jfrog_artifactory/
├── README.md                      # This file
├── docker-compose.yml             # Service definitions
├── Dockerfile.deltaglider         # DeltaGlider build config
├── run_simple_demo.sh             # Main demo script (recommended)
├── jars/                          # Downloaded Elasticsearch JARs (created by demo)
├── results/                       # Generated after running demo
│   ├── baseline_storage.txt
│   ├── deltaglider_storage.txt
│   └── report.md
└── plan.md                        # Original planning document
```

## Manual Testing

### Verify MinIO Storage

```bash
# Configure MinIO client
mc alias set minio http://localhost:9010 minio minio123

# Check baseline bucket
mc du minio/artifacts

# Check DeltaGlider bucket
mc du minio/deltaglider-data

# List objects
mc ls --recursive minio/artifacts
mc ls --recursive minio/deltaglider-data
```

### Verify DeltaGlider Proxy

```bash
# Test S3 API through DeltaGlider
aws --endpoint-url http://localhost:9012 s3 ls s3://default/

# Check DeltaGlider response headers
curl -i http://localhost:9012/default/

# Upload a test file
echo "test" | aws --endpoint-url http://localhost:9012 s3 cp - s3://default/test.txt

# Download and verify
aws --endpoint-url http://localhost:9012 s3 cp s3://default/test.txt -
```

### Using AWS CLI with DeltaGlider

```bash
# Configure credentials
export AWS_ACCESS_KEY_ID=minio
export AWS_SECRET_ACCESS_KEY=minio123
export AWS_DEFAULT_REGION=us-east-1

# List buckets
aws --endpoint-url http://localhost:9012 s3 ls

# Upload files
aws --endpoint-url http://localhost:9012 s3 cp myfile.jar s3://default/myfile.jar

# Download files
aws --endpoint-url http://localhost:9012 s3 cp s3://default/myfile.jar downloaded.jar
```

## Cleanup

```bash
# Stop services and remove volumes
docker compose down -v

# Remove generated results and downloaded JARs
rm -rf results/ jars/
```

## Troubleshooting

### DeltaGlider build fails

```bash
# Build manually from project root
cd ../..
cargo build --release

# Then rebuild Docker image
cd demo/jfrog_artifactory
docker compose build deltaglider
```

### MinIO connection issues

```bash
# Verify MinIO is healthy
mc admin info minio

# Check buckets exist
mc ls minio/
```

### DeltaGlider proxy not responding

```bash
# Check DeltaGlider health
curl http://localhost:9012/health

# Check logs
docker compose logs deltaglider
```

## Notes

- DeltaGlider's `max_delta_ratio=0.5` means files are only stored as deltas if the delta is <50% of the original size
- JAR files are delta-eligible by default (see DeltaGlider file routing configuration)
- First-time JAR downloads may be slow depending on network speed

## Artifactory OSS Limitations

> **Important**: JFrog Artifactory OSS does **not** support S3 binarystore - that's a PRO/Enterprise feature. The binary-store-core JAR in OSS only includes:
> - `FileBinaryProviderImpl` (filesystem)
> - `FileCacheBinaryProviderImpl` (cache-fs)
> - `BlobBinaryProvider` (database)
>
> If you need Artifactory with S3 backend, you'll need Artifactory PRO or Enterprise.

## Learn More

- [DeltaGlider Proxy README](../../README.md)
- [DeltaGlider Operations Guide](../../docs/OPERATIONS.md)
- [AWS S3 API Reference](https://docs.aws.amazon.com/AmazonS3/latest/API/Welcome.html)
- [xdelta3](https://github.com/jmacd/xdelta)
