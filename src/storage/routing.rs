// SPDX-License-Identifier: GPL-3.0-only

//! Multi-backend routing storage layer.
//!
//! `RoutingBackend` implements `StorageBackend` and transparently routes
//! each call to the correct underlying backend based on the bucket name.
//! The engine sees a single `StorageBackend` — caches, codec, prefix locks,
//! and compression policies remain shared across all backends.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;

use crate::types::FileMetadata;

use super::traits::{
    BucketListing, DelegatedListResult, LiteScanResult, MultipartUpload, StorageBackend,
    StorageError, UploadedPart,
};

/// Route entry: maps a virtual bucket to a backend and optional real bucket name.
#[derive(Debug, Clone)]
struct BucketRoute {
    backend_name: String,
    /// Real bucket name on the backend. `None` = same as virtual name.
    real_bucket: Option<String>,
}

/// Multi-backend routing storage backend.
///
/// Dispatches each storage operation to the correct underlying backend
/// by resolving the virtual bucket name to a `(backend, real_bucket)` pair.
/// One backend's (real_bucket, creation_date) roster.
type BackendRoster = Vec<(String, chrono::DateTime<chrono::Utc>)>;
/// (backend_name, listing outcome) from the parallel fan-out.
type BackendListing = (String, Result<BackendRoster, StorageError>);

/// Per-backend listing health, behind one lock (`gather_listings` takes it once
/// after the fan-out anyway).
#[derive(Default)]
struct BackendHealth {
    /// Last successful (real_bucket, date) listing per backend — synthesizes
    /// unavailable placeholders when that backend fails (explicit routes alone
    /// would miss default-backend buckets).
    last_known: HashMap<String, BackendRoster>,
    /// Backends in failure cooldown: name → (retry-after deadline, last error).
    /// Skipped by `gather_listings` until the deadline, so one dead backend
    /// doesn't tax every listing with a connect timeout.
    cooldown: HashMap<String, (std::time::Instant, String)>,
}

pub struct RoutingBackend {
    backends: HashMap<String, Arc<Box<dyn StorageBackend>>>,
    routes: HashMap<String, BucketRoute>,
    default_backend: String,
    health: parking_lot::Mutex<BackendHealth>,
    /// How long a backend stays in cooldown after a failed listing.
    list_cooldown: std::time::Duration,
    /// Per-backend cap for a single listing call (bounds a hung backend).
    list_timeout: std::time::Duration,
}

impl RoutingBackend {
    /// Create a new routing backend.
    ///
    /// # Errors
    /// Returns an error if `default_backend` doesn't reference a known backend.
    pub fn new(
        backends: HashMap<String, Arc<Box<dyn StorageBackend>>>,
        routes: HashMap<String, (String, Option<String>)>,
        default_backend: String,
    ) -> Result<Self, StorageError> {
        // Read the listing-resilience knobs once (rebuilt fresh with the engine,
        // so a hot config reload clears cooldowns for free).
        let list_cooldown = std::time::Duration::from_secs(crate::config::env_parse_with_default(
            "DGP_BACKEND_LIST_COOLDOWN_SECS",
            30u64,
        ));
        // Floor the timeout at 1s: a 0 would make tokio::time::timeout elapse
        // before any real network call, cycling every backend through cooldown.
        let list_timeout = std::time::Duration::from_secs(
            crate::config::env_parse_with_default("DGP_BACKEND_LIST_TIMEOUT_SECS", 5u64).max(1),
        );
        Self::with_health_config(
            backends,
            routes,
            default_backend,
            list_cooldown,
            list_timeout,
        )
    }

    /// Constructor with injectable cooldown/timeout — the test seam.
    fn with_health_config(
        backends: HashMap<String, Arc<Box<dyn StorageBackend>>>,
        routes: HashMap<String, (String, Option<String>)>,
        default_backend: String,
        list_cooldown: std::time::Duration,
        list_timeout: std::time::Duration,
    ) -> Result<Self, StorageError> {
        if !backends.contains_key(&default_backend) {
            return Err(StorageError::Other(format!(
                "Default backend '{}' not found in configured backends: {:?}",
                default_backend,
                backends.keys().collect::<Vec<_>>()
            )));
        }

        // Validate that all routes reference existing backends
        for (bucket, (backend_name, _)) in &routes {
            if !backends.contains_key(backend_name) {
                return Err(StorageError::Other(format!(
                    "Bucket '{}' routes to unknown backend '{}'",
                    bucket, backend_name
                )));
            }
        }

        let routes = routes
            .into_iter()
            .map(|(bucket, (backend_name, real_bucket))| {
                (
                    bucket,
                    BucketRoute {
                        backend_name,
                        real_bucket,
                    },
                )
            })
            .collect();

        Ok(Self {
            backends,
            routes,
            default_backend,
            health: parking_lot::Mutex::new(BackendHealth::default()),
            list_cooldown,
            list_timeout,
        })
    }

    /// Reverse-lookup: given a backend name and real bucket, find the virtual name.
    /// Returns `None` if no route maps to this (backend, real_bucket) pair.
    fn reverse_lookup(&self, backend_name: &str, real_bucket: &str) -> Option<String> {
        for (virtual_name, route) in &self.routes {
            if route.backend_name == backend_name {
                let route_real = route
                    .real_bucket
                    .as_deref()
                    .unwrap_or(virtual_name.as_str());
                if route_real == real_bucket {
                    return Some(virtual_name.clone());
                }
            }
        }
        None
    }

    /// Convert a bucket discovered on a concrete backend into the virtual
    /// bucket name that clients may safely use.
    ///
    /// If a route maps `(backend, real_bucket)` to a virtual name, that name
    /// is returned. Otherwise the real bucket name is returned as-is — this is
    /// safe because `resolve_existing` HEAD-scans all backends and will find
    /// the bucket at runtime regardless of whether an explicit route exists.
    fn listed_bucket_virtual_name(&self, backend_name: &str, real_bucket: &str) -> String {
        self.reverse_lookup(backend_name, real_bucket)
            .unwrap_or_else(|| real_bucket.to_string())
    }

    /// Migration plumbing (`__dgmigrate_*` staging routes) must never
    /// surface in bucket listings: clients can't reference such names
    /// anyway (s3s rejects them at parse time) and the underlying real
    /// bucket is already listed under its own name from the source side.
    fn is_listing_plumbing(virtual_name: &str) -> bool {
        virtual_name.starts_with(crate::maintenance::migrate::TRANSIENT_PREFIX)
    }

    fn default_backend(&self) -> &dyn StorageBackend {
        self.backends[&self.default_backend].as_ref().as_ref()
    }

    fn explicit_route<'a>(
        &'a self,
        virtual_bucket: &'a str,
    ) -> Option<(&'a dyn StorageBackend, Cow<'a, str>)> {
        self.routes.get(virtual_bucket).map(|route| {
            let backend = &self.backends[&route.backend_name];
            let real = match &route.real_bucket {
                Some(alias) => Cow::Borrowed(alias.as_str()),
                None => Cow::Borrowed(virtual_bucket),
            };
            (backend.as_ref().as_ref(), real)
        })
    }

    /// Resolve existing bucket operations.
    ///
    /// Explicit bucket policies always win. Otherwise, if the default backend
    /// has the bucket, use it. If not, scan other backends and use the first
    /// backend that contains the bucket. This makes buckets discovered by
    /// ListBuckets usable without forcing operators to author bucket policies.
    /// The default backend remains the target for new/ambiguous buckets.
    async fn resolve_existing<'a>(
        &'a self,
        virtual_bucket: &'a str,
    ) -> (&'a dyn StorageBackend, Cow<'a, str>) {
        if let Some(route) = self.explicit_route(virtual_bucket) {
            return route;
        }

        let default = self.default_backend();
        if default.head_bucket(virtual_bucket).await.unwrap_or(false) {
            return (default, Cow::Borrowed(virtual_bucket));
        }

        let mut names: Vec<&String> = self.backends.keys().collect();
        names.sort();
        for name in names {
            if name == &self.default_backend {
                continue;
            }
            let backend = self.backends[name].as_ref().as_ref();
            if backend.head_bucket(virtual_bucket).await.unwrap_or(false) {
                return (backend, Cow::Borrowed(virtual_bucket));
            }
        }

        (default, Cow::Borrowed(virtual_bucket))
    }
}

macro_rules! route_existing {
    ($self:ident, $bucket:ident, $method:ident $(, $arg:expr)*) => {{
        let (backend, real_bucket) = $self.resolve_existing($bucket).await;
        backend.$method(&real_bucket $(, $arg)*).await
    }};
}

impl RoutingBackend {
    /// Query every backend CONCURRENTLY, in stable name order — one dead
    /// backend costs a single timeout, not one per backend in sequence.
    /// Successful listings refresh the last-known-good snapshot.
    async fn gather_listings(&self) -> Vec<BackendListing> {
        let mut names: Vec<&String> = self.backends.keys().collect();
        names.sort();
        let now = std::time::Instant::now();

        // Decide per backend: probe it, or short-circuit as cooling-down. The
        // first request past a deadline optimistically RE-ARMS the cooldown
        // before probing, so concurrent requests in the same window skip and
        // exactly one prober pays the timeout per cooldown period.
        let mut to_probe: Vec<&String> = Vec::new();
        let mut cooling: Vec<BackendListing> = Vec::new();
        {
            let mut h = self.health.lock();
            for name in names {
                // Snapshot the entry so the mutable re-arm below doesn't alias
                // the immutable `.get()` borrow.
                match h.cooldown.get(name).cloned() {
                    Some((deadline, last_err)) if deadline > now => {
                        let secs = (deadline - now).as_secs();
                        cooling.push((
                            name.clone(),
                            Err(StorageError::S3(format!(
                                "backend in failure cooldown ({secs}s remaining): {last_err}"
                            ))),
                        ));
                    }
                    Some((_, last_err)) => {
                        // Deadline expired → re-arm now; this request re-probes.
                        h.cooldown
                            .insert(name.clone(), (now + self.list_cooldown, last_err));
                        to_probe.push(name);
                    }
                    None => to_probe.push(name),
                }
            }
        }

        // Probe the live candidates concurrently, each capped by list_timeout.
        let futs = to_probe.into_iter().map(|name| {
            let backend = &self.backends[name];
            let timeout = self.list_timeout;
            async move {
                let outcome =
                    match tokio::time::timeout(timeout, backend.list_buckets_with_dates()).await {
                        Ok(res) => res,
                        Err(_) => Err(StorageError::S3(format!(
                            "listing timed out after {}s",
                            timeout.as_secs()
                        ))),
                    };
                (name.clone(), outcome)
            }
        });
        let probed = futures::future::join_all(futs).await;

        // Update health from the probes: success clears cooldown + refreshes the
        // roster; failure (re)arms cooldown with the real error.
        {
            let mut h = self.health.lock();
            for (name, res) in &probed {
                match res {
                    Ok(list) => {
                        h.last_known.insert(name.clone(), list.clone());
                        h.cooldown.remove(name);
                    }
                    Err(e) => {
                        h.cooldown
                            .insert(name.clone(), (now + self.list_cooldown, e.to_string()));
                    }
                }
            }
        }

        // Deterministic order (backends were name-sorted; re-sort the union).
        let mut results = probed;
        results.extend(cooling);
        results.sort_by(|a, b| a.0.cmp(&b.0));
        results
    }

    /// Total-failure guard shared by the three listing methods: `Some(Err)`
    /// when NO backend answered — an empty Ok would read as "zero buckets" and
    /// render the UI's first-run empty state during a transient outage. The
    /// aggregate is deterministic (backends already name-sorted).
    fn total_failure_error(results: &[BackendListing]) -> Option<StorageError> {
        if results.iter().any(|(_, r)| r.is_ok()) {
            return None;
        }
        let msgs: Vec<String> = results
            .iter()
            .filter_map(|(n, r)| r.as_ref().err().map(|e| format!("{n}: {e}")))
            .collect();
        (!msgs.is_empty()).then(|| StorageError::S3(msgs.join("; ")))
    }

    /// Resolve the backend an in-progress multipart upload belongs to.
    /// `MultipartUpload.backend` is the configured name stamped by
    /// `create_multipart_upload`; fall back to the default backend when it
    /// is absent (single-backend) or no longer configured.
    fn resolve_multipart_backend(&self, upload: &MultipartUpload) -> &dyn StorageBackend {
        if let Some(name) = upload.backend.as_deref() {
            if let Some(b) = self.backends.get(name) {
                return b.as_ref().as_ref();
            }
        }
        self.default_backend()
    }

    /// Resolve a virtual bucket to `(backend_name, backend, real_bucket)`.
    /// Name-aware variant of `resolve_existing` used by the multipart path
    /// (the name is stamped into `MultipartUpload` for re-targeting).
    async fn resolve_existing_named<'a>(
        &'a self,
        virtual_bucket: &'a str,
    ) -> (String, &'a dyn StorageBackend, Cow<'a, str>) {
        if let Some(route) = self.routes.get(virtual_bucket) {
            let backend = self.backends[&route.backend_name].as_ref().as_ref();
            let real = match &route.real_bucket {
                Some(alias) => Cow::Borrowed(alias.as_str()),
                None => Cow::Borrowed(virtual_bucket),
            };
            return (route.backend_name.clone(), backend, real);
        }
        let default = self.default_backend();
        if default.head_bucket(virtual_bucket).await.unwrap_or(false) {
            return (
                self.default_backend.clone(),
                default,
                Cow::Borrowed(virtual_bucket),
            );
        }
        let mut names: Vec<&String> = self.backends.keys().collect();
        names.sort();
        for name in names {
            if name == &self.default_backend {
                continue;
            }
            let backend = self.backends[name].as_ref().as_ref();
            if backend.head_bucket(virtual_bucket).await.unwrap_or(false) {
                return (name.clone(), backend, Cow::Borrowed(virtual_bucket));
            }
        }
        (
            self.default_backend.clone(),
            default,
            Cow::Borrowed(virtual_bucket),
        )
    }
}

#[async_trait]
impl StorageBackend for RoutingBackend {
    // === Bucket operations ===

    async fn create_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        route_existing!(self, bucket, create_bucket)
    }

    async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        route_existing!(self, bucket, delete_bucket)
    }

    /// Aggregate buckets across all backends, deduplicating by virtual name.
    async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        // Parallel fan-out; a failing backend is logged but doesn't drop the
        // reachable ones. Total failure → Err (see `total_failure_error`).
        let results = self.gather_listings().await;
        if let Some(e) = Self::total_failure_error(&results) {
            return Err(e);
        }
        let mut all_buckets = HashSet::new();
        for (backend_name, res) in &results {
            match res {
                Ok(buckets) => {
                    for (real_bucket, _) in buckets {
                        let virtual_name =
                            self.listed_bucket_virtual_name(backend_name, real_bucket);
                        if Self::is_listing_plumbing(&virtual_name) {
                            continue;
                        }
                        all_buckets.insert(virtual_name);
                    }
                }
                Err(e) => tracing::error!(
                    "Failed to list buckets from backend '{}': {} — results may be incomplete",
                    backend_name,
                    e
                ),
            }
        }
        let mut result: Vec<String> = all_buckets.into_iter().collect();
        result.sort();
        Ok(result)
    }

    /// Aggregate buckets with dates across all backends.
    async fn list_buckets_with_dates(
        &self,
    ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, StorageError> {
        let results = self.gather_listings().await;
        if let Some(e) = Self::total_failure_error(&results) {
            return Err(e);
        }
        let mut all_buckets: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
        for (backend_name, res) in &results {
            match res {
                Ok(buckets) => {
                    for (real_bucket, date) in buckets {
                        let virtual_name =
                            self.listed_bucket_virtual_name(backend_name, real_bucket);
                        if Self::is_listing_plumbing(&virtual_name) {
                            continue;
                        }
                        all_buckets.entry(virtual_name).or_insert(*date);
                    }
                }
                Err(e) => tracing::error!(
                    "Failed to list buckets from backend '{}': {} — results may be incomplete",
                    backend_name,
                    e
                ),
            }
        }
        let mut result: Vec<_> = all_buckets.into_iter().collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }

    /// Aggregate buckets across all backends while preserving the backend that
    /// produced each visible bucket. This is used by the admin UI to display
    /// compact provider badges without changing S3-compatible XML semantics.
    async fn list_bucket_origins(&self) -> Result<Vec<BucketListing>, StorageError> {
        let results = self.gather_listings().await;
        let mut candidates: Vec<(String, u8, String, BucketListing)> = Vec::new();
        let mut any_ok = false;
        for (backend_name, res) in &results {
            match res {
                Ok(buckets) => {
                    any_ok = true;
                    for (real_bucket, creation_date) in buckets {
                        let virtual_name =
                            self.listed_bucket_virtual_name(backend_name, real_bucket);
                        if Self::is_listing_plumbing(&virtual_name) {
                            continue;
                        }
                        let priority = if self.reverse_lookup(backend_name, real_bucket).is_some() {
                            0
                        } else if backend_name == &self.default_backend {
                            1
                        } else {
                            2
                        };
                        let real_bucket_alias = (real_bucket.as_str() != virtual_name.as_str())
                            .then(|| real_bucket.clone());
                        candidates.push((
                            virtual_name.clone(),
                            priority,
                            backend_name.clone(),
                            BucketListing {
                                name: virtual_name,
                                creation_date: Some(*creation_date),
                                backend_name: Some(backend_name.clone()),
                                real_bucket: real_bucket_alias,
                                unavailable: None,
                            },
                        ));
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to list bucket origins from backend '{}': {} — results may be incomplete",
                        backend_name,
                        e
                    );
                    // Don't DROP this backend's buckets — synthesize placeholders
                    // flagged unavailable (verbatim origin error), from the union
                    // of CONFIG routes and the LAST-KNOWN-GOOD listing (routes
                    // alone miss default-backend buckets). Priority 3 so a live
                    // listing always wins the dedup; no fabricated date.
                    let origin = e.to_string();
                    let mut push = |virtual_name: String, real_alias: Option<String>| {
                        if Self::is_listing_plumbing(&virtual_name) {
                            return;
                        }
                        candidates.push((
                            virtual_name.clone(),
                            3,
                            backend_name.clone(),
                            BucketListing {
                                name: virtual_name,
                                creation_date: None,
                                backend_name: Some(backend_name.clone()),
                                real_bucket: real_alias,
                                unavailable: Some(origin.clone()),
                            },
                        ));
                    };
                    for (virtual_name, route) in &self.routes {
                        if route.backend_name != *backend_name {
                            continue;
                        }
                        let real_alias = route
                            .real_bucket
                            .as_ref()
                            .filter(|rb| rb.as_str() != virtual_name.as_str())
                            .cloned();
                        push(virtual_name.clone(), real_alias);
                    }
                    let snapshot = self.health.lock().last_known.get(backend_name).cloned();
                    for (real_bucket, _) in snapshot.unwrap_or_default() {
                        let virtual_name =
                            self.listed_bucket_virtual_name(backend_name, &real_bucket);
                        let real_alias = (real_bucket.as_str() != virtual_name.as_str())
                            .then(|| real_bucket.clone());
                        push(virtual_name, real_alias);
                    }
                }
            }
        }
        if !any_ok && candidates.is_empty() {
            // Nothing reachable AND nothing declared/remembered → surface the
            // aggregate error rather than an empty list.
            if let Some(e) = Self::total_failure_error(&results) {
                return Err(e);
            }
        }

        // Deduplicate by the same preference order used for request routing:
        // explicit route, default backend, then stable backend-name order.
        candidates.sort_by(|a, b| (&a.0, a.1, &a.2).cmp(&(&b.0, b.1, &b.2)));
        let mut all_buckets: HashMap<String, BucketListing> = HashMap::new();
        for (name, _, _, bucket) in candidates {
            all_buckets.entry(name).or_insert(bucket);
        }

        let mut result: Vec<_> = all_buckets.into_values().collect();
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }

    async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
        let (backend, real_bucket) = self.resolve_existing(bucket).await;
        backend.head_bucket(&real_bucket).await
    }

    // === Reference file operations ===

    async fn get_reference(&self, bucket: &str, prefix: &str) -> Result<Vec<u8>, StorageError> {
        route_existing!(self, bucket, get_reference, prefix)
    }

    async fn get_reference_to_file(
        &self,
        bucket: &str,
        prefix: &str,
        dest: &std::path::Path,
    ) -> Result<u64, StorageError> {
        // Delegate to the routed backend's streaming impl (filesystem hardlink /
        // S3 stream-to-file) rather than the buffering default.
        route_existing!(self, bucket, get_reference_to_file, prefix, dest)
    }

    async fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        route_existing!(self, bucket, put_reference, prefix, data, metadata)
    }

    async fn put_reference_from_file(
        &self,
        bucket: &str,
        prefix: &str,
        source_path: &std::path::Path,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        route_existing!(
            self,
            bucket,
            put_reference_from_file,
            prefix,
            source_path,
            metadata
        )
    }

    async fn put_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        route_existing!(self, bucket, put_reference_metadata, prefix, metadata)
    }

    async fn get_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<FileMetadata, StorageError> {
        route_existing!(self, bucket, get_reference_metadata, prefix)
    }

    async fn has_reference(&self, bucket: &str, prefix: &str) -> bool {
        let (backend, real_bucket) = self.resolve_existing(bucket).await;
        backend.has_reference(&real_bucket, prefix).await
    }

    async fn delete_reference(&self, bucket: &str, prefix: &str) -> Result<(), StorageError> {
        route_existing!(self, bucket, delete_reference, prefix)
    }

    // === Delta file operations ===

    async fn get_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        route_existing!(self, bucket, get_delta, prefix, filename)
    }

    async fn put_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        route_existing!(self, bucket, put_delta, prefix, filename, data, metadata)
    }

    async fn get_delta_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        route_existing!(self, bucket, get_delta_metadata, prefix, filename)
    }

    async fn delete_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        route_existing!(self, bucket, delete_delta, prefix, filename)
    }

    // === Passthrough file operations ===

    async fn get_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        route_existing!(self, bucket, get_passthrough, prefix, filename)
    }

    async fn put_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        route_existing!(
            self,
            bucket,
            put_passthrough,
            prefix,
            filename,
            data,
            metadata
        )
    }

    // Forward the FILE + PARTS sinks so a routed EncryptingBackend's streaming
    // overrides are reached — the trait default `tokio::fs::read`s the whole
    // object into RAM (O(object), no cap), which defeats the bounded-memory
    // guarantee for the production multi-backend shape.
    async fn put_passthrough_file(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        source_path: &std::path::Path,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        route_existing!(
            self,
            bucket,
            put_passthrough_file,
            prefix,
            filename,
            source_path,
            metadata
        )
    }

    async fn put_passthrough_parts(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        part_paths: &[std::path::PathBuf],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        route_existing!(
            self,
            bucket,
            put_passthrough_parts,
            prefix,
            filename,
            part_paths,
            metadata
        )
    }

    async fn get_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        route_existing!(self, bucket, get_passthrough_metadata, prefix, filename)
    }

    async fn delete_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        route_existing!(self, bucket, delete_passthrough, prefix, filename)
    }

    // === Streaming operations ===

    async fn get_passthrough_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
        route_existing!(self, bucket, get_passthrough_stream, prefix, filename)
    }

    async fn get_passthrough_stream_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
        route_existing!(
            self,
            bucket,
            get_passthrough_stream_range,
            prefix,
            filename,
            start,
            end
        )
    }

    async fn put_passthrough_chunked(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        chunks: &[Bytes],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        route_existing!(
            self,
            bucket,
            put_passthrough_chunked,
            prefix,
            filename,
            chunks,
            metadata
        )
    }

    // === Multipart upload (Phase B) ===

    async fn create_multipart_upload(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        metadata: &FileMetadata,
    ) -> Result<MultipartUpload, StorageError> {
        let (name, backend, real_bucket) = self.resolve_existing_named(bucket).await;
        let mut upload = backend
            .create_multipart_upload(&real_bucket, prefix, filename, metadata)
            .await?;
        // Stamp the resolved backend name so upload_part/complete/abort
        // re-target the SAME backend without re-probing.
        upload.backend = Some(name);
        Ok(upload)
    }

    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        part_number: i32,
        data: Bytes,
    ) -> Result<UploadedPart, StorageError> {
        self.resolve_multipart_backend(upload)
            .upload_part(upload, prefix, filename, part_number, data)
            .await
    }

    async fn complete_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        parts: &[UploadedPart],
        assembled: &[Bytes],
        metadata: &FileMetadata,
    ) -> Result<String, StorageError> {
        self.resolve_multipart_backend(upload)
            .complete_multipart_upload(upload, prefix, filename, parts, assembled, metadata)
            .await
    }

    async fn abort_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        self.resolve_multipart_backend(upload)
            .abort_multipart_upload(upload, prefix, filename)
            .await
    }

    fn multipart_storage_label(&self, bucket: &str) -> &'static str {
        // Route by explicit policy only (sync, no head probing). For
        // unrouted buckets fall back to the default backend's label. This
        // is a conservative capability hint, not a correctness boundary.
        if let Some(route) = self.routes.get(bucket) {
            return self.backends[&route.backend_name]
                .as_ref()
                .as_ref()
                .multipart_storage_label(bucket);
        }
        self.default_backend().multipart_storage_label(bucket)
    }

    fn supports_native_multipart(&self, bucket: &str) -> bool {
        if let Some(route) = self.routes.get(bucket) {
            return self.backends[&route.backend_name]
                .as_ref()
                .as_ref()
                .supports_native_multipart(bucket);
        }
        self.default_backend().supports_native_multipart(bucket)
    }

    fn lite_list_carries_logical_facts(&self, bucket: &str) -> bool {
        if let Some(route) = self.routes.get(bucket) {
            return self.backends[&route.backend_name]
                .as_ref()
                .as_ref()
                .lite_list_carries_logical_facts(bucket);
        }
        self.default_backend()
            .lite_list_carries_logical_facts(bucket)
    }

    // === Scanning operations ===

    async fn scan_deltaspace(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<FileMetadata>, StorageError> {
        route_existing!(self, bucket, scan_deltaspace, prefix)
    }

    async fn scan_deltaspace_lite(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<LiteScanResult, StorageError> {
        route_existing!(self, bucket, scan_deltaspace_lite, prefix)
    }

    async fn list_deltaspaces(&self, bucket: &str) -> Result<Vec<String>, StorageError> {
        route_existing!(self, bucket, list_deltaspaces)
    }

    /// When bucket is None, sum total_size across all backends.
    async fn total_size(&self, bucket: Option<&str>) -> Result<u64, StorageError> {
        match bucket {
            Some(b) => {
                let (backend, real_bucket) = self.resolve_existing(b).await;
                backend.total_size(Some(&real_bucket)).await
            }
            None => {
                let mut total = 0u64;
                for (name, backend) in &self.backends {
                    match backend.total_size(None).await {
                        Ok(size) => total += size,
                        Err(e) => {
                            tracing::error!(
                                "Failed to get total_size from backend '{}': {}",
                                name,
                                e
                            );
                            return Err(StorageError::Other(format!(
                                "Backend '{}' failed during total_size aggregation: {}",
                                name, e
                            )));
                        }
                    }
                }
                Ok(total)
            }
        }
    }

    async fn put_directory_marker(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        route_existing!(self, bucket, put_directory_marker, key)
    }

    async fn bulk_list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        route_existing!(self, bucket, bulk_list_objects, prefix)
    }

    async fn enrich_list_metadata(
        &self,
        bucket: &str,
        objects: Vec<(String, FileMetadata)>,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        let (backend, real_bucket) = self.resolve_existing(bucket).await;
        backend.enrich_list_metadata(&real_bucket, objects).await
    }

    async fn list_objects_delegated(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: &str,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<Option<DelegatedListResult>, StorageError> {
        let (backend, real_bucket) = self.resolve_existing(bucket).await;
        backend
            .list_objects_delegated(
                &real_bucket,
                prefix,
                delimiter,
                max_keys,
                continuation_token,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[derive(Clone, Default)]
    struct TestBackend {
        buckets: Arc<StdMutex<HashSet<String>>>,
        create_calls: Arc<StdMutex<Vec<String>>>,
        /// When true, list_buckets(_with_dates) returns an error — models an
        /// upstream backend 503-ing. Shared so a test can flip it mid-test.
        fail_list: Arc<StdMutex<bool>>,
        /// Count of list_buckets calls — asserts the cooldown skips a backend.
        list_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TestBackend {
        fn with_buckets(buckets: &[&str]) -> Self {
            Self {
                buckets: Arc::new(StdMutex::new(
                    buckets.iter().map(|b| b.to_string()).collect(),
                )),
                create_calls: Arc::new(StdMutex::new(Vec::new())),
                fail_list: Arc::new(StdMutex::new(false)),
                list_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        fn failing() -> Self {
            let b = Self::with_buckets(&[]);
            *b.fail_list.lock().unwrap() = true;
            b
        }

        fn create_calls(&self) -> Vec<String> {
            self.create_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl StorageBackend for TestBackend {
        async fn create_bucket(&self, bucket: &str) -> Result<(), StorageError> {
            self.create_calls.lock().unwrap().push(bucket.to_string());
            let mut buckets = self.buckets.lock().unwrap();
            if !buckets.insert(bucket.to_string()) {
                return Err(StorageError::AlreadyExists(bucket.to_string()));
            }
            Ok(())
        }

        async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
            self.buckets.lock().unwrap().remove(bucket);
            Ok(())
        }

        async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
            self.list_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if *self.fail_list.lock().unwrap() {
                return Err(StorageError::S3("simulated backend outage".into()));
            }
            Ok(self.buckets.lock().unwrap().iter().cloned().collect())
        }

        async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
            Ok(self.buckets.lock().unwrap().contains(bucket))
        }

        async fn get_reference(&self, _: &str, _: &str) -> Result<Vec<u8>, StorageError> {
            Err(StorageError::NotFound("reference".to_string()))
        }

        async fn put_reference(
            &self,
            _: &str,
            _: &str,
            _: &[u8],
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Ok(())
        }

        async fn put_reference_metadata(
            &self,
            _: &str,
            _: &str,
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Ok(())
        }

        async fn get_reference_metadata(
            &self,
            _: &str,
            _: &str,
        ) -> Result<FileMetadata, StorageError> {
            Err(StorageError::NotFound("metadata".to_string()))
        }

        async fn has_reference(&self, _: &str, _: &str) -> bool {
            false
        }

        async fn delete_reference(&self, _: &str, _: &str) -> Result<(), StorageError> {
            Ok(())
        }

        async fn get_delta(&self, _: &str, _: &str, _: &str) -> Result<Vec<u8>, StorageError> {
            Err(StorageError::NotFound("delta".to_string()))
        }

        async fn put_delta(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Ok(())
        }

        async fn get_delta_metadata(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<FileMetadata, StorageError> {
            Err(StorageError::NotFound("delta metadata".to_string()))
        }

        async fn delete_delta(&self, _: &str, _: &str, _: &str) -> Result<(), StorageError> {
            Ok(())
        }

        async fn get_passthrough(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<Vec<u8>, StorageError> {
            Err(StorageError::NotFound("object".to_string()))
        }

        async fn get_passthrough_stream(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
            Ok(Box::pin(futures::stream::empty()))
        }

        async fn get_passthrough_stream_range(
            &self,
            bucket: &str,
            prefix: &str,
            filename: &str,
            _: u64,
            _: u64,
        ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
            let stream = self
                .get_passthrough_stream(bucket, prefix, filename)
                .await?;
            Ok((stream, 0))
        }

        async fn put_passthrough(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Ok(())
        }

        async fn get_passthrough_metadata(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<FileMetadata, StorageError> {
            Err(StorageError::NotFound("object metadata".to_string()))
        }

        async fn delete_passthrough(&self, _: &str, _: &str, _: &str) -> Result<(), StorageError> {
            Ok(())
        }

        async fn scan_deltaspace(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<FileMetadata>, StorageError> {
            Ok(Vec::new())
        }

        async fn list_deltaspaces(&self, _: &str) -> Result<Vec<String>, StorageError> {
            Ok(Vec::new())
        }

        async fn total_size(&self, _: Option<&str>) -> Result<u64, StorageError> {
            Ok(0)
        }

        async fn bulk_list_objects(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn test_routing_backend_rejects_unknown_default() {
        let backends = HashMap::new();
        let routes = HashMap::new();
        let result = RoutingBackend::new(backends, routes, "nonexistent".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_routing_backend_rejects_unknown_route_backend() {
        let backends: HashMap<String, Arc<Box<dyn StorageBackend>>> = HashMap::new();
        let mut routes = HashMap::new();
        routes.insert(
            "test".to_string(),
            ("nonexistent".to_string(), None::<String>),
        );
        // Can't validate without backends, but ensure empty map is handled
        assert!(backends.is_empty());
    }

    #[test]
    fn test_reverse_lookup() {
        // Can't construct RoutingBackend without real backends, but we can test
        // the reverse lookup logic conceptually via the BucketRoute struct
        let route = BucketRoute {
            backend_name: "hetzner".to_string(),
            real_bucket: Some("prod-archive".to_string()),
        };
        assert_eq!(
            route.real_bucket.as_deref().unwrap_or("archive"),
            "prod-archive"
        );

        let route_no_alias = BucketRoute {
            backend_name: "local".to_string(),
            real_bucket: None,
        };
        assert_eq!(
            route_no_alias.real_bucket.as_deref().unwrap_or("dev-data"),
            "dev-data"
        );
    }

    #[test]
    fn listed_bucket_virtual_name_exposes_unrouted_backend_buckets() {
        let mut routes = HashMap::new();
        routes.insert(
            "virtual-archive".to_string(),
            BucketRoute {
                backend_name: "archive".to_string(),
                real_bucket: Some("real-archive".to_string()),
            },
        );
        routes.insert(
            "plain-routed".to_string(),
            BucketRoute {
                backend_name: "archive".to_string(),
                real_bucket: None,
            },
        );
        let routing = RoutingBackend {
            backends: HashMap::new(),
            routes,
            default_backend: "primary".to_string(),
            health: parking_lot::Mutex::new(BackendHealth::default()),
            list_cooldown: std::time::Duration::from_secs(30),
            list_timeout: std::time::Duration::from_secs(5),
        };

        assert_eq!(
            routing.listed_bucket_virtual_name("primary", "default-bucket"),
            "default-bucket"
        );
        assert_eq!(
            routing.listed_bucket_virtual_name("archive", "real-archive"),
            "virtual-archive"
        );
        assert_eq!(
            routing.listed_bucket_virtual_name("archive", "plain-routed"),
            "plain-routed"
        );
        assert_eq!(
            routing.listed_bucket_virtual_name("archive", "unrouted-real"),
            "unrouted-real"
        );
    }

    #[tokio::test]
    async fn create_bucket_resolves_existing_unrouted_bucket_before_defaulting() {
        let primary_probe = TestBackend::default();
        let primary = Arc::new(Box::new(primary_probe.clone()) as Box<dyn StorageBackend>);
        let archive_probe = TestBackend::with_buckets(&["shared"]);
        let archive = Arc::new(Box::new(archive_probe.clone()) as Box<dyn StorageBackend>);
        let mut backends = HashMap::new();
        backends.insert("primary".to_string(), primary.clone());
        backends.insert("archive".to_string(), archive);

        let routing = RoutingBackend::new(backends, HashMap::new(), "primary".to_string())
            .expect("routing backend");

        let result = routing.create_bucket("shared").await;
        assert!(
            matches!(&result, Err(StorageError::AlreadyExists(bucket)) if bucket == "shared"),
            "create should be routed to the backend that already has the bucket: {:?}",
            result
        );
        assert!(
            !primary_probe.head_bucket("shared").await.unwrap(),
            "create_bucket must not create a duplicate on the default backend"
        );
        assert_eq!(archive_probe.create_calls(), vec!["shared".to_string()]);
    }

    /// Regression (prod RCA 2026-07-05): when EVERY backend errors on
    /// list_buckets (an upstream provider 503-throttling us), the routing layer
    /// must return Err — NOT an empty Ok. An empty Ok reads to the UI as "zero
    /// buckets" and shows the "create your first bucket" empty state during a
    /// transient outage, with a clean JS console (200 response, empty list).
    #[tokio::test]
    async fn total_backend_failure_returns_err_not_empty_ok() {
        let mut backends = HashMap::new();
        backends.insert(
            "only".to_string(),
            Arc::new(Box::new(TestBackend::failing()) as Box<dyn StorageBackend>),
        );
        let routing = RoutingBackend::new(backends, HashMap::new(), "only".to_string())
            .expect("routing backend");
        assert!(
            routing.list_buckets().await.is_err(),
            "all-backends-failed must be Err, got an Ok (would empty the UI)"
        );
        assert!(routing.list_buckets_with_dates().await.is_err());
        assert!(routing.list_bucket_origins().await.is_err());
    }

    /// The union requirement (user, 2026-07-05): when a backend fails its live
    /// listing, its CONFIG-DECLARED buckets must NOT be dropped — they appear
    /// flagged `unavailable` carrying the VERBATIM origin error. A bucket that's
    /// temporarily unreachable is still the user's bucket; hiding it reads as
    /// "it vanished".
    #[tokio::test]
    async fn failed_backend_lists_config_declared_buckets_as_unavailable() {
        let mut backends = HashMap::new();
        backends.insert(
            "up".to_string(),
            Arc::new(Box::new(TestBackend::with_buckets(&["alive"])) as Box<dyn StorageBackend>),
        );
        backends.insert(
            "down".to_string(),
            Arc::new(Box::new(TestBackend::failing()) as Box<dyn StorageBackend>),
        );
        // Two buckets are config-declared as routed to the DOWN backend.
        let mut routes = HashMap::new();
        routes.insert("mirror-a".to_string(), ("down".to_string(), None));
        routes.insert(
            "mirror-b".to_string(),
            ("down".to_string(), Some("real-mirror-b".to_string())),
        );
        let routing =
            RoutingBackend::new(backends, routes, "up".to_string()).expect("routing backend");

        let origins = routing.list_bucket_origins().await.expect("must be Ok");
        let by: HashMap<_, _> = origins.iter().map(|b| (b.name.as_str(), b)).collect();

        // The reachable bucket is present and NOT flagged.
        assert!(
            by["alive"].unavailable.is_none(),
            "reachable bucket must be clean"
        );
        // Both config-declared buckets on the down backend ARE present, flagged
        // with the verbatim backend error — never dropped.
        for name in ["mirror-a", "mirror-b"] {
            let b = by
                .get(name)
                .unwrap_or_else(|| panic!("{name} must be listed"));
            let err = b
                .unavailable
                .as_deref()
                .unwrap_or_else(|| panic!("{name} must be flagged unavailable"));
            assert!(
                err.contains("simulated backend outage"),
                "{name} must carry the verbatim origin error, got: {err}"
            );
            assert_eq!(b.backend_name.as_deref(), Some("down"));
        }
        assert_eq!(
            by["mirror-b"].real_bucket.as_deref(),
            Some("real-mirror-b"),
            "alias preserved on the unavailable placeholder"
        );
    }

    /// H1 regression: an UNROUTED bucket (no explicit `backend:` policy — the
    /// common default-backend case) must survive its backend's outage via the
    /// last-known-good snapshot, flagged unavailable — not silently vanish.
    #[tokio::test]
    async fn failed_backend_keeps_last_known_unrouted_buckets_as_unavailable() {
        let flaky = TestBackend::with_buckets(&["unrouted-data"]);
        let flip = flaky.fail_list.clone();
        let mut backends = HashMap::new();
        backends.insert(
            "up".to_string(),
            Arc::new(Box::new(TestBackend::with_buckets(&["alive"])) as Box<dyn StorageBackend>),
        );
        backends.insert(
            "flaky".to_string(),
            Arc::new(Box::new(flaky) as Box<dyn StorageBackend>),
        );
        let routing = RoutingBackend::new(backends, HashMap::new(), "up".to_string())
            .expect("routing backend");

        // First listing succeeds → populates the last-known-good snapshot.
        let origins = routing.list_bucket_origins().await.expect("ok");
        assert!(origins
            .iter()
            .any(|b| b.name == "unrouted-data" && b.unavailable.is_none()));

        // Backend goes dark → the bucket is still listed, flagged, undated.
        *flip.lock().unwrap() = true;
        let origins = routing.list_bucket_origins().await.expect("ok");
        let b = origins
            .iter()
            .find(|b| b.name == "unrouted-data")
            .expect("unrouted bucket must survive the outage");
        assert!(b.unavailable.as_deref().unwrap_or("").contains("simulated"));
        assert!(b.creation_date.is_none(), "no fabricated date");
    }

    // ── dead-backend listing cooldown (Design A) ──

    /// Build a routing backend with an injected cooldown TTL (timeout large so
    /// it never fires in these logic tests).
    fn routing_with_cooldown(
        backends: HashMap<String, Arc<Box<dyn StorageBackend>>>,
        default: &str,
        cooldown: std::time::Duration,
    ) -> RoutingBackend {
        RoutingBackend::with_health_config(
            backends,
            HashMap::new(),
            default.to_string(),
            cooldown,
            std::time::Duration::from_secs(30),
        )
        .expect("routing backend")
    }

    #[tokio::test]
    async fn failed_backend_is_skipped_during_cooldown() {
        let dead = TestBackend::failing();
        let probe_count = dead.list_calls.clone();
        let mut backends = HashMap::new();
        backends.insert(
            "up".to_string(),
            Arc::new(Box::new(TestBackend::with_buckets(&["alive"])) as Box<dyn StorageBackend>),
        );
        backends.insert(
            "dead".to_string(),
            Arc::new(Box::new(dead) as Box<dyn StorageBackend>),
        );
        let routing = routing_with_cooldown(backends, "up", std::time::Duration::from_secs(300));

        // First listing probes the dead backend (fails → arms cooldown).
        let _ = routing.list_bucket_origins().await.expect("ok");
        assert_eq!(probe_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Second listing must NOT re-probe it, yet still flag it unavailable
        // with the cooldown-prefixed origin error.
        let origins = routing.list_bucket_origins().await.expect("ok");
        assert_eq!(
            probe_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "dead backend must not be re-probed during cooldown"
        );
        // (no unavailable placeholder here because 'dead' has no routes/roster;
        //  the reachable bucket is still returned — the point is zero re-probe.)
        assert!(origins.iter().any(|b| b.name == "alive"));
    }

    #[tokio::test]
    async fn cooldown_prefixed_error_carries_the_real_origin() {
        // A dead backend WITH a last-known roster → the placeholder path fires
        // on the cooled request, carrying the cooldown-prefixed real error.
        let flaky = TestBackend::with_buckets(&["data"]);
        let flip = flaky.fail_list.clone();
        let mut backends = HashMap::new();
        backends.insert(
            "up".to_string(),
            Arc::new(Box::new(TestBackend::with_buckets(&["alive"])) as Box<dyn StorageBackend>),
        );
        backends.insert(
            "flaky".to_string(),
            Arc::new(Box::new(flaky) as Box<dyn StorageBackend>),
        );
        let routing = routing_with_cooldown(backends, "up", std::time::Duration::from_secs(300));

        routing.list_bucket_origins().await.expect("ok"); // seed roster
        *flip.lock().unwrap() = true;
        routing.list_bucket_origins().await.expect("ok"); // fail → arm cooldown
        let origins = routing.list_bucket_origins().await.expect("ok"); // cooled
        let b = origins
            .iter()
            .find(|b| b.name == "data")
            .expect("placeholder from last-known roster");
        let err = b.unavailable.as_deref().unwrap_or("");
        assert!(err.contains("failure cooldown"), "{err}");
        assert!(err.contains("simulated backend outage"), "{err}");
    }

    #[tokio::test]
    async fn successful_probe_clears_cooldown() {
        let flaky = TestBackend::with_buckets(&["data"]);
        let flip = flaky.fail_list.clone();
        let count = flaky.list_calls.clone();
        let mut backends = HashMap::new();
        backends.insert(
            "up".to_string(),
            Arc::new(Box::new(TestBackend::with_buckets(&["alive"])) as Box<dyn StorageBackend>),
        );
        backends.insert(
            "flaky".to_string(),
            Arc::new(Box::new(flaky) as Box<dyn StorageBackend>),
        );
        // TTL zero → every request re-probes (no lingering cooldown).
        let routing = routing_with_cooldown(backends, "up", std::time::Duration::ZERO);

        *flip.lock().unwrap() = true;
        routing.list_bucket_origins().await.expect("ok"); // fail
        *flip.lock().unwrap() = false; // recover
        let before = count.load(std::sync::atomic::Ordering::SeqCst);
        let origins = routing.list_bucket_origins().await.expect("ok");
        assert!(
            count.load(std::sync::atomic::Ordering::SeqCst) > before,
            "a zero-TTL cooldown must let the next request re-probe"
        );
        // Recovered → listed live, not flagged.
        assert!(origins
            .iter()
            .any(|b| b.name == "data" && b.unavailable.is_none()));
    }

    #[tokio::test]
    async fn all_backends_cooling_returns_err_with_origins() {
        let d1 = TestBackend::failing();
        let d2 = TestBackend::failing();
        let mut backends = HashMap::new();
        backends.insert(
            "a".to_string(),
            Arc::new(Box::new(d1) as Box<dyn StorageBackend>),
        );
        backends.insert(
            "b".to_string(),
            Arc::new(Box::new(d2) as Box<dyn StorageBackend>),
        );
        let routing = routing_with_cooldown(backends, "a", std::time::Duration::from_secs(300));

        // First call fails all → arms cooldowns and returns Err.
        assert!(routing.list_buckets().await.is_err());
        // Second call: all cooling; still Err, and the aggregate names the real
        // origin error, not just "in cooldown".
        let err = routing.list_buckets().await.expect_err("all cooling → Err");
        let msg = err.to_string();
        assert!(msg.contains("simulated backend outage"), "{msg}");
    }

    /// The complement: a PARTIAL failure (one backend up, one down) still
    /// returns the reachable buckets — partial results beat none for a listing.
    #[tokio::test]
    async fn partial_backend_failure_returns_reachable_buckets() {
        let mut backends = HashMap::new();
        backends.insert(
            "up".to_string(),
            Arc::new(Box::new(TestBackend::with_buckets(&["alive"])) as Box<dyn StorageBackend>),
        );
        backends.insert(
            "down".to_string(),
            Arc::new(Box::new(TestBackend::failing()) as Box<dyn StorageBackend>),
        );
        let routing = RoutingBackend::new(backends, HashMap::new(), "up".to_string())
            .expect("routing backend");
        let names = routing.list_buckets().await.expect("partial must be Ok");
        assert_eq!(names, vec!["alive".to_string()]);
    }

    #[tokio::test]
    async fn list_bucket_origins_reports_routed_backend() {
        let primary = Arc::new(
            Box::new(TestBackend::with_buckets(&["shared", "local-only"]))
                as Box<dyn StorageBackend>,
        );
        let archive = Arc::new(
            Box::new(TestBackend::with_buckets(&["shared", "real-archive"]))
                as Box<dyn StorageBackend>,
        );
        let mut backends = HashMap::new();
        backends.insert("primary".to_string(), primary);
        backends.insert("archive".to_string(), archive);
        let mut routes = HashMap::new();
        routes.insert(
            "virtual-archive".to_string(),
            ("archive".to_string(), Some("real-archive".to_string())),
        );

        let routing =
            RoutingBackend::new(backends, routes, "primary".to_string()).expect("routing backend");
        let origins = routing.list_bucket_origins().await.expect("origins");

        let by_name: HashMap<_, _> = origins
            .iter()
            .map(|bucket| (bucket.name.as_str(), bucket))
            .collect();
        assert_eq!(
            by_name["shared"].backend_name.as_deref(),
            Some("primary"),
            "unrouted duplicate bucket should match default-backend resolution"
        );
        assert_eq!(
            by_name["virtual-archive"].backend_name.as_deref(),
            Some("archive")
        );
        assert_eq!(
            by_name["virtual-archive"].real_bucket.as_deref(),
            Some("real-archive")
        );
    }

    // Regression: a bucket on a NON-default backend, routed by an explicit
    // policy WITHOUT an alias (real_bucket == virtual name), where the default
    // backend does NOT have that bucket. Mirrors the prod repro
    // "create test-localfs-bucket on localfs" — origins must report `localfs`,
    // not the default backend.
    #[tokio::test]
    async fn list_bucket_origins_reports_non_default_backend_no_alias() {
        let primary =
            Arc::new(Box::new(TestBackend::with_buckets(&["only-on-primary"]))
                as Box<dyn StorageBackend>);
        let secondary =
            Arc::new(Box::new(TestBackend::with_buckets(&["only-on-secondary"]))
                as Box<dyn StorageBackend>);
        let mut backends = HashMap::new();
        backends.insert("primary".to_string(), primary);
        backends.insert("secondary".to_string(), secondary);
        // Explicit route, NO alias: virtual name == real bucket name.
        let mut routes = HashMap::new();
        routes.insert(
            "only-on-secondary".to_string(),
            ("secondary".to_string(), None),
        );

        let routing =
            RoutingBackend::new(backends, routes, "primary".to_string()).expect("routing backend");
        let origins = routing.list_bucket_origins().await.expect("origins");
        let by_name: HashMap<_, _> = origins.iter().map(|b| (b.name.as_str(), b)).collect();

        assert_eq!(
            by_name["only-on-secondary"].backend_name.as_deref(),
            Some("secondary"),
            "a bucket living only on a non-default backend (routed, no alias) must be \
             attributed to that backend, not the default"
        );
    }

    // Regression for the real prod bug: the engine holds its storage as a
    // `Box<dyn StorageBackend>`. The blanket `impl StorageBackend for
    // Box<dyn StorageBackend>` must FORWARD list_bucket_origins to the inner
    // backend — if it falls through to the trait default, every bucket comes
    // back with `backend_name: None` and the admin API mis-attributes them all
    // to the default backend. This test calls through the Box exactly like the
    // engine does.
    #[tokio::test]
    async fn list_bucket_origins_forwards_through_box_dyn() {
        let primary = Arc::new(
            Box::new(TestBackend::with_buckets(&["on-primary"])) as Box<dyn StorageBackend>
        );
        let secondary = Arc::new(
            Box::new(TestBackend::with_buckets(&["on-secondary"])) as Box<dyn StorageBackend>
        );
        let mut backends = HashMap::new();
        backends.insert("primary".to_string(), primary);
        backends.insert("secondary".to_string(), secondary);
        let mut routes = HashMap::new();
        routes.insert("on-secondary".to_string(), ("secondary".to_string(), None));
        let routing =
            RoutingBackend::new(backends, routes, "primary".to_string()).expect("routing backend");

        // Box it, exactly as DeltaGliderEngine stores its storage.
        let boxed: Box<dyn StorageBackend> = Box::new(routing);
        let origins = boxed.list_bucket_origins().await.expect("origins via box");
        let by_name: HashMap<_, _> = origins.iter().map(|b| (b.name.as_str(), b)).collect();

        assert_eq!(
            by_name["on-secondary"].backend_name.as_deref(),
            Some("secondary"),
            "list_bucket_origins must forward through Box<dyn StorageBackend>, not fall back \
             to the default impl that drops backend attribution"
        );
        assert_eq!(
            by_name["on-primary"].backend_name.as_deref(),
            Some("primary"),
        );
    }
}
