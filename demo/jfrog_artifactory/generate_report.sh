#!/usr/bin/env bash
#
# generate_report.sh - Generate storage savings report
#
# This script compares baseline and DeltaGlider storage measurements
# and produces a formatted Markdown report.
#
# Usage:
#   ./generate_report.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS_DIR="${SCRIPT_DIR}/results"
REPORT_FILE="${RESULTS_DIR}/report.md"

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[INFO]${NC} $1"; }
log_success() { echo -e "${GREEN}[SUCCESS]${NC} $1"; }

# Parse size string to bytes
parse_size() {
    local size_str=$1
    local num unit

    # Extract number and unit (e.g., "150MiB" -> "150" "MiB")
    num=$(echo "${size_str}" | grep -oE '[0-9]+\.?[0-9]*')
    unit=$(echo "${size_str}" | grep -oE '[A-Za-z]+')

    case "${unit}" in
        B)    echo "${num}" ;;
        KiB|KB|K) echo "$(echo "${num} * 1024" | bc)" ;;
        MiB|MB|M) echo "$(echo "${num} * 1024 * 1024" | bc)" ;;
        GiB|GB|G) echo "$(echo "${num} * 1024 * 1024 * 1024" | bc)" ;;
        TiB|TB|T) echo "$(echo "${num} * 1024 * 1024 * 1024 * 1024" | bc)" ;;
        *) echo "${num}" ;;
    esac
}

# Format bytes to human readable
format_bytes() {
    local bytes=$1

    if [ -z "${bytes}" ] || [ "${bytes}" = "0" ]; then
        echo "0 B"
        return
    fi

    if (( $(echo "${bytes} >= 1099511627776" | bc -l) )); then
        echo "$(echo "scale=2; ${bytes} / 1099511627776" | bc) TiB"
    elif (( $(echo "${bytes} >= 1073741824" | bc -l) )); then
        echo "$(echo "scale=2; ${bytes} / 1073741824" | bc) GiB"
    elif (( $(echo "${bytes} >= 1048576" | bc -l) )); then
        echo "$(echo "scale=2; ${bytes} / 1048576" | bc) MiB"
    elif (( $(echo "${bytes} >= 1024" | bc -l) )); then
        echo "$(echo "scale=2; ${bytes} / 1024" | bc) KiB"
    else
        echo "${bytes} B"
    fi
}

# Extract size from storage file
extract_size() {
    local file=$1

    if [ ! -f "${file}" ]; then
        echo "0"
        return
    fi

    # mc du output format: "SIZE\tOBJECTS\tBUCKET"
    local size_str
    size_str=$(head -1 "${file}" | awk '{print $1}')
    parse_size "${size_str}"
}

# Extract object count from storage file
extract_objects() {
    local file=$1

    if [ ! -f "${file}" ]; then
        echo "0"
        return
    fi

    grep -oE 'Objects: [0-9]+' "${file}" | awk '{print $2}' || echo "0"
}

# Main report generation
main() {
    log_info "Generating storage savings report..."

    mkdir -p "${RESULTS_DIR}"

    # Check if result files exist
    if [ ! -f "${RESULTS_DIR}/baseline_storage.txt" ]; then
        log_info "No baseline results found. Using placeholder values."
        echo "0B 0 objects" > "${RESULTS_DIR}/baseline_storage.txt"
        echo "Objects: 0" >> "${RESULTS_DIR}/baseline_storage.txt"
    fi

    if [ ! -f "${RESULTS_DIR}/deltaglider_storage.txt" ]; then
        log_info "No DeltaGlider results found. Using placeholder values."
        echo "0B 0 objects" > "${RESULTS_DIR}/deltaglider_storage.txt"
        echo "Objects: 0" >> "${RESULTS_DIR}/deltaglider_storage.txt"
    fi

    # Extract measurements
    local baseline_bytes deltaglider_bytes
    local baseline_objects deltaglider_objects

    baseline_bytes=$(extract_size "${RESULTS_DIR}/baseline_storage.txt")
    deltaglider_bytes=$(extract_size "${RESULTS_DIR}/deltaglider_storage.txt")
    baseline_objects=$(extract_objects "${RESULTS_DIR}/baseline_storage.txt")
    deltaglider_objects=$(extract_objects "${RESULTS_DIR}/deltaglider_storage.txt")

    # Calculate savings
    local savings_bytes savings_percent

    if [ "${baseline_bytes}" -gt 0 ] 2>/dev/null; then
        savings_bytes=$(echo "${baseline_bytes} - ${deltaglider_bytes}" | bc)
        savings_percent=$(echo "scale=1; (${savings_bytes} * 100) / ${baseline_bytes}" | bc)
    else
        savings_bytes=0
        savings_percent=0
    fi

    # Format sizes
    local baseline_human deltaglider_human savings_human
    baseline_human=$(format_bytes "${baseline_bytes}")
    deltaglider_human=$(format_bytes "${deltaglider_bytes}")
    savings_human=$(format_bytes "${savings_bytes}")

    # Generate report
    cat > "${REPORT_FILE}" <<EOF
# DeltaGlider Proxy Storage Savings Report

**Generated:** $(date -u '+%Y-%m-%d %H:%M:%S UTC')

## Test Configuration

| Parameter | Value |
|-----------|-------|
| Artifactory | JFrog Artifactory OSS |
| Backend Storage | MinIO |
| Test Artifacts | Elasticsearch Maven artifacts |
| Versions Tested | 7.17.0, 7.17.14, 7.17.18, 8.0.0, 8.1.0, 8.5.0, 8.10.0, 8.11.0, 8.12.0 |

## Storage Comparison

| Metric | Baseline (Direct MinIO) | DeltaGlider Proxy |
|--------|-------------------------|-------------------|
| **Storage Size** | ${baseline_human} | ${deltaglider_human} |
| **Object Count** | ${baseline_objects} | ${deltaglider_objects} |

## Savings Summary

| Metric | Value |
|--------|-------|
| **Storage Saved** | ${savings_human} |
| **Savings Percentage** | ${savings_percent}% |
| **Compression Ratio** | $(echo "scale=2; ${baseline_bytes} / (${deltaglider_bytes} + 1)" | bc)x |

## How It Works

1. **Baseline Test**: Artifacts stored directly in MinIO via Artifactory's S3 filestore
2. **DeltaGlider Test**: Artifacts routed through DeltaGlider Proxy, which applies xdelta3 compression

### Delta Compression Benefits

- Similar artifact versions (e.g., Elasticsearch 8.10.0 vs 8.11.0) share ~90%+ common data
- DeltaGlider stores only the differences (deltas)
- Full artifacts are reconstructed transparently on retrieval

## Raw Data

### Baseline Storage Output
\`\`\`
$(cat "${RESULTS_DIR}/baseline_storage.txt" 2>/dev/null || echo "No data")
\`\`\`

### DeltaGlider Storage Output
\`\`\`
$(cat "${RESULTS_DIR}/deltaglider_storage.txt" 2>/dev/null || echo "No data")
\`\`\`

---

*Report generated by DeltaGlider Proxy Demo*
EOF

    log_success "Report generated: ${REPORT_FILE}"

    # Also print summary to console
    echo ""
    echo "╔══════════════════════════════════════════════════════════════╗"
    echo "║                    STORAGE SAVINGS SUMMARY                    ║"
    echo "╠══════════════════════════════════════════════════════════════╣"
    printf "║  Baseline (Direct MinIO):    %-30s ║\n" "${baseline_human}"
    printf "║  DeltaGlider Proxy:          %-30s ║\n" "${deltaglider_human}"
    printf "║  ──────────────────────────────────────────────────────────  ║\n"
    printf "║  Storage Saved:              %-30s ║\n" "${savings_human}"
    printf "║  Savings Percentage:         %-30s ║\n" "${savings_percent}%"
    echo "╚══════════════════════════════════════════════════════════════╝"
    echo ""
    echo "Full report: ${REPORT_FILE}"
}

main "$@"
