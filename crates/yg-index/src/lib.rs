//! Tree-sitter syntactic indexing, history ingestion, and worker orchestration.

mod commit;
mod gc;
mod history;
mod pass;
mod resolve;
mod worker;

pub use history::extract_history;
pub use pass::syntactic_pass;
pub use worker::IndexWorker;
