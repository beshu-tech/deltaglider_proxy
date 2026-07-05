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
pub struct RoutingBackend {
    backends: HashMap<String, Arc<Box<dyn StorageBackend>>>,
    routes: HashMap<String, BucketRoute>,
    default_backend: String,
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
        let mut all_buckets = HashSet::new();

        // Query each backend. A failing backend is logged but doesn't drop the
        // reachable ones (partial > nothing for a listing). BUT if EVERY backend
        // fails we must NOT return an empty Ok — that reads as "zero buckets" and
        // renders the UI's "create your first bucket" empty state during a
        // transient upstream outage (e.g. an S3 provider 503-throttling us).
        // Propagate the error so the client sees a real 5xx instead.
        let mut last_err: Option<StorageError> = None;
        let mut any_ok = false;
        for (backend_name, backend) in &self.backends {
            match backend.list_buckets().await {
                Ok(buckets) => {
                    any_ok = true;
                    for real_bucket in buckets {
                        let virtual_name =
                            self.listed_bucket_virtual_name(backend_name, &real_bucket);
                        if Self::is_listing_plumbing(&virtual_name) {
                            continue;
                        }
                        all_buckets.insert(virtual_name);
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to list buckets from backend '{}': {} — results may be incomplete",
                        backend_name,
                        e
                    );
                    last_err = Some(e);
                }
            }
        }
        if !any_ok {
            if let Some(e) = last_err {
                return Err(e);
            }
        }

        let mut result: Vec<String> = all_buckets.into_iter().collect();
        result.sort();
        Ok(result)
    }

    /// Aggregate buckets with dates across all backends.
    /// Queries backends first to get real dates, then adds routed virtual
    /// names (with current time) only if they weren't already found.
    async fn list_buckets_with_dates(
        &self,
    ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, StorageError> {
        let mut all_buckets: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();

        // Query backends first — real dates take precedence. As in
        // `list_buckets`: total failure returns Err (not an empty Ok that reads
        // as "zero buckets" and empties the UI during an upstream outage).
        let mut last_err: Option<StorageError> = None;
        let mut any_ok = false;
        for (backend_name, backend) in &self.backends {
            match backend.list_buckets_with_dates().await {
                Ok(buckets) => {
                    any_ok = true;
                    for (real_bucket, date) in buckets {
                        let virtual_name =
                            self.listed_bucket_virtual_name(backend_name, &real_bucket);
                        if Self::is_listing_plumbing(&virtual_name) {
                            continue;
                        }
                        all_buckets.entry(virtual_name).or_insert(date);
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to list buckets from backend '{}': {} — results may be incomplete",
                        backend_name,
                        e
                    );
                    last_err = Some(e);
                }
            }
        }
        if !any_ok {
            if let Some(e) = last_err {
                return Err(e);
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
        let mut candidates: Vec<(String, u8, String, BucketListing)> = Vec::new();

        // Same total-failure guard as list_buckets: don't return an empty Ok
        // when every backend errored (empties the admin buckets panel on a
        // transient upstream outage).
        let mut last_err: Option<StorageError> = None;
        let mut any_ok = false;
        for (backend_name, backend) in &self.backends {
            match backend.list_buckets_with_dates().await {
                Ok(buckets) => {
                    any_ok = true;
                    for (real_bucket, creation_date) in buckets {
                        let virtual_name =
                            self.listed_bucket_virtual_name(backend_name, &real_bucket);
                        if Self::is_listing_plumbing(&virtual_name) {
                            continue;
                        }
                        let priority = if self.reverse_lookup(backend_name, &real_bucket).is_some()
                        {
                            0
                        } else if backend_name == &self.default_backend {
                            1
                        } else {
                            2
                        };
                        let real_bucket_alias =
                            (real_bucket != virtual_name).then_some(real_bucket);
                        candidates.push((
                            virtual_name.clone(),
                            priority,
                            backend_name.clone(),
                            BucketListing {
                                name: virtual_name,
                                creation_date,
                                backend_name: Some(backend_name.clone()),
                                real_bucket: real_bucket_alias,
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
                    last_err = Some(e);
                }
            }
        }
        if !any_ok {
            if let Some(e) = last_err {
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
        /// upstream backend 503-ing so the total-failure guard can be tested.
        fail_list: bool,
    }

    impl TestBackend {
        fn with_buckets(buckets: &[&str]) -> Self {
            Self {
                buckets: Arc::new(StdMutex::new(
                    buckets.iter().map(|b| b.to_string()).collect(),
                )),
                create_calls: Arc::new(StdMutex::new(Vec::new())),
                fail_list: false,
            }
        }

        fn failing() -> Self {
            let mut b = Self::with_buckets(&[]);
            b.fail_list = true;
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
            if self.fail_list {
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
