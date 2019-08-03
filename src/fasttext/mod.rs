//! Compatibility with fastText embeddings.

mod indexer;
pub use self::indexer::FastTextIndexer;

mod io;
pub use self::io::ReadFastText;