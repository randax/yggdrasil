//! Shard read/write, cache tier, formats.
//!
//! A Shard is the immutable per-repo index artifact (RFC 0001 §6): a
//! graph segment plus a manifest, written to object storage under
//! `shards/<repo-id>/<revision>/` and never mutated afterwards.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context;
use object_store::aws::AmazonS3Builder;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Version of the Shard layout (graph tables + manifest shape). Part of
/// every revision id: bumping it re-indexes the world rather than mixing
/// layouts under one revision.
pub const SCHEMA_VERSION: u32 = 1;

/// How a graph edge was derived (ADR 0002, CONTEXT.md). Carried by every
/// edge from day one, even while only syntactic values exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Compiler-grade indexer (SCIP) — arrives with the M1 precise pass.
    Precise,
    /// Heuristic parsing (tree-sitter).
    Syntactic,
    /// Deterministically derived from non-code sources.
    Extracted,
    /// Heuristic guess, reversible.
    Inferred,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Precise => "precise",
            Provenance::Syntactic => "syntactic",
            Provenance::Extracted => "extracted",
            Provenance::Inferred => "inferred",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Symbol,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::File => "File",
            NodeKind::Symbol => "Symbol",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    Defines,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Defines => "DEFINES",
        }
    }
}

/// A graph node. IDs are stable content-derived strings relative to the
/// repo (`file:<path>`, `sym:<path>#<name>`) so Shard rebuilds don't
/// invalidate references held by agents mid-session (RFC 0001 §5).
#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    /// Human name (Symbols); None for nodes whose id says it all.
    pub name: Option<String>,
    /// Repo-relative path for file-anchored nodes.
    pub path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub src: String,
    pub dst: String,
    pub kind: EdgeKind,
    pub provenance: Provenance,
    /// In [0, 1]: how sure the producing pass is (ADR 0002).
    pub confidence: f32,
}

/// The graph segment of one Shard, as produced by an indexing pass.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// The manifest written beside every Shard's segments: enough to verify
/// and describe the artifact without opening it.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    /// The commit this Shard indexes.
    pub commit: String,
    /// Indexing pass that produced it: `syntactic` (M0) or `precise` (M1).
    pub pass: String,
    pub counts: Counts,
    /// Checksums and sizes per segment file, keyed by file name.
    pub segments: BTreeMap<String, Segment>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Counts {
    pub nodes: i64,
    pub edges: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Segment {
    pub sha256: String,
    pub bytes: u64,
}

/// A Shard that made it to object storage, ready to be recorded in the
/// control plane.
#[derive(Debug, Clone)]
pub struct PublishedShard {
    pub revision: String,
    pub manifest_key: String,
    pub commit: String,
    pub node_count: i64,
    pub edge_count: i64,
}

/// The revision id the syntactic pass publishes for a commit.
/// Deterministic on purpose: re-indexing the same commit derives the same
/// revision, which is what makes re-indexing idempotent.
pub fn syntactic_revision(commit: &str) -> String {
    format!("{commit}-syntactic-v{SCHEMA_VERSION}")
}

/// S3-compatible object storage holding the Shards (ADR 0005).
pub struct ObjectStoreConfig {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
}

impl ObjectStoreConfig {
    pub fn connect(&self) -> anyhow::Result<Arc<dyn ObjectStore>> {
        Ok(Arc::new(
            AmazonS3Builder::new()
                .with_endpoint(&self.endpoint)
                .with_bucket_name(&self.bucket)
                .with_access_key_id(&self.access_key)
                .with_secret_access_key(&self.secret_key)
                .with_region(&self.region)
                .with_allow_http(true)
                .build()
                .context("configuring object store client")?,
        ))
    }
}

/// Write a Shard for `commit` of repo `repo_id`: the graph segment, then
/// the manifest. The manifest goes last — its presence marks a complete
/// Shard, so a write that dies half-way leaves garbage, never a torn
/// artifact.
pub async fn write_shard(
    store: &dyn ObjectStore,
    repo_id: i64,
    commit: &str,
    graph: Graph,
) -> anyhow::Result<PublishedShard> {
    let revision = syntactic_revision(commit);
    let prefix = format!("shards/{repo_id}/{revision}");
    let manifest_key = format!("{prefix}/manifest.json");
    let counts = Counts {
        nodes: graph.nodes.len() as i64,
        edges: graph.edges.len() as i64,
    };

    // The manifest marks a complete Shard, and Shards are immutable: if
    // this revision is already published, leave its objects untouched.
    match store.head(&manifest_key.as_str().into()).await {
        Ok(_) => {
            return Ok(PublishedShard {
                revision,
                manifest_key,
                commit: commit.to_string(),
                node_count: counts.nodes,
                edge_count: counts.edges,
            });
        }
        Err(object_store::Error::NotFound { .. }) => {}
        Err(e) => return Err(e).context("checking for an already-published Shard"),
    }

    let graph_bytes = tokio::task::spawn_blocking(move || build_graph_sqlite(&graph))
        .await
        .context("graph segment build task panicked")??;
    let segment = Segment {
        sha256: hex::encode(Sha256::digest(&graph_bytes)),
        bytes: graph_bytes.len() as u64,
    };
    store
        .put(
            &format!("{prefix}/graph.sqlite").into(),
            PutPayload::from(graph_bytes),
        )
        .await
        .context("uploading the graph segment")?;

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        commit: commit.to_string(),
        pass: "syntactic".to_string(),
        counts: Counts {
            nodes: counts.nodes,
            edges: counts.edges,
        },
        segments: BTreeMap::from([("graph.sqlite".to_string(), segment)]),
    };
    store
        .put(
            &manifest_key.as_str().into(),
            PutPayload::from(serde_json::to_vec_pretty(&manifest)?),
        )
        .await
        .context("uploading the manifest")?;

    Ok(PublishedShard {
        revision,
        manifest_key,
        commit: commit.to_string(),
        node_count: counts.nodes,
        edge_count: counts.edges,
    })
}

/// Serialize a graph into a single-file SQLite database (SQLite as
/// *artifact format*, RFC 0001 §6) and return its bytes.
fn build_graph_sqlite(graph: &Graph) -> anyhow::Result<Vec<u8>> {
    // SQLite writes to a real file; a tempdir gives it one without ever
    // racing another build.
    let dir = tempfile::tempdir().context("creating a scratch dir for the graph segment")?;
    let path = dir.path().join("graph.sqlite");
    let conn = rusqlite::Connection::open(&path)?;
    // Write-once artifact: no journal to clean up, nothing to recover.
    conn.pragma_update(None, "journal_mode", "OFF")?;
    conn.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE nodes (
             id   TEXT PRIMARY KEY,
             kind TEXT NOT NULL,
             name TEXT,
             path TEXT
         );
         CREATE TABLE edges (
             src        TEXT NOT NULL,
             dst        TEXT NOT NULL,
             kind       TEXT NOT NULL,
             provenance TEXT NOT NULL,
             confidence REAL NOT NULL
         );",
    )?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
        [SCHEMA_VERSION.to_string()],
    )?;
    {
        let tx = conn.unchecked_transaction()?;
        {
            let mut insert_node =
                tx.prepare("INSERT INTO nodes (id, kind, name, path) VALUES (?1, ?2, ?3, ?4)")?;
            for node in &graph.nodes {
                insert_node.execute(rusqlite::params![
                    node.id,
                    node.kind.as_str(),
                    node.name,
                    node.path
                ])?;
            }
            let mut insert_edge = tx.prepare(
                "INSERT INTO edges (src, dst, kind, provenance, confidence)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for edge in &graph.edges {
                insert_edge.execute(rusqlite::params![
                    edge.src,
                    edge.dst,
                    edge.kind.as_str(),
                    edge.provenance.as_str(),
                    edge.confidence
                ])?;
            }
        }
        tx.commit()?;
    }
    conn.close().map_err(|(_, e)| e)?;
    std::fs::read(&path).context("reading the built graph segment")
}
