#!/bin/bash
#
# run_simple_demo.sh - Simple demo using direct S3 uploads to compare storage
#
# This script demonstrates DeltaGlider storage savings by uploading
# multiple versions of JAR files directly to MinIO (baseline) and
# through DeltaGlider (compressed).
#
# Usage:
#   ./run_simple_demo.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[INFO]${NC} $1"; }
log_success() { echo -e "${GREEN}[SUCCESS]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }

# Elasticsearch versions to download - comprehensive 7.x and 8.x families
VERSIONS=(
    # 7.x family - all minor versions
    "7.0.0"
    "7.1.0"
    "7.2.0"
    "7.3.0"
    "7.4.0"
    "7.5.0"
    "7.6.0"
    "7.7.0"
    "7.8.0"
    "7.9.0"
    "7.10.0"
    "7.11.0"
    "7.12.0"
    "7.13.0"
    "7.14.0"
    "7.15.0"
    "7.16.0"
    "7.17.0"
    "7.17.10"
    "7.17.20"
    "7.17.25"
    # 8.x family - all minor versions
    "8.0.0"
    "8.1.0"
    "8.2.0"
    "8.3.0"
    "8.4.0"
    "8.5.0"
    "8.6.0"
    "8.7.0"
    "8.8.0"
    "8.9.0"
    "8.10.0"
    "8.11.0"
    "8.12.0"
    "8.13.0"
    "8.14.0"
    "8.15.0"
    "8.16.0"
    "8.17.0"
    "8.18.0"
)

MAVEN_BASE="https://repo1.maven.org/maven2/org/elasticsearch/elasticsearch"

# Ensure services are running
ensure_services() {
    log_info "Ensuring Docker services are running..."
    docker compose up -d minio minio-setup deltaglider

    # Wait for services
    sleep 5

    # Check DeltaGlider
    if curl -sf "http://localhost:9012/" >/dev/null 2>&1; then
        log_success "DeltaGlider is ready"
    else
        log_error "DeltaGlider is not responding"
        exit 1
    fi
}

# Configure mc alias
setup_mc() {
    log_info "Configuring MinIO client..."
    mc alias set minio http://localhost:9010 minio minio123 --api S3v4 2>/dev/null
    mc alias set deltaglider http://localhost:9012 minio minio123 --api S3v4 2>/dev/null
    log_success "MinIO client configured"
}

# Download JAR files
download_jars() {
    log_info "Downloading Elasticsearch JARs..."
    mkdir -p "${SCRIPT_DIR}/jars"

    for version in "${VERSIONS[@]}"; do
        local jar_file="${SCRIPT_DIR}/jars/elasticsearch-${version}.jar"
        if [ -f "${jar_file}" ]; then
            log_info "Already have ${version}"
        else
            log_info "Downloading Elasticsearch ${version}..."
            curl -sf -o "${jar_file}" "${MAVEN_BASE}/${version}/elasticsearch-${version}.jar" || {
                log_warn "Failed to download ${version}"
                continue
            }
            log_success "Downloaded ${version}"
        fi
    done
}

# Reset bucket
reset_bucket() {
    local bucket=$1
    log_info "Resetting bucket: ${bucket}..."
    mc rm --recursive --force minio/${bucket} 2>/dev/null || true
    mc mb --ignore-existing minio/${bucket} 2>/dev/null || true
}

# Upload to baseline (direct MinIO)
upload_baseline() {
    log_info "=========================================="
    log_info "Uploading to BASELINE (Direct MinIO)"
    log_info "=========================================="

    reset_bucket "artifacts"

    for version in "${VERSIONS[@]}"; do
        local jar_file="${SCRIPT_DIR}/jars/elasticsearch-${version}.jar"
        if [ -f "${jar_file}" ]; then
            log_info "Uploading ${version} to baseline..."
            mc cp "${jar_file}" minio/artifacts/elasticsearch-${version}.jar 2>/dev/null
        fi
    done

    log_success "Baseline upload complete"
}

# Upload to DeltaGlider
upload_deltaglider() {
    log_info "=========================================="
    log_info "Uploading to DELTAGLIDER (via Proxy)"
    log_info "=========================================="

    reset_bucket "deltaglider-data"

    for version in "${VERSIONS[@]}"; do
        local jar_file="${SCRIPT_DIR}/jars/elasticsearch-${version}.jar"
        if [ -f "${jar_file}" ]; then
            log_info "Uploading ${version} through DeltaGlider..."
            # Use curl directly for S3 PUT (mc has issues with the proxy)
            if curl -sf -X PUT "http://localhost:9012/default/elasticsearch-${version}.jar" \
                --data-binary "@${jar_file}" \
                -H "Content-Type: application/java-archive"; then
                log_success "Uploaded ${version}"
            else
                log_warn "Failed ${version}"
            fi
        fi
    done

    log_success "DeltaGlider upload complete"
}

# Measure storage
measure_storage() {
    log_info "Measuring storage..."

    mkdir -p "${SCRIPT_DIR}/results"

    echo "=== Baseline Storage ===" > "${SCRIPT_DIR}/results/baseline_storage.txt"
    mc du minio/artifacts 2>/dev/null >> "${SCRIPT_DIR}/results/baseline_storage.txt" || echo "N/A" >> "${SCRIPT_DIR}/results/baseline_storage.txt"
    mc ls minio/artifacts 2>/dev/null >> "${SCRIPT_DIR}/results/baseline_storage.txt"

    echo "=== DeltaGlider Storage ===" > "${SCRIPT_DIR}/results/deltaglider_storage.txt"
    mc du minio/deltaglider-data 2>/dev/null >> "${SCRIPT_DIR}/results/deltaglider_storage.txt" || echo "N/A" >> "${SCRIPT_DIR}/results/deltaglider_storage.txt"
    mc ls minio/deltaglider-data 2>/dev/null >> "${SCRIPT_DIR}/results/deltaglider_storage.txt"
}

# Generate report
generate_report() {
    log_info "Generating report..."

    local baseline_size=$(mc du minio/artifacts 2>/dev/null | awk '{print $1}')
    local deltaglider_size=$(mc du minio/deltaglider-data 2>/dev/null | awk '{print $1}')

    # Get numeric values for calculation
    local baseline_bytes=$(mc du --json minio/artifacts 2>/dev/null | grep -o '"size":[0-9]*' | cut -d: -f2 || echo "0")
    local deltaglider_bytes=$(mc du --json minio/deltaglider-data 2>/dev/null | grep -o '"size":[0-9]*' | cut -d: -f2 || echo "0")

    local savings="N/A"
    if [ "$baseline_bytes" -gt 0 ] && [ "$deltaglider_bytes" -gt 0 ]; then
        savings=$(echo "scale=1; (1 - $deltaglider_bytes / $baseline_bytes) * 100" | bc 2>/dev/null || echo "N/A")
    fi

    cat > "${SCRIPT_DIR}/results/report.md" << EOF
# DeltaGlider Storage Savings Report

## Test Configuration
- **Date**: $(date)
- **Artifacts**: Elasticsearch JAR files (${#VERSIONS[@]} versions)
- **Versions**: ${VERSIONS[*]}

## Results

### Baseline (Direct MinIO)
\`\`\`
$(cat "${SCRIPT_DIR}/results/baseline_storage.txt")
\`\`\`

### DeltaGlider (Delta Compression)
\`\`\`
$(cat "${SCRIPT_DIR}/results/deltaglider_storage.txt")
\`\`\`

## Summary

| Metric | Baseline | DeltaGlider |
|--------|----------|-------------|
| Storage | ${baseline_size:-N/A} | ${deltaglider_size:-N/A} |
| Savings | - | ${savings}% |

## How It Works

DeltaGlider Proxy applies **xdelta3 delta compression** to similar files:

1. First file uploaded → stored as-is (base version)
2. Subsequent similar files → only the delta (difference) is stored
3. On retrieval → original file is reconstructed transparently

This is particularly effective for:
- Sequential software versions (like Elasticsearch 7.17.0 → 7.17.14)
- Similar artifacts with shared code
- Backup systems with incremental changes

## Architecture

\`\`\`
Client → DeltaGlider Proxy → MinIO
              ↓
        xdelta3 compression
        (stores deltas only)
\`\`\`
EOF

    log_success "Report generated: results/report.md"
    echo ""
    echo "=========================================="
    echo "           RESULTS SUMMARY"
    echo "=========================================="
    echo ""
    echo "Baseline storage:    ${baseline_size:-N/A}"
    echo "DeltaGlider storage: ${deltaglider_size:-N/A}"
    echo "Storage savings:     ${savings}%"
    echo ""
    cat "${SCRIPT_DIR}/results/report.md"
}

# Main
main() {
    echo ""
    echo "╔══════════════════════════════════════════════════════════════╗"
    echo "║    DeltaGlider Proxy Storage Savings Demo (Simple)           ║"
    echo "║    Direct S3 Upload Test with Elasticsearch JARs             ║"
    echo "╚══════════════════════════════════════════════════════════════╝"
    echo ""

    ensure_services
    setup_mc
    download_jars

    upload_baseline
    echo ""
    sleep 2
    upload_deltaglider
    echo ""
    sleep 2

    measure_storage
    generate_report
}

main "$@"
