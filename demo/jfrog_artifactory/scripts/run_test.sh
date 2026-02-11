#!/bin/bash
#
# run_test.sh - Containerized demo test script
#
# This script runs inside the maven-runner container to fetch artifacts
# through Artifactory and measure storage savings.
#
# Usage:
#   ./run_test.sh [baseline|deltaglider|all]

set -e

# Configuration
ARTIFACTORY_URL="${ARTIFACTORY_URL:-http://artifactory:8081/artifactory}"
MINIO_ENDPOINT="${MINIO_ENDPOINT:-http://minio:9000}"

# Elasticsearch versions to fetch
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

# Setup Maven settings to use Artifactory
setup_maven() {
    log_info "Configuring Maven to use Artifactory..."

    mkdir -p ~/.m2
    cat > ~/.m2/settings.xml <<EOF
<settings xmlns="http://maven.apache.org/SETTINGS/1.2.0"
          xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
          xsi:schemaLocation="http://maven.apache.org/SETTINGS/1.2.0 http://maven.apache.org/xsd/settings-1.2.0.xsd">
  <mirrors>
    <mirror>
      <id>artifactory</id>
      <mirrorOf>*</mirrorOf>
      <url>${ARTIFACTORY_URL}/maven-all</url>
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

    log_success "Maven configured"
}

# Setup Artifactory repositories via REST API
setup_artifactory_repos() {
    log_info "Setting up Artifactory repositories..."

    local AUTH="admin:password"

    # Create remote repository for Maven Central
    log_info "Creating maven-central-remote repository..."
    curl -sf -u "${AUTH}" -X PUT "${ARTIFACTORY_URL}/api/repositories/maven-central-remote" \
        -H "Content-Type: application/json" \
        -d '{
            "key": "maven-central-remote",
            "rclass": "remote",
            "packageType": "maven",
            "url": "https://repo1.maven.org/maven2/",
            "description": "Maven Central Remote Repository"
        }' 2>/dev/null || log_warn "maven-central-remote may already exist"

    # Create local repository
    log_info "Creating maven-local repository..."
    curl -sf -u "${AUTH}" -X PUT "${ARTIFACTORY_URL}/api/repositories/maven-local" \
        -H "Content-Type: application/json" \
        -d '{
            "key": "maven-local",
            "rclass": "local",
            "packageType": "maven",
            "description": "Local Maven Repository"
        }' 2>/dev/null || log_warn "maven-local may already exist"

    # Create virtual repository
    log_info "Creating maven-all virtual repository..."
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

# Fetch Elasticsearch artifacts
fetch_artifacts() {
    log_info "Fetching ${#VERSIONS[@]} versions of Elasticsearch..."

    # Clear local cache
    rm -rf ~/.m2/repository/org/elasticsearch

    local success=0
    local failed=0

    for version in "${VERSIONS[@]}"; do
        log_info "Fetching Elasticsearch ${version}..."

        if mvn dependency:get \
            -DgroupId=org.elasticsearch \
            -DartifactId=elasticsearch \
            -Dversion="${version}" \
            -Dtransitive=true \
            -q 2>/dev/null; then
            log_success "Fetched ${version}"
            ((success++))
        else
            log_warn "Failed to fetch ${version}"
            ((failed++))
        fi
    done

    log_info "Fetch complete: ${success} succeeded, ${failed} failed"
}

# Switch Artifactory binarystore configuration
switch_binarystore() {
    local config=$1
    local AUTH="admin:password"

    log_info "Switching Artifactory to ${config} configuration..."

    # Copy config file to Artifactory container
    docker cp "/workspace/config/binarystore-${config}.xml" \
        demo-artifactory:/var/opt/jfrog/artifactory/etc/binarystore.xml 2>/dev/null || {
        # If docker cp fails (running inside container), use curl to restart
        log_warn "Cannot copy config from inside container, assuming config is already set"
    }

    # Restart Artifactory (this needs to be done from host)
    log_info "Please restart Artifactory from host: docker restart demo-artifactory"
}

# Main
main() {
    local mode="${1:-all}"

    echo ""
    echo "=============================================="
    echo "  DeltaGlider Storage Savings Demo"
    echo "  Running inside Maven container"
    echo "=============================================="
    echo ""

    mkdir -p /workspace/results

    setup_artifactory_repos
    setup_maven

    case "${mode}" in
        baseline|deltaglider|all)
            fetch_artifacts
            ;;
        *)
            echo "Usage: $0 [baseline|deltaglider|all]"
            exit 1
            ;;
    esac

    log_success "Test complete!"
}

main "$@"
