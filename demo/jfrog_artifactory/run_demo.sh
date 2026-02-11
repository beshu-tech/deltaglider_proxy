#!/usr/bin/env bash
#
# run_demo.sh - Populate Artifactory cache with Elasticsearch artifacts
#
# This script fetches multiple versions of Elasticsearch from Maven Central
# via Artifactory, triggering cache population in MinIO.
#
# Usage:
#   ./run_demo.sh [baseline|deltaglider]
#
# The script will:
#   1. Configure Maven to use Artifactory as a mirror
#   2. Fetch multiple Elasticsearch versions (with transitive dependencies)
#   3. Measure and report MinIO storage usage

set -e

# Configuration
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ARTIFACTORY_URL="${ARTIFACTORY_URL:-http://localhost:8081/artifactory/maven-all}"
MINIO_ALIAS="${MINIO_ALIAS:-minio}"
MINIO_ENDPOINT="${MINIO_ENDPOINT:-http://localhost:9010}"
MINIO_ACCESS_KEY="${MINIO_ACCESS_KEY:-minio}"
MINIO_SECRET_KEY="${MINIO_SECRET_KEY:-minio123}"

# Buckets
BASELINE_BUCKET="artifacts"
DELTAGLIDER_BUCKET="deltaglider-data"

# Elasticsearch versions to fetch (mix of major/minor versions for good delta potential)
VERSIONS=(
    "7.17.0"
    "7.17.14"
    "7.17.18"
    "8.0.0"
    "8.1.0"
    "8.5.0"
    "8.10.0"
    "8.11.0"
    "8.12.0"
)

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info() { echo -e "${BLUE}[INFO]${NC} $1"; }
log_success() { echo -e "${GREEN}[SUCCESS]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }

# Check prerequisites
check_prerequisites() {
    log_info "Checking prerequisites..."

    local missing=()

    command -v mvn >/dev/null 2>&1 || missing+=("maven")
    command -v mc >/dev/null 2>&1 || missing+=("minio-client (mc)")
    command -v curl >/dev/null 2>&1 || missing+=("curl")
    command -v docker >/dev/null 2>&1 || missing+=("docker")

    if [ ${#missing[@]} -gt 0 ]; then
        log_error "Missing required tools: ${missing[*]}"
        echo ""
        echo "Install instructions:"
        echo "  - maven: brew install maven (macOS) or apt install maven (Linux)"
        echo "  - minio-client: brew install minio/stable/mc (macOS) or see https://min.io/docs/minio/linux/reference/minio-mc.html"
        echo "  - curl: Usually pre-installed"
        echo "  - docker: https://docs.docker.com/get-docker/"
        exit 1
    fi

    log_success "All prerequisites installed"
}

# Wait for Artifactory to be ready
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

    log_error "Artifactory did not become ready in time"
    exit 1
}

# Configure MinIO client
setup_minio_client() {
    log_info "Configuring MinIO client..."
    mc alias set ${MINIO_ALIAS} ${MINIO_ENDPOINT} ${MINIO_ACCESS_KEY} ${MINIO_SECRET_KEY} --api S3v4
    log_success "MinIO client configured"
}

# Configure Maven settings to use Artifactory
setup_maven() {
    log_info "Configuring Maven to use Artifactory..."

    mkdir -p ~/.m2

    # Backup existing settings if present
    if [ -f ~/.m2/settings.xml ]; then
        cp ~/.m2/settings.xml ~/.m2/settings.xml.backup.$(date +%s)
        log_warn "Backed up existing Maven settings"
    fi

    cat > ~/.m2/settings.xml <<EOF
<settings xmlns="http://maven.apache.org/SETTINGS/1.2.0"
          xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
          xsi:schemaLocation="http://maven.apache.org/SETTINGS/1.2.0 http://maven.apache.org/xsd/settings-1.2.0.xsd">
  <mirrors>
    <mirror>
      <id>artifactory</id>
      <mirrorOf>*</mirrorOf>
      <url>${ARTIFACTORY_URL}</url>
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
EOF

    log_success "Maven configured to use Artifactory at ${ARTIFACTORY_URL}"
}

# Setup Artifactory repositories via REST API
setup_artifactory_repos() {
    log_info "Setting up Artifactory repositories..."

    local ARTIFACTORY_BASE="http://localhost:8081/artifactory"
    local AUTH="admin:password"

    # Create remote repository for Maven Central
    log_info "Creating maven-central-remote repository..."
    curl -sf -u "${AUTH}" -X PUT "${ARTIFACTORY_BASE}/api/repositories/maven-central-remote" \
        -H "Content-Type: application/json" \
        -d '{
            "key": "maven-central-remote",
            "rclass": "remote",
            "packageType": "maven",
            "url": "https://repo1.maven.org/maven2/",
            "description": "Maven Central Remote Repository"
        }' || log_warn "maven-central-remote may already exist"

    # Create local repository
    log_info "Creating maven-local repository..."
    curl -sf -u "${AUTH}" -X PUT "${ARTIFACTORY_BASE}/api/repositories/maven-local" \
        -H "Content-Type: application/json" \
        -d '{
            "key": "maven-local",
            "rclass": "local",
            "packageType": "maven",
            "description": "Local Maven Repository"
        }' || log_warn "maven-local may already exist"

    # Create virtual repository combining both
    log_info "Creating maven-all virtual repository..."
    curl -sf -u "${AUTH}" -X PUT "${ARTIFACTORY_BASE}/api/repositories/maven-all" \
        -H "Content-Type: application/json" \
        -d '{
            "key": "maven-all",
            "rclass": "virtual",
            "packageType": "maven",
            "repositories": ["maven-local", "maven-central-remote"],
            "defaultDeploymentRepo": "maven-local",
            "description": "Virtual Maven Repository"
        }' || log_warn "maven-all may already exist"

    log_success "Artifactory repositories configured"
}

# Fetch Elasticsearch artifacts
fetch_artifacts() {
    log_info "Starting artifact fetch..."
    echo ""
    echo "Fetching ${#VERSIONS[@]} versions of Elasticsearch with transitive dependencies."
    echo "This may take several minutes depending on network speed."
    echo ""

    # Clear local Maven cache to force fresh downloads through Artifactory
    log_info "Clearing local Elasticsearch cache..."
    rm -rf ~/.m2/repository/org/elasticsearch

    local success_count=0
    local fail_count=0

    for version in "${VERSIONS[@]}"; do
        echo ""
        log_info "Fetching Elasticsearch ${version}..."

        if mvn dependency:get \
            -DgroupId=org.elasticsearch \
            -DartifactId=elasticsearch \
            -Dversion="${version}" \
            -Dtransitive=true \
            -q 2>/dev/null; then
            log_success "Fetched Elasticsearch ${version}"
            ((success_count++))
        else
            log_warn "Failed to fetch Elasticsearch ${version} (may not exist)"
            ((fail_count++))
        fi
    done

    echo ""
    log_info "Fetch complete: ${success_count} succeeded, ${fail_count} failed"
}

# Measure storage usage
measure_storage() {
    local bucket=$1
    local label=$2
    local output_file=$3

    log_info "Measuring storage for ${label}..."

    # Get bucket size
    local size_output
    size_output=$(mc du ${MINIO_ALIAS}/${bucket} 2>/dev/null || echo "0B 0 objects")

    echo "${size_output}" | tee "${output_file}"

    # Also get object count
    local object_count
    object_count=$(mc ls --recursive ${MINIO_ALIAS}/${bucket} 2>/dev/null | wc -l || echo "0")
    echo "Objects: ${object_count}" | tee -a "${output_file}"
}

# Reset storage
reset_storage() {
    local bucket=$1

    log_info "Resetting bucket: ${bucket}..."

    # Remove all objects
    mc rm --recursive --force ${MINIO_ALIAS}/${bucket} 2>/dev/null || true

    # Recreate bucket
    mc mb --ignore-existing ${MINIO_ALIAS}/${bucket}

    log_success "Bucket ${bucket} reset"
}

# Switch Artifactory binarystore configuration
switch_binarystore() {
    local config=$1

    log_info "Switching Artifactory to ${config} configuration..."

    # Copy new config
    docker cp "${SCRIPT_DIR}/config/binarystore-${config}.xml" \
        demo-artifactory:/var/opt/jfrog/artifactory/etc/binarystore.xml

    # Restart Artifactory
    log_info "Restarting Artifactory..."
    docker restart demo-artifactory

    # Wait for it to come back
    wait_for_artifactory
}

# Run baseline test
run_baseline_test() {
    log_info "=========================================="
    log_info "Running BASELINE test (Direct MinIO)"
    log_info "=========================================="

    # Reset storage
    reset_storage "${BASELINE_BUCKET}"

    # Clear Artifactory cache (restart clears in-memory cache)
    switch_binarystore "baseline"

    # Fetch artifacts
    fetch_artifacts

    # Measure storage
    measure_storage "${BASELINE_BUCKET}" "Baseline" "${SCRIPT_DIR}/results/baseline_storage.txt"

    log_success "Baseline test complete"
}

# Run DeltaGlider test
run_deltaglider_test() {
    log_info "=========================================="
    log_info "Running DELTAGLIDER test (via DeltaGlider Proxy)"
    log_info "=========================================="

    # Reset storage
    reset_storage "${DELTAGLIDER_BUCKET}"

    # Clear Artifactory cache and switch config
    switch_binarystore "deltaglider"

    # Clear local Maven cache again
    rm -rf ~/.m2/repository/org/elasticsearch

    # Fetch artifacts
    fetch_artifacts

    # Measure storage
    measure_storage "${DELTAGLIDER_BUCKET}" "DeltaGlider" "${SCRIPT_DIR}/results/deltaglider_storage.txt"

    log_success "DeltaGlider test complete"
}

# Main function
main() {
    local mode="${1:-all}"

    echo ""
    echo "╔══════════════════════════════════════════════════════════════╗"
    echo "║    DeltaGlider Proxy Storage Savings Demo                    ║"
    echo "║    JFrog Artifactory OSS + Elasticsearch Artifacts           ║"
    echo "╚══════════════════════════════════════════════════════════════╝"
    echo ""

    # Create results directory
    mkdir -p "${SCRIPT_DIR}/results"

    # Prerequisites
    check_prerequisites
    setup_minio_client

    case "${mode}" in
        baseline)
            wait_for_artifactory
            setup_artifactory_repos
            setup_maven
            run_baseline_test
            ;;
        deltaglider)
            wait_for_artifactory
            setup_artifactory_repos
            setup_maven
            run_deltaglider_test
            ;;
        all)
            wait_for_artifactory
            setup_artifactory_repos
            setup_maven
            run_baseline_test
            echo ""
            echo "Pausing 10 seconds before DeltaGlider test..."
            sleep 10
            run_deltaglider_test
            echo ""
            log_info "Running report generation..."
            "${SCRIPT_DIR}/generate_report.sh"
            ;;
        *)
            echo "Usage: $0 [baseline|deltaglider|all]"
            echo ""
            echo "  baseline    - Run only baseline test (direct MinIO)"
            echo "  deltaglider - Run only DeltaGlider test"
            echo "  all         - Run both tests and generate report (default)"
            exit 1
            ;;
    esac
}

main "$@"
