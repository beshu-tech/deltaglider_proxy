//! DeltaGlider delta-based deduplication engine

mod cache;
mod codec;
mod deltaspace;
mod engine;
mod file_router;

pub use cache::ReferenceCache;
pub use codec::{CodecError, DeltaCodec};
pub use deltaspace::DeltaSpaceManager;
pub use engine::{DeltaGliderEngine, DynEngine};
pub use file_router::{CompressionStrategy, FileRouter};
