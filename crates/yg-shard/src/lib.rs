//! Shard read/write, cache tier, formats.
//!
//! A Shard is the immutable per-repo index artifact (RFC 0001 §6): a
//! graph segment plus a manifest, written to object storage under
//! `shards/<repo-id>/<revision>/` and never mutated afterwards.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context;
use object_store::aws::AmazonS3Builder;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Version of the Shard layout (graph tables + manifest shape). Part of
/// every revision id: bumping it re-indexes the world rather than mixing
/// layouts under one revision.
///
/// It also stands in for the pass implementation: a change to what the
/// syntactic pass extracts must bump it, or already-published revisions
/// keep their old artifacts (re-publishing an existing revision is a
/// no-op by design).
pub const SCHEMA_VERSION: u32 = 1;

/// Name of the M0 indexing pass, as recorded in revision ids, manifests,
/// and the control plane's `provenance_level`. The precise pass (M1)
/// adds its own.
pub const SYNTACTIC_PASS: &str = "syntactic";

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

impl Node {
    /// The File node for a repo-relative path: id `file:<path>`.
    pub fn file(path: &str) -> Self {
        Self {
            id: format!("file:{path}"),
            kind: NodeKind::File,
            name: None,
            path: Some(path.to_string()),
        }
    }

    /// A Symbol declared in `path`: id `sym:<path>#<name>`. `ordinal`
    /// disambiguates same-named declarations in one file (multiple
    /// `func init()`): the first is 1 (no suffix), later ones get `~n`.
    pub fn symbol(path: &str, name: &str, ordinal: u32) -> Self {
        let id = if ordinal <= 1 {
            format!("sym:{path}#{name}")
        } else {
            format!("sym:{path}#{name}~{ordinal}")
        };
        Self {
            id,
            kind: NodeKind::Symbol,
            name: Some(name.to_string()),
            path: Some(path.to_string()),
        }
    }
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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
/// control plane. Commit and pass aren't carried: both entry points
/// fail closed unless the manifest matches the commit the caller asked
/// about and [`SYNTACTIC_PASS`], so the caller already holds them.
#[derive(Debug, Clone)]
pub struct PublishedShard {
    pub revision: String,
    pub manifest_key: String,
    pub node_count: i64,
    pub edge_count: i64,
}

/// The revision id the syntactic pass publishes for a commit.
/// Deterministic on purpose: re-indexing the same commit derives the same
/// revision, which is what makes re-indexing idempotent.
pub fn syntactic_revision(commit: &str) -> String {
    format!("{commit}-{SYNTACTIC_PASS}-v{SCHEMA_VERSION}")
}

/// Object key of a revision's manifest — the one definition of the
/// Shard layout (RFC 0001 §6), shared by the writer, the reader, and
/// anything that needs to plant or inspect a Shard.
pub fn manifest_key(repo_id: i64, revision: &str) -> String {
    format!("shards/{repo_id}/{revision}/manifest.json")
}

/// File name of the graph segment inside a Shard — the key under
/// [`Manifest::segments`] and the last component of
/// [`graph_segment_key`].
pub const GRAPH_SEGMENT_FILE: &str = "graph.sqlite";

/// Object key of a revision's graph segment, beside its manifest.
pub fn graph_segment_key(repo_id: i64, revision: &str) -> String {
    format!("shards/{repo_id}/{revision}/{GRAPH_SEGMENT_FILE}")
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
    /// Build from `YG_S3_*` environment variables, defaulting to the
    /// in-repo dev compose stack (MinIO).
    pub fn from_env() -> Self {
        fn var_or(key: &str, default: &str) -> String {
            std::env::var(key).unwrap_or_else(|_| default.to_string())
        }
        Self {
            endpoint: var_or("YG_S3_ENDPOINT", "http://localhost:9000"),
            bucket: var_or("YG_S3_BUCKET", "yggdrasil"),
            access_key: var_or("YG_S3_ACCESS_KEY", "yggdrasil"),
            secret_key: var_or("YG_S3_SECRET_KEY", "yggdrasil"),
            region: var_or("YG_S3_REGION", "us-east-1"),
        }
    }

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

/// Cheap reachability check that distinguishes "bucket missing or
/// unreachable" from "bucket empty": a delimited list succeeds on an
/// empty bucket but errors when the bucket doesn't exist. Every process
/// that touches Shards probes at boot — [`ObjectStoreConfig::connect`]
/// never goes near the network, so a misconfigured endpoint otherwise
/// boots cleanly and fails every job instead.
pub async fn probe_object_store(store: &dyn ObjectStore) -> anyhow::Result<()> {
    store.list_with_delimiter(None).await?;
    Ok(())
}

/// The Shard already published for `commit` at the current schema
/// version, if any — read back from its manifest, so the returned counts
/// describe the artifact actually in storage, not whatever a fresh run
/// of the pass would produce.
///
/// Syntactic-pass only, like [`write_shard`]: M1's precise pass brings
/// its own entry points rather than a pass parameter guessed at now.
pub async fn published_shard(
    store: &dyn ObjectStore,
    repo_id: i64,
    commit: &str,
) -> anyhow::Result<Option<PublishedShard>> {
    let revision = syntactic_revision(commit);
    let manifest_key = manifest_key(repo_id, &revision);
    let bytes = match store.get(&manifest_key.as_str().into()).await {
        Ok(get) => get
            .bytes()
            .await
            .context("reading the published manifest")?,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(e) => return Err(e).context("checking for an already-published Shard"),
    };
    let manifest: Manifest =
        serde_json::from_slice(&bytes).context("parsing the published manifest")?;
    // Fail closed: the revision id asserts (commit, pass, schema), and a
    // manifest that disagrees — bucket aliasing across deployments, a
    // manual repair gone wrong — must not have its contents recorded as
    // this revision's.
    if manifest.commit != commit
        || manifest.pass != SYNTACTIC_PASS
        || manifest.schema_version != SCHEMA_VERSION
    {
        anyhow::bail!(
            "the published manifest at {manifest_key} does not describe this revision \
             (it says commit {}, pass {}, schema v{}); refusing to trust it",
            manifest.commit,
            manifest.pass,
            manifest.schema_version
        );
    }
    // Same fail-closed stance for shape: write_shard always records the
    // graph segment, so a manifest without it describes an artifact the
    // layout says cannot exist (a half-copied bucket, a manual repair).
    // Trusting it would surface as a missing segment at first read,
    // far from the cause — and the deterministic revision plus
    // create-only puts mean nothing would ever repair it.
    if !manifest.segments.contains_key(GRAPH_SEGMENT_FILE) {
        anyhow::bail!(
            "the published manifest at {manifest_key} records no {GRAPH_SEGMENT_FILE} segment; \
             refusing to trust it"
        );
    }
    Ok(Some(PublishedShard {
        revision,
        manifest_key,
        node_count: manifest.counts.nodes,
        edge_count: manifest.counts.edges,
    }))
}

/// Write a syntactic-pass Shard for `commit` of repo `repo_id`: the
/// graph segment, then the manifest. The manifest goes last — its
/// presence marks a complete Shard, so a write that dies half-way leaves
/// garbage, never a torn artifact. Published Shards are immutable: every
/// put is create-only (If-None-Match), so even a stale lease holder
/// racing a fresh one can never overwrite published objects, and an
/// already-published revision is returned as-is.
pub async fn write_shard(
    store: &dyn ObjectStore,
    repo_id: i64,
    commit: &str,
    graph: Graph,
) -> anyhow::Result<PublishedShard> {
    let revision = syntactic_revision(commit);
    let manifest_key = manifest_key(repo_id, &revision);
    let graph_key = object_store::path::Path::from(graph_segment_key(repo_id, &revision));
    let local_counts = Counts {
        nodes: graph.nodes.len() as i64,
        edges: graph.edges.len() as i64,
    };

    // Build and digest together off the runtime threads: hashing a large
    // artifact is as blocking as building it.
    let (graph_bytes, local_segment) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<u8>, Segment)> {
            let bytes = build_graph_sqlite(&graph)?;
            let segment = Segment {
                sha256: hex::encode(Sha256::digest(&bytes)),
                bytes: bytes.len() as u64,
            };
            Ok((bytes, segment))
        })
        .await
        .context("graph segment build task panicked")??;
    let create_only = PutOptions {
        mode: PutMode::Create,
        ..Default::default()
    };
    let (segment, counts) = match store
        .put_opts(
            &graph_key,
            PutPayload::from(graph_bytes),
            create_only.clone(),
        )
        .await
    {
        Ok(_) => (local_segment, local_counts),
        // Another publisher beat us to the segment. When its manifest
        // landed too, the published Shard is the whole answer — done,
        // without re-reading the artifact. Otherwise the manifest is
        // ours to write, and it must describe what storage actually
        // holds — a racing worker on a different build (mixed images
        // mid-deploy) may have written subtly different bytes — so
        // digest AND counts come from the stored artifact.
        Err(object_store::Error::AlreadyExists { .. }) => {
            if let Some(published) = published_shard(store, repo_id, commit).await? {
                return Ok(published);
            }
            let stored = store
                .get(&graph_key)
                .await
                .context("reading the segment another publisher just wrote")?
                .bytes()
                .await
                .context("reading the segment another publisher just wrote")?;
            tokio::task::spawn_blocking(move || -> anyhow::Result<(Segment, Counts)> {
                let segment = Segment {
                    sha256: hex::encode(Sha256::digest(&stored)),
                    bytes: stored.len() as u64,
                };
                let counts = read_graph_counts(&stored)
                    .context("counting the segment another publisher just wrote")?;
                Ok((segment, counts))
            })
            .await
            .context("segment inspection task panicked")??
        }
        Err(e) => return Err(e).context("uploading the graph segment"),
    };

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        commit: commit.to_string(),
        pass: SYNTACTIC_PASS.to_string(),
        counts,
        segments: BTreeMap::from([(GRAPH_SEGMENT_FILE.to_string(), segment)]),
    };
    match store
        .put_opts(
            &manifest_key.as_str().into(),
            PutPayload::from(serde_json::to_vec_pretty(&manifest)?),
            create_only,
        )
        .await
    {
        Ok(_) => {}
        // A racing publisher committed the manifest first: defer to the
        // published artifact.
        Err(object_store::Error::AlreadyExists { .. }) => {
            return published_shard(store, repo_id, commit)
                .await?
                .context("a manifest that just existed has vanished");
        }
        Err(e) => return Err(e).context("uploading the manifest"),
    }

    Ok(PublishedShard {
        revision,
        manifest_key,
        node_count: counts.nodes,
        edge_count: counts.edges,
    })
}

/// Node/edge counts of a serialized graph segment, read back from its
/// bytes — used when another publisher's artifact, not ours, is the one
/// in storage.
fn read_graph_counts(bytes: &[u8]) -> anyhow::Result<Counts> {
    let dir = tempfile::tempdir().context("creating a scratch dir to inspect a graph segment")?;
    let path = dir.path().join("graph.sqlite");
    std::fs::write(&path, bytes).context("staging the graph segment for inspection")?;
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let counts = conn.query_row(
        "SELECT (SELECT count(*) FROM nodes), (SELECT count(*) FROM edges)",
        [],
        |row| {
            Ok(Counts {
                nodes: row.get(0)?,
                edges: row.get(1)?,
            })
        },
    )?;
    Ok(counts)
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
