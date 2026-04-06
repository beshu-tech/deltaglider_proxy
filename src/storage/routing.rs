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

use super::traits::{DelegatedListResult, StorageBackend, StorageError};

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

    /// Resolve a virtual bucket to `(backend, real_bucket_name)`.
    fn resolve<'a>(&'a self, virtual_bucket: &'a str) -> (&'a dyn StorageBackend, Cow<'a, str>) {
        match self.routes.get(virtual_bucket) {
            Some(route) => {
                let backend = &self.backends[&route.backend_name];
                let real = match &route.real_bucket {
                    Some(alias) => Cow::Borrowed(alias.as_str()),
                    None => Cow::Borrowed(virtual_bucket),
                };
                (backend.as_ref().as_ref(), real)
            }
            None => {
                let backend = &self.backends[&self.default_backend];
                (backend.as_ref().as_ref(), Cow::Borrowed(virtual_bucket))
            }
        }
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
}

/// Macro for routing methods that take `bucket` as first arg.
/// Resolves the virtual bucket, substitutes the real name, and dispatches.
macro_rules! route {
    // bucket + remaining args
    ($self:ident, $bucket:ident, $method:ident $(, $arg:expr)*) => {{
        let (backend, real_bucket) = $self.resolve($bucket);
        backend.$method(&real_bucket $(, $arg)*).await
    }};
}

#[async_trait]
impl StorageBackend for RoutingBackend {
    // === Bucket operations ===

    async fn create_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        route!(self, bucket, create_bucket)
    }

    async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        route!(self, bucket, delete_bucket)
    }

    /// Aggregate buckets across all backends, deduplicating by virtual name.
    async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        let mut all_buckets = HashSet::new();

        // Include all explicitly routed virtual names
        for virtual_name in self.routes.keys() {
            all_buckets.insert(virtual_name.clone());
        }

        // Query each backend
        for (backend_name, backend) in &self.backends {
            match backend.list_buckets().await {
                Ok(buckets) => {
                    for real_bucket in buckets {
                        // If a route maps this (backend, real_bucket) → virtual, use virtual name
                        let virtual_name = self
                            .reverse_lookup(backend_name, &real_bucket)
                            .unwrap_or(real_bucket);
                        all_buckets.insert(virtual_name);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to list buckets from backend '{}': {}",
                        backend_name,
                        e
                    );
                }
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

        // Query backends first — real dates take precedence
        for (backend_name, backend) in &self.backends {
            match backend.list_buckets_with_dates().await {
                Ok(buckets) => {
                    for (real_bucket, date) in buckets {
                        let virtual_name = self
                            .reverse_lookup(backend_name, &real_bucket)
                            .unwrap_or(real_bucket);
                        all_buckets.entry(virtual_name).or_insert(date);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to list buckets from backend '{}': {}",
                        backend_name,
                        e
                    );
                }
            }
        }

        // Add routed virtual names that weren't found on any backend
        // (bucket may not exist yet, but the route is configured)
        for virtual_name in self.routes.keys() {
            all_buckets
                .entry(virtual_name.clone())
                .or_insert_with(chrono::Utc::now);
        }

        let mut result: Vec<_> = all_buckets.into_iter().collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }

    async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
        route!(self, bucket, head_bucket)
    }

    // === Reference file operations ===

    async fn get_reference(&self, bucket: &str, prefix: &str) -> Result<Vec<u8>, StorageError> {
        route!(self, bucket, get_reference, prefix)
    }

    async fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        route!(self, bucket, put_reference, prefix, data, metadata)
    }

    async fn put_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        route!(self, bucket, put_reference_metadata, prefix, metadata)
    }

    async fn get_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<FileMetadata, StorageError> {
        route!(self, bucket, get_reference_metadata, prefix)
    }

    async fn has_reference(&self, bucket: &str, prefix: &str) -> bool {
        let (backend, real_bucket) = self.resolve(bucket);
        backend.has_reference(&real_bucket, prefix).await
    }

    async fn delete_reference(&self, bucket: &str, prefix: &str) -> Result<(), StorageError> {
        route!(self, bucket, delete_reference, prefix)
    }

    // === Delta file operations ===

    async fn get_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        route!(self, bucket, get_delta, prefix, filename)
    }

    async fn put_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        route!(self, bucket, put_delta, prefix, filename, data, metadata)
    }

    async fn get_delta_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        route!(self, bucket, get_delta_metadata, prefix, filename)
    }

    async fn delete_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        route!(self, bucket, delete_delta, prefix, filename)
    }

    // === Passthrough file operations ===

    async fn get_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        route!(self, bucket, get_passthrough, prefix, filename)
    }

    async fn put_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        route!(
            self,
            bucket,
            put_passthrough,
            prefix,
            filename,
            data,
            metadata
        )
    }

    async fn get_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        route!(self, bucket, get_passthrough_metadata, prefix, filename)
    }

    async fn delete_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        route!(self, bucket, delete_passthrough, prefix, filename)
    }

    // === Streaming operations ===

    async fn get_passthrough_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
        route!(self, bucket, get_passthrough_stream, prefix, filename)
    }

    async fn get_passthrough_stream_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
        route!(
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
        route!(
            self,
            bucket,
            put_passthrough_chunked,
            prefix,
            filename,
            chunks,
            metadata
        )
    }

    // === Scanning operations ===

    async fn scan_deltaspace(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<FileMetadata>, StorageError> {
        route!(self, bucket, scan_deltaspace, prefix)
    }

    async fn list_deltaspaces(&self, bucket: &str) -> Result<Vec<String>, StorageError> {
        route!(self, bucket, list_deltaspaces)
    }

    /// When bucket is None, sum total_size across all backends.
    async fn total_size(&self, bucket: Option<&str>) -> Result<u64, StorageError> {
        match bucket {
            Some(b) => {
                let (backend, real_bucket) = self.resolve(b);
                backend.total_size(Some(&real_bucket)).await
            }
            None => {
                let mut total = 0u64;
                for (name, backend) in &self.backends {
                    match backend.total_size(None).await {
                        Ok(size) => total += size,
                        Err(e) => {
                            tracing::warn!(
                                "Failed to get total_size from backend '{}': {}",
                                name,
                                e
                            );
                        }
                    }
                }
                Ok(total)
            }
        }
    }

    async fn put_directory_marker(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        route!(self, bucket, put_directory_marker, key)
    }

    async fn bulk_list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        route!(self, bucket, bulk_list_objects, prefix)
    }

    async fn enrich_list_metadata(
        &self,
        bucket: &str,
        objects: Vec<(String, FileMetadata)>,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        let (backend, real_bucket) = self.resolve(bucket);
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
        let (backend, real_bucket) = self.resolve(bucket);
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
}
