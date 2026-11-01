//! Tree-sitter syntactic indexing, history ingestion, and worker orchestration.

mod commit;
mod gc;
mod history;
mod pass;
mod reconcile;
mod resolve;
mod shard_lifecycle;
mod worker;

pub use history::extract_history;
pub use pass::syntactic_pass;
#[cfg(feature = "contract")]
pub use shard_lifecycle::contract;
pub use worker::IndexWorker;
