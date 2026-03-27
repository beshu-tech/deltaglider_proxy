//! DeltaGlider delta-based deduplication engine

mod cache;
mod codec;
mod engine;
mod file_router;

pub use cache::ReferenceCache;
pub use codec::{CodecError, DeltaCodec};
pub(crate) use engine::interleave_and_paginate;
pub use engine::{DeltaGliderEngine, DynEngine, RetrieveResponse};
pub use file_router::{CompressionStrategy, FileRouter};
