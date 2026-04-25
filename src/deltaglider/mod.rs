//! DeltaGlider delta-based deduplication engine

mod cache;
mod codec;
mod engine;
mod file_router;

pub use cache::ReferenceCache;
pub use codec::{CodecError, DeltaCodec};
pub(crate) use engine::{derive_key_id, interleave_and_paginate};
pub use engine::{DeltaGliderEngine, DynEngine, EngineError, ListObjectsPage, RetrieveResponse};
pub use file_router::{CompressionStrategy, FileRouter};
