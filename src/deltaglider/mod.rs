// SPDX-License-Identifier: BUSL-1.1

//! DeltaGlider delta-based deduplication engine

mod cache;
mod codec;
mod engine;
mod file_router;
pub(crate) mod range_spool;
pub mod savings;
pub mod spool;

pub use cache::ReferenceCache;
pub use codec::{CodecError, DeltaCodec};
pub use engine::conditional::{ConditionalError, ObjectWriteGuard, Precondition};
pub use engine::store::{
    KnownSha256, MultipartObjectFacts, PassthroughMultipartHandle, BLOCKING_HASH_THRESHOLD,
};
pub(crate) use engine::{
    build_raw_backend, effective_legacy_key_id, interleave_and_paginate, CounterPrior,
};
#[cfg(test)]
pub(crate) use engine::{s3_engine, store_deltas};
pub use engine::{
    ConditionalDelete, DeleteHooks, DeleteItem, DeleteOutcome, DeltaGliderEngine, DynEngine,
    EngineError, ListObjectsPage, ListedPageForms, RefWriteProof, ReferenceScan, RetrieveResponse,
    StoredForms, BULK_DELETE_CONCURRENCY, REFERENCE_SCAN_LIMIT,
};
pub use file_router::{
    normalize_extension, CompressionStrategy, FileRouter, DEFAULT_DELTA_EXTENSIONS,
};
pub use savings::SavingsTotals;
