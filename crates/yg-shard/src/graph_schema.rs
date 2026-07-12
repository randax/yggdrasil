//! The graph segment's SQL vocabulary: table and column names shared by
//! the writer ([`build_graph_sqlite`](crate)) and the Verb reader
//! (yg-verbs). Both sides assemble their SQL from these constants, so a
//! rename that reaches only one side fails to compile instead of
//! becoming a query over a column that no longer exists.

/// The nodes table.
pub const NODES: &str = "nodes";
/// Node id, Shard-internal form (`file:<path>`, `sym:<path>#<name>`).
pub const NODE_ID: &str = "id";
/// Node kind (`NodeKind::as_str`).
pub const NODE_KIND: &str = "kind";
/// Human name; NULL for nodes whose id says it all.
pub const NODE_NAME: &str = "name";
/// Repo-relative path; NULL for non-file-anchored nodes.
pub const NODE_PATH: &str = "path";
/// Committer date in unix seconds; NULL for everything but Commits.
pub const NODE_COMMITTED_AT: &str = "committed_at";

/// The edges table.
pub const EDGES: &str = "edges";
/// Source node id, Shard-internal form.
pub const EDGE_SRC: &str = "src";
/// Destination node id, Shard-internal form.
pub const EDGE_DST: &str = "dst";
/// Edge kind (`EdgeKind::as_str`).
pub const EDGE_KIND: &str = "kind";
/// How the edge was derived (`Provenance::as_str`).
pub const EDGE_PROVENANCE: &str = "provenance";
/// How sure the deriving pass was, 0.0–1.0.
pub const EDGE_CONFIDENCE: &str = "confidence";
/// Where the edge was witnessed (`<path>:<line>:<col>`); NULL for
/// edges without a site.
pub const EDGE_LOCATION: &str = "location";
