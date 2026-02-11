#!/bin/bash
#
# run_demo_docker.sh - Run the complete demo in Docker
#
# This script orchestrates the entire demo without requiring local Maven/mc.
# All operations run inside Docker containers.
#
# Usage:
#   ./run_demo_docker.sh [baseline|deltaglider|all]

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

# Wait for Artifactory
wait_for_artifactory() {
    log_info "Waiting for Artifactory to be ready..."
    local max_attempts=60
    local attempt=1

    while [ $attempt -le $max_attempts ]; do
        if curl -sf "http://localhost:8082/artifactory/api/system/ping" >/dev/null 2>&1; then
            log_success "Artifactory is ready"
            return 0
        fi
        echo -n "."
        sleep 5
        ((attempt++))
    done

    log_error "Artifactory did not become ready"
    return 1
}

# Setup Artifactory repos from host
setup_artifactory_repos() {
    log_info "Setting up Artifactory repositories..."

    local ARTIFACTORY_URL="http://localhost:8081/artifactory"
    local AUTH="admin:password"

    curl -sf -u "${AUTH}" -X PUT "${ARTIFACTORY_URL}/api/repositories/maven-central-remote" \
        -H "Content-Type: application/json" \
        -d '{
            "key": "maven-central-remote",
            "rclass": "remote",
            "packageType": "maven",
            "url": "https://repo1.maven.org/maven2/",
            "description": "Maven Central Remote Repository"
        }' 2>/dev/null || log_warn "maven-central-remote may already exist"

    curl -sf -u "${AUTH}" -X PUT "${ARTIFACTORY_URL}/api/repositories/maven-local" \
        -H "Content-Type: application/json" \
        -d '{
            "key": "maven-local",
            "rclass": "local",
            "packageType": "maven",
            "description": "Local Maven Repository"
        }' 2>/dev/null || log_warn "maven-local may already exist"

    curl -sf -u "${AUTH}" -X PUT "${ARTIFACTORY_URL}/api/repositories/maven-all" \
        -H "Content-Type: application/json" \
        -d '{
            "key": "maven-all",
            "rclass": "virtual",
            "packageType": "maven",
            "repositories": ["maven-local", "maven-central-remote"],
            "defaultDeploymentRepo": "maven-local",
            "description": "Virtual Maven Repository"
        }' 2>/dev/null || log_warn "maven-all may already exist"

    log_success "Artifactory repositories configured"
}

# Switch binarystore config
switch_binarystore() {
    local config=$1
    log_info "Switching to ${config} binarystore configuration..."

    docker cp "${SCRIPT_DIR}/config/binarystore-${config}.xml" \
        demo-artifactory:/var/opt/jfrog/artifactory/etc/binarystore.xml

    log_info "Restarting Artifactory..."
    docker restart demo-artifactory

    wait_for_artifactory
}

# Measure storage using mc inside minio container
measure_storage() {
    local bucket=$1
    local label=$2
    local output_file=$3

    log_info "Measuring storage for ${label}..."

    # Use mc from inside the minio container
    docker exec demo-minio sh -c "mc alias set local http://localhost:9000 minio minio123 >/dev/null 2>&1; mc du local/${bucket}" > "${output_file}" 2>/dev/null || echo "0B 0 objects" > "${output_file}"

    # Get object count
    local count=$(docker exec demo-minio sh -c "mc alias set local http://localhost:9000 minio minio123 >/dev/null 2>&1; mc ls --recursive local/${bucket} 2>/dev/null | wc -l" || echo "0")
    echo "Objects: ${count}" >> "${output_file}"

    cat "${output_file}"
}

# Reset bucket
reset_bucket() {
    local bucket=$1
    log_info "Resetting bucket: ${bucket}..."

    docker exec demo-minio sh -c "mc alias set local http://localhost:9000 minio minio123 >/dev/null 2>&1; mc rm --recursive --force local/${bucket} 2>/dev/null; mc mb --ignore-existing local/${bucket}" 2>/dev/null || true

    log_success "Bucket ${bucket} reset"
}

# Run Maven fetch inside container
run_maven_fetch() {
    log_info "Starting Maven artifact fetch..."

    # Ensure maven-runner is up
    docker compose up -d maven-runner

    # Wait for container to be ready
    sleep 2

    # Create Maven settings inside container
    docker exec demo-maven-runner sh -c 'mkdir -p /root/.m2 && cat > /root/.m2/settings.xml << EOF
<settings xmlns="http://maven.apache.org/SETTINGS/1.2.0">
  <mirrors>
    <mirror>
      <id>artifactory</id>
      <mirrorOf>*</mirrorOf>
      <url>http://artifactory:8081/artifactory/maven-all</url>
      <name>Artifactory Maven Proxy</name>
    </mirror>
  </mirrors>
  <servers>
    <server>
      <id>artifactory</id>
      <username>admin</username>
      <password>password</password>
    </server>
  </servers>
</settings>
EOF'

    # Clear local cache
    docker exec demo-maven-runner sh -c 'rm -rf /root/.m2/repository/org/elasticsearch'

    # Fetch each version
    local versions=("7.17.0" "7.17.14" "7.17.18" "8.0.0" "8.1.0" "8.5.0" "8.10.0" "8.11.0" "8.12.0")

    for version in "${versions[@]}"; do
        log_info "Fetching Elasticsearch ${version}..."
        docker exec demo-maven-runner mvn dependency:get \
            -DgroupId=org.elasticsearch \
            -DartifactId=elasticsearch \
            -Dversion="${version}" \
            -Dtransitive=true \
            -q 2>/dev/null && log_success "Fetched ${version}" || log_warn "Failed ${version}"
    done

    log_success "Maven fetch complete"
}

# Run baseline test
run_baseline_test() {
    log_info "=========================================="
    log_info "Running BASELINE test (Direct MinIO)"
    log_info "=========================================="

    reset_bucket "artifacts"
    switch_binarystore "baseline"

    # Clear Maven cache before test
    docker exec demo-maven-runner sh -c 'rm -rf /root/.m2/repository/org/elasticsearch' 2>/dev/null || true

    run_maven_fetch

    # Allow time for writes to complete
    sleep 5

    measure_storage "artifacts" "Baseline" "${SCRIPT_DIR}/results/baseline_storage.txt"

    log_success "Baseline test complete"
}

# Run DeltaGlider test
run_deltaglider_test() {
    log_info "=========================================="
    log_info "Running DELTAGLIDER test (via Proxy)"
    log_info "=========================================="

    reset_bucket "deltaglider-data"
    switch_binarystore "deltaglider"

    # Clear Maven cache before test
    docker exec demo-maven-runner sh -c 'rm -rf /root/.m2/repository/org/elasticsearch' 2>/dev/null || true

    run_maven_fetch

    # Allow time for writes to complete
    sleep 5

    measure_storage "deltaglider-data" "DeltaGlider" "${SCRIPT_DIR}/results/deltaglider_storage.txt"

    log_success "DeltaGlider test complete"
}

# Generate report
generate_report() {
    log_info "Generating report..."

    local baseline_size=$(head -1 "${SCRIPT_DIR}/results/baseline_storage.txt" 2>/dev/null || echo "N/A")
    local deltaglider_size=$(head -1 "${SCRIPT_DIR}/results/deltaglider_storage.txt" 2>/dev/null || echo "N/A")

    cat > "${SCRIPT_DIR}/results/report.md" << EOF
# DeltaGlider Storage Savings Report

## Test Configuration
- **Date**: $(date)
- **Artifacts**: Elasticsearch (9 versions: 7.17.x, 8.x)
- **Source**: Maven Central via JFrog Artifactory OSS

## Results

### Baseline (Direct MinIO)
\`\`\`
$(cat "${SCRIPT_DIR}/results/baseline_storage.txt" 2>/dev/null || echo "No data")
\`\`\`

### DeltaGlider (Delta Compression)
\`\`\`
$(cat "${SCRIPT_DIR}/results/deltaglider_storage.txt" 2>/dev/null || echo "No data")
\`\`\`

## Analysis

DeltaGlider Proxy applies xdelta3 delta compression to similar files,
storing only the differences between versions. This is particularly
effective for versioned artifacts like Elasticsearch where consecutive
versions share significant common code.

### Expected Savings
- **Typical**: 50-80% storage reduction
- **Best case** (sequential minor versions): Up to 90%

## Architecture

\`\`\`
Maven Client → Artifactory OSS → DeltaGlider Proxy → MinIO
                                      ↓
                              xdelta3 compression
                              (stores deltas only)
\`\`\`
EOF

    log_success "Report generated: results/report.md"
    echo ""
    cat "${SCRIPT_DIR}/results/report.md"
}

# Main
main() {
    local mode="${1:-all}"

    echo ""
    echo "╔══════════════════════════════════════════════════════════════╗"
    echo "║    DeltaGlider Proxy Storage Savings Demo (Docker)           ║"
    echo "║    JFrog Artifactory OSS + Elasticsearch Artifacts           ║"
    echo "╚══════════════════════════════════════════════════════════════╝"
    echo ""

    mkdir -p "${SCRIPT_DIR}/results"

    # Ensure all services are running
    log_info "Starting Docker services..."
    docker compose up -d

    wait_for_artifactory
    setup_artifactory_repos

    case "${mode}" in
        baseline)
            run_baseline_test
            ;;
        deltaglider)
            run_deltaglider_test
            ;;
        all)
            run_baseline_test
            echo ""
            log_info "Pausing 10 seconds before DeltaGlider test..."
            sleep 10
            run_deltaglider_test
            echo ""
            generate_report
            ;;
        *)
            echo "Usage: $0 [baseline|deltaglider|all]"
            exit 1
            ;;
    esac
}

main "$@"
