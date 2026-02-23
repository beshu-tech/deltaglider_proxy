# DeltaGlider Proxy: S3-Compatible Object Storage with DeltaGlider Deduplication

> **Note:** This is the original design document. Phases 1-4 are complete. Some implementation details (file names, type names, module layout, size limits) evolved during development — see `STORAGE_FORMAT.md`, `CONTRIBUTING.md`, and `HARDENING_PLAN.md` for current specifics.

## Overview

DeltaGlider Proxy is an S3-compatible object storage server implementing the **DeltaGlider algorithm** for transparent delta-based deduplication. By storing similar files as compact binary deltas (using xdelta3), DeltaGlider Proxy achieves 90-99% storage reduction for versioned artifacts.

### Target Use Cases
- Software release artifacts (.zip, .jar, .tar.gz)
- Database backups and snapshots
- ML model checkpoints
- Any versioned binary content with incremental changes

### Scope
- **Endpoints**: PUT, GET, HEAD, DELETE, LIST, COPY, multipart upload (Create, UploadPart, Complete, Abort, ListParts, ListUploads), bucket operations
- **Storage**: Local filesystem or upstream S3 backend
- **Authentication**: Optional SigV4 with CORS preflight support
- **Deduplication**: DeltaGlider with xdelta3
- **Limitations**: 100MB max object size, single-node

---

## Architecture

### DeltaGlider Model

```
┌─────────────────────────────────────────────────────────────────┐
│                        S3 API Layer                             │
│                   PUT / GET / LIST handlers                     │
├─────────────────────────────────────────────────────────────────┤
│                     DeltaGlider Engine                          │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────────────────┐  │
│  │ File Router │  │ DeltaSpace   │  │ xdelta3 Codec          │  │
│  │ (type→strategy)│ │ Manager      │  │ encode/decode          │  │
│  └─────────────┘  └──────────────┘  └────────────────────────┘  │
├─────────────────────────────────────────────────────────────────┤
│                      Storage Backend                            │
│              Filesystem with metadata (xattr / S3 headers)      │
└─────────────────────────────────────────────────────────────────┘
```

### DeltaSpace Concept

Each S3 prefix forms a **DeltaSpace** containing:
- **One reference file**: Full content of the first/best baseline
- **N delta files**: Compact patches against the reference

```
data/
├── releases/                      # DeltaSpace: "releases"
│   ├── _reference.bin             # Base file (full content)
│   ├── _deltaspace.json           # Metadata: key mappings, checksums
│   ├── v1.0.0.zip.delta           # Delta from reference
│   └── v1.0.1.zip.delta           # Delta from reference (NOT chained!)
└── backups/                       # DeltaSpace: "backups"
    ├── _reference.bin
    └── ...
```

**Key Insight**: All deltas reference the SAME base file (no chains). This ensures O(1) reconstruction time.

### Storage Decision Flow

```
PUT object
    │
    ▼
┌─────────────────┐
│ File type       │──── .exe/.dll/unknown ────▶ Store as passthrough (no delta benefit)
│ detection       │
└────────┬────────┘
         │ .zip/.jar/.tar.gz
         ▼
┌─────────────────┐
│ DeltaSpace has  │──── NO ────▶ Store as reference.bin
│ reference?      │
└────────┬────────┘
         │ YES
         ▼
┌─────────────────┐
│ Compute delta   │
│ via xdelta3     │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ delta_size /    │──── ratio >= 0.5 ────▶ Store as passthrough (poor compression)
│ original_size   │
└────────┬────────┘
         │ ratio < 0.5
         ▼
    Store as .delta
```

---

## Module Structure

```
src/
├── main.rs                     # Axum server bootstrap, config
├── config.rs                   # Configuration (port, data_dir, max_ratio)
├── auth.rs                     # SigV4 authentication middleware
├── multipart.rs                # In-memory multipart upload state management
├── demo.rs                     # Embedded React demo UI (rust-embed, served on S3 port + 1)
│
├── api/
│   ├── mod.rs
│   ├── handlers.rs             # S3 API endpoint handlers (PUT, GET, LIST, multipart, etc.)
│   ├── xml.rs                  # S3 XML response/request builders
│   └── errors.rs               # S3 error codes (NoSuchKey, NoSuchUpload, etc.)
│
├── deltaglider/
│   ├── mod.rs
│   ├── engine.rs               # Main orchestrator: store/retrieve with delta logic
│   ├── deltaspace.rs           # DeltaSpace CRUD, reference management
│   ├── codec.rs                # xdelta3 encode/decode wrappers
│   ├── file_router.rs          # Extension → compression strategy mapping
│   └── cache.rs                # LRU cache for reference files
│
├── storage/
│   ├── mod.rs
│   ├── traits.rs               # StorageBackend trait (ports)
│   ├── filesystem.rs           # Filesystem implementation (adapter)
│   └── s3.rs                   # S3 backend implementation
│
└── types.rs                    # ObjectKey, ContentHash, ObjectMetadata
demo/s3-browser/ui/             # React demo UI source (Vite + TypeScript)
```

---

## Core Types

```rust
/// S3 object key parsed into components
pub struct ObjectKey {
    pub bucket: String,        // Real S3 bucket name
    pub prefix: String,        // Parent path = DeltaSpace identifier
    pub filename: String,      // Object name
}

/// How an object is physically stored
pub enum StorageType {
    Reference,                 // Full content, serves as delta base
    Delta { ratio: f32 },      // Compressed diff from reference
    Passthrough,               // Stored as-is (non-delta-eligible type)
}

/// Object metadata persisted alongside content
pub struct ObjectMetadata {
    pub key: String,
    pub sha256: String,        // Checksum of ORIGINAL content
    pub original_size: u64,    // Size of ORIGINAL content
    pub storage_type: StorageType,
    pub created_at: DateTime<Utc>,
}

/// DeltaSpace state
pub struct DeltaSpace {
    pub prefix: String,
    pub reference_sha256: Option<String>,
    pub objects: HashMap<String, ObjectMetadata>,
}

/// Configuration
pub struct Config {
    pub listen_addr: SocketAddr,
    pub data_dir: PathBuf,
    pub max_delta_ratio: f32,  // Default: 0.5
    pub cache_size_mb: usize,  // Reference cache size
}
```

---

## Phase 1: Foundation

**Goal**: Working S3 server with passthrough storage (no delta compression yet)

### Tasks

1. **Project setup**
   - Update Cargo.toml with dependencies
   - Create module structure (empty files)

2. **Configuration**
   - Environment-based config (listen address, data directory)
   - Config struct with defaults

3. **Storage backend**
   - `StorageBackend` trait: `put`, `get`, `list`, `delete`, `exists`
   - Filesystem implementation with directory structure
   - Metadata storage via xattr (filesystem) or S3 user metadata headers (S3)

4. **S3 API handlers**
   - PUT: `PUT /{bucket}/{key}` → store object
   - GET: `GET /{bucket}/{key}` → retrieve object
   - LIST: `GET /{bucket}?list-type=2&prefix=` → list objects

5. **S3 response formatting**
   - XML builder for ListObjectsV2 response
   - Error XML responses (NoSuchKey, InternalError)
   - Proper headers (ETag, Content-Type, Content-Length)

### Deliverables
- Server starts and accepts S3 requests
- Objects stored/retrieved without transformation
- LIST returns XML with object keys and sizes

---

## Phase 2: DeltaGlider Core

**Goal**: Delta compression engine (isolated, testable)

### Tasks

1. **File router**
   - Extension detection: `.zip`, `.jar`, `.war`, `.tar`, `.tar.gz`, `.tgz`
   - Strategy enum: `DeltaEligible` | `Passthrough`
   - Configurable extension list

2. **xdelta3 codec wrapper**
   ```rust
   pub fn encode(source: &[u8], target: &[u8]) -> Result<Vec<u8>, CodecError>;
   pub fn decode(source: &[u8], delta: &[u8]) -> Result<Vec<u8>, CodecError>;
   ```
   - Error handling for xdelta3 failures
   - Size validation (reject >10MB)

3. **DeltaSpace manager**
   - `get_or_create_deltaspace(prefix: &str)`
   - `get_reference(prefix: &str) -> Option<Vec<u8>>`
   - `set_reference(prefix: &str, data: &[u8])`
   - `store_delta(prefix: &str, key: &str, delta: &[u8])`
   - Persistence: `_deltaspace.json` metadata file per prefix

4. **Reference cache**
   - LRU cache for frequently-accessed reference files
   - Configurable size limit
   - Cache invalidation on reference update

### Deliverables
- Unit tests for codec (encode → decode roundtrip)
- Unit tests for deltaspace (reference management)
- File router correctly categorizes extensions

---

## Phase 3: Full Integration

**Goal**: Wire DeltaGlider into S3 handlers

### Tasks

1. **DeltaGlider engine**
   - `store(key: &str, data: &[u8]) -> Result<ObjectMetadata>`
     - Route by file type
     - Compute delta if eligible
     - Decide storage type by ratio
     - Persist with metadata
   - `retrieve(key: &str) -> Result<Vec<u8>>`
     - Load metadata
     - If delta: fetch reference, apply patch
     - Verify SHA256 checksum
     - Return original content

2. **Handler integration**
   - PUT handler calls `engine.store()`
   - GET handler calls `engine.retrieve()`
   - LIST handler reads deltaspace metadata, returns original keys

3. **S3 header correctness**
   - ETag: MD5 of original content (not stored bytes)
   - Content-Length: original size (not delta size)
   - x-amz-meta-*: expose storage type for debugging

4. **Error handling**
   - Missing reference file → 500 Internal Error (log corruption)
   - SHA256 mismatch → 500 Internal Error (log corruption)
   - Delta decode failure → 500 Internal Error

### Deliverables
- PUT of similar files creates deltas
- GET reconstructs original content
- LIST shows logical keys (not .delta suffixes)

---

## Phase 4: Testing & Hardening

**Goal**: Comprehensive tests, edge case handling

### Integration Test Plan

```rust
#[tokio::test]
async fn test_delta_deduplication_e2e() {
    let server = TestServer::start().await;
    let client = s3_client(&server.addr);

    // Generate test files: 100KB base + 2 variants with small changes
    let base = generate_binary_file(100_000, 42);      // seed=42
    let variant1 = mutate_binary(&base, 0.01);         // 1% different
    let variant2 = mutate_binary(&base, 0.02);         // 2% different

    // Upload all three
    client.put("test/base.zip", &base).await?;
    client.put("test/v1.zip", &variant1).await?;
    client.put("test/v2.zip", &variant2).await?;

    // Verify retrieval matches original
    assert_eq!(client.get("test/base.zip").await?, base);
    assert_eq!(client.get("test/v1.zip").await?, variant1);
    assert_eq!(client.get("test/v2.zip").await?, variant2);

    // Verify LIST returns all keys
    let list = client.list("test/").await?;
    assert!(list.contains("base.zip"));
    assert!(list.contains("v1.zip"));
    assert!(list.contains("v2.zip"));

    // Verify storage savings (implementation detail check)
    let storage_size = server.data_dir_size();
    let naive_size = base.len() + variant1.len() + variant2.len();
    assert!(storage_size < naive_size / 2, "Expected >50% storage reduction");
}
```

### Additional Test Cases

| Test | Description |
|------|-------------|
| `test_non_delta_file` | Upload .exe file → stored as passthrough, no delta |
| `test_poor_compression_ratio` | Upload very different file → stored as passthrough |
| `test_empty_deltaspace` | First upload becomes reference |
| `test_concurrent_uploads` | Multiple uploads to same deltaspace |
| `test_get_nonexistent` | GET missing key → 404 NoSuchKey |
| `test_large_file_rejection` | PUT >10MB → 400 error |
| `test_corrupted_delta` | Tamper with delta file → GET returns 500 |

### Hardening Tasks

1. **Graceful error recovery**
   - Detect corrupted deltaspace metadata → rebuild from files
   - Missing reference → return error, don't panic

2. **Concurrent access**
   - File locking for deltaspace metadata updates
   - Atomic file writes (write to temp, rename)

3. **Observability**
   - Structured logging (tracing crate)
   - Metrics: delta ratio histogram, cache hit rate

---

## Tricky Details & Mitigations

### 1. Reference Replacement Strategy

**Problem**: When a new file has poor delta ratio (>0.5), should it replace the reference?

**Decision**: NO. The original reference is "sticky". Poor-ratio files are stored as passthrough alongside. This avoids:
- Invalidating existing deltas
- Complex re-encoding logic
- Race conditions during replacement

**Trade-off**: Some storage waste for outlier files. Acceptable for MVP.

### 2. DeltaSpace Boundary Detection

**Problem**: How to map `bucket/path/to/file.zip` to a deltaspace?

**Decision**: Use immediate parent directory as deltaspace.
- `releases/v1.0.0.zip` → deltaspace = `releases/`
- `backups/db/monday.zip` → deltaspace = `backups/db/`
- `file.zip` (root) → deltaspace = `` (empty string, files stored at root)

### 3. xdelta3 Memory Usage

**Problem**: xdelta3 loads entire source and target into memory.

**Mitigation**:
- Enforce 10MB max object size
- Document limitation clearly
- Future: streaming/chunked approach for large files

### 4. LIST Response Key Mapping

**Problem**: Storage has `v1.0.0.zip.delta` but LIST must show `v1.0.0.zip`.

**Solution**: DeltaSpace metadata contains `original_key → storage_path` mapping. LIST reads metadata, returns original keys.

### 5. SHA256 Verification Failure

**Problem**: Reconstructed content doesn't match stored checksum.

**Response**:
- Log error with full context (key, expected hash, actual hash)
- Return 500 Internal Server Error
- Do NOT return corrupted data

### 6. Concurrent First Upload Race

**Problem**: Two clients upload first file to empty deltaspace simultaneously.

**Mitigation**:
- Use file locking when creating deltaspace
- First writer wins (creates reference)
- Second writer computes delta against new reference

---

## Dependencies

```toml
[dependencies]
# Web framework
axum = "0.7"
tokio = { version = "1", features = ["full"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Delta compression
xdelta3 = "0.1.5"

# Utilities
sha2 = "0.10"              # SHA256 checksums
hex = "0.4"                # Hash encoding
md-5 = "0.10"              # ETag generation
chrono = { version = "0.4", features = ["serde"] }
quick-xml = "0.36"         # S3 XML responses
thiserror = "2"            # Error types
tracing = "0.1"            # Structured logging
tracing-subscriber = "0.3"
lru = "0.12"               # Reference cache
parking_lot = "0.12"       # Fast locks

[dev-dependencies]
reqwest = { version = "0.12", features = ["json"] }
tempfile = "3"
rand = "0.8"
```

---

## Success Criteria

| Metric | Target |
|--------|--------|
| Storage reduction | >50% for similar files (1-5% diff) |
| GET latency | <50ms for delta reconstruction (10MB) |
| PUT throughput | >100 req/s (small files) |
| Test coverage | All core paths covered |
| S3 compatibility | Works with standard S3 CLI/SDK |

---

## Future Enhancements (Out of Scope)

- Multi-node clustering with distributed deltaspace
- Streaming for large files (>10MB)
- Automatic reference rotation (better compression over time)
- ~~DELETE endpoint with garbage collection~~ — ✅ Implemented
- ~~Multipart upload support~~ — ✅ Implemented (CreateMultipartUpload, UploadPart, CompleteMultipartUpload, AbortMultipartUpload, ListParts, ListMultipartUploads)
- ~~Bucket creation/deletion APIs~~ — ✅ Implemented (CreateBucket, HeadBucket, DeleteBucket, ListBuckets)
- ~~Authentication (S3 signature v4)~~ — ✅ Implemented (optional SigV4 with CORS preflight support)
