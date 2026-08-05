// SPDX-License-Identifier: BUSL-1.1

//! DeltaGlider delta-based deduplication engine

mod cache;
mod codec;
mod engine;
mod file_router;
pub mod savings;
pub mod spool;

pub use cache::ReferenceCache;
pub use codec::{CodecError, DeltaCodec};
pub use engine::store::PassthroughMultipartHandle;
pub(crate) use engine::{derive_key_id, interleave_and_paginate};
pub use engine::{
    DeltaGliderEngine, DynEngine, EngineError, ListObjectsPage, ReferenceScan, RetrieveResponse,
    REFERENCE_SCAN_LIMIT,
};
pub use file_router::{CompressionStrategy, FileRouter};
pub use savings::SavingsTotals;
