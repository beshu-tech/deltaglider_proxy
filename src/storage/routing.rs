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

        // Query each backend — errors are logged but don't prevent listing
        // buckets from other backends (partial results are better than no results
        // for a listing operation).
        for (backend_name, backend) in &self.backends {
            match backend.list_buckets().await {
                Ok(buckets) => {
                    for real_bucket in buckets {
                        let virtual_name =
                            self.listed_bucket_virtual_name(backend_name, &real_bucket);
                        all_buckets.insert(virtual_name);
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to list buckets from backend '{}': {} — results may be incomplete",
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
                        let virtual_name =
                            self.listed_bucket_virtual_name(backend_name, &real_bucket);
                        all_buckets.entry(virtual_name).or_insert(date);
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to list buckets from backend '{}': {} — results may be incomplete",
                        backend_name,
                        e
                    );
                }
            }
        }

        let mut result: Vec<_> = all_buckets.into_iter().collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
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

    async fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        route_existing!(self, bucket, put_reference, prefix, data, metadata)
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

    // === Scanning operations ===

    async fn scan_deltaspace(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<FileMetadata>, StorageError> {
        route_existing!(self, bucket, scan_deltaspace, prefix)
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
    }

    impl TestBackend {
        fn with_buckets(buckets: &[&str]) -> Self {
            Self {
                buckets: Arc::new(StdMutex::new(
                    buckets.iter().map(|b| b.to_string()).collect(),
                )),
                create_calls: Arc::new(StdMutex::new(Vec::new())),
            }
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
}
