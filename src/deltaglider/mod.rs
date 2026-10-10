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
pub use engine::store::{MultipartObjectFacts, PassthroughMultipartHandle};
pub(crate) use engine::{derive_key_id, effective_legacy_key_id, interleave_and_paginate};
#[cfg(test)]
pub(crate) use engine::{s3_engine, store_deltas};
pub use engine::{
    ConditionalDelete, DeltaGliderEngine, DynEngine, EngineError, ListObjectsPage, RefWriteProof,
    ReferenceScan, RetrieveResponse, REFERENCE_SCAN_LIMIT,
};
pub use file_router::{CompressionStrategy, FileRouter};
pub use savings::SavingsTotals;
