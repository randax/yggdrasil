//! Tree-sitter pass, SCIP ingestion, extractors, sandbox driver.
//!
//! M0 ships the syntactic pass (RFC 0001 §4, ADR 0002): tree-sitter over
//! a synced checkout, Go grammar for Symbols and DEFINES edges, every
//! other file a File node. The precise SCIP pass arrives with M1.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use object_store::ObjectStore;
use yg_control::ControlPlane;
use yg_shard::{Edge, EdgeKind, Graph, Node, NodeKind, Provenance};

/// How long a worker may hold an index job before a crashed run becomes
/// claimable again. The syntactic pass targets minutes (RFC 0001 §4).
const INDEX_LEASE: Duration = Duration::from_secs(15 * 60);

/// An indexing worker: drains the index queue, running the syntactic
/// pass over synced checkouts and publishing Shards.
pub struct IndexWorker {
    control: ControlPlane,
    store: Arc<dyn ObjectStore>,
    git_cache: PathBuf,
}

impl IndexWorker {
    pub fn new(
        control: ControlPlane,
        store: Arc<dyn ObjectStore>,
        git_cache: impl Into<PathBuf>,
    ) -> Self {
        Self {
            control,
            store,
            git_cache: git_cache.into(),
        }
    }

    /// Claim and run one due index job. Returns whether there was work.
    /// A failed run is recorded (with backoff) rather than returned as an
    /// error — `Err` means the control plane itself is unreachable.
    pub async fn run_once(&self) -> anyhow::Result<bool> {
        let Some(job) = self.control.claim_due_index(INDEX_LEASE).await? else {
            return Ok(false);
        };
        match self.index(job.repo_id, &job.commit).await {
            Ok(shard) => {
                let applied = self
                    .control
                    .complete_index(
                        &job,
                        yg_control::ShardRecord {
                            revision: &shard.revision,
                            manifest_key: &shard.manifest_key,
                            commit_sha: &shard.commit,
                            provenance_level: "syntactic",
                            node_count: shard.node_count,
                            edge_count: shard.edge_count,
                        },
                    )
                    .await?;
                if applied {
                    tracing::info!(slug = %job.slug, revision = %shard.revision, "indexed");
                } else {
                    tracing::warn!(slug = %job.slug, "lease lapsed mid-index; result discarded");
                }
            }
            Err(e) => {
                let error = format!("{e:#}");
                if self.control.fail_index(&job, &error).await? {
                    tracing::warn!(slug = %job.slug, attempt = job.attempts + 1, error, "index failed");
                } else {
                    tracing::warn!(slug = %job.slug, "lease lapsed mid-index; failure discarded");
                }
            }
        }
        Ok(true)
    }

    /// Run the syntactic pass over `commit` of the repo's cached mirror
    /// and publish the resulting Shard.
    async fn index(&self, repo_id: i64, commit: &str) -> anyhow::Result<yg_shard::PublishedShard> {
        let mirror = self.git_cache.join(format!("{repo_id}.git"));
        let checkout = tempfile::tempdir().context("creating a scratch checkout dir")?;
        let graph = {
            let mirror = mirror.clone();
            let commit = commit.to_string();
            let dest = checkout.path().to_path_buf();
            tokio::task::spawn_blocking(move || -> anyhow::Result<Graph> {
                extract_tree(&mirror, &commit, &dest)?;
                syntactic_pass(&dest)
            })
            .await
            .context("syntactic pass task panicked")??
        };
        yg_shard::write_shard(self.store.as_ref(), repo_id, commit, graph).await
    }
}

/// Materialize `commit`'s tree from a bare mirror into `dest`, without
/// touching the mirror: `git archive` piped through `tar`.
fn extract_tree(mirror: &Path, commit: &str, dest: &Path) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    let mut archive = Command::new("git")
        .arg("-C")
        .arg(mirror)
        .args(["archive", commit])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git archive (is git installed on this worker?)")?;
    let unpack = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(dest)
        .stdin(Stdio::from(
            archive.stdout.take().expect("stdout was piped above"),
        ))
        .stderr(Stdio::piped())
        .output()
        .context("running tar (is tar installed on this worker?)")?;
    let archive = archive
        .wait_with_output()
        .context("waiting for git archive")?;
    if !archive.status.success() {
        anyhow::bail!(
            "git archive {commit} failed: {}",
            String::from_utf8_lossy(&archive.stderr).trim()
        );
    }
    if !unpack.status.success() {
        anyhow::bail!(
            "unpacking the checkout failed: {}",
            String::from_utf8_lossy(&unpack.stderr).trim()
        );
    }
    Ok(())
}

/// The syntactic pass: walk a materialized checkout and build its graph
/// segment. Every file becomes a File node; Go files additionally yield
/// Symbols and DEFINES edges via tree-sitter (ADR 0002).
pub fn syntactic_pass(root: &Path) -> anyhow::Result<Graph> {
    let mut graph = Graph::default();
    let mut paths = Vec::new();
    collect_files(root, root, &mut paths)?;
    // Walk order must not depend on the filesystem: the graph segment is
    // checksummed, so identical trees should yield identical artifacts.
    paths.sort_by(|a, b| a.path.cmp(&b.path));
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .context("loading the Go grammar")?;
    for FileEntry { path, is_symlink } in paths {
        let file_id = format!("file:{path}");
        graph.nodes.push(Node {
            id: file_id.clone(),
            kind: NodeKind::File,
            name: None,
            path: Some(path.clone()),
        });
        // Symlinks stay content-unread: their target can point anywhere,
        // including outside the checkout.
        if path.ends_with(".go") && !is_symlink {
            let source = std::fs::read(root.join(&path))
                .with_context(|| format!("reading {path} from the checkout"))?;
            extract_go_symbols(&mut parser, &path, &file_id, &source, &mut graph);
        }
    }
    Ok(graph)
}

/// Parse one Go file and append its Symbols and DEFINES edges.
fn extract_go_symbols(
    parser: &mut tree_sitter::Parser,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) {
    let Some(tree) = parser.parse(source, None) else {
        // tree-sitter only gives up on timeouts/cancellation, neither of
        // which we set; treat "no tree" as "no symbols" rather than
        // failing the whole pass over one file.
        tracing::warn!(path, "tree-sitter produced no tree; skipping symbols");
        return;
    };
    // Duplicate names (multiple `func init()`, redeclarations mid-edit)
    // must still mint unique node ids — the graph segment keys on them.
    let mut id_uses: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut cursor = tree.root_node().walk();
    for declaration in tree.root_node().children(&mut cursor) {
        // CONTEXT.md's Symbol: function, method, type, constant. Each
        // top-level Go declaration of those kinds names one or more.
        let names: Vec<String> = match declaration.kind() {
            "function_declaration" => field_text(declaration, "name", source)
                .map(str::to_string)
                .into_iter()
                .collect(),
            // Methods are receiver-qualified (Widget.Render): two types'
            // same-named methods are different Symbols.
            "method_declaration" => field_text(declaration, "name", source)
                .map(|name| match receiver_type_name(declaration, source) {
                    Some(receiver) => format!("{receiver}.{name}"),
                    None => name.to_string(),
                })
                .into_iter()
                .collect(),
            // One declaration can hold many specs: type ( A …; B … ),
            // const ( X = 1; Y = 2 ) — and one const spec many names.
            "type_declaration" | "const_declaration" => {
                let mut cursor = declaration.walk();
                declaration
                    .children(&mut cursor)
                    .filter(|spec| matches!(spec.kind(), "type_spec" | "type_alias" | "const_spec"))
                    .flat_map(|spec| {
                        let mut cursor = spec.walk();
                        spec.children_by_field_name("name", &mut cursor)
                            .filter_map(|n| n.utf8_text(source).ok().map(str::to_string))
                            .collect::<Vec<_>>()
                    })
                    .collect()
            }
            _ => continue,
        };
        for name in names {
            let base_id = format!("sym:{path}#{name}");
            let uses = id_uses.entry(base_id.clone()).or_insert(0);
            *uses += 1;
            let symbol_id = if *uses == 1 {
                base_id
            } else {
                format!("{base_id}~{uses}")
            };
            graph.nodes.push(Node {
                id: symbol_id.clone(),
                kind: NodeKind::Symbol,
                name: Some(name),
                path: Some(path.to_string()),
            });
            graph.edges.push(Edge {
                src: file_id.to_string(),
                dst: symbol_id,
                kind: EdgeKind::Defines,
                provenance: Provenance::Syntactic,
                // The declaration is right there in the parse tree; what
                // is syntactic about it is the pass, not any guesswork.
                confidence: 1.0,
            });
        }
    }
}

/// Text of a named field on a node, when present and valid UTF-8.
fn field_text<'a>(node: tree_sitter::Node<'_>, field: &str, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name(field)
        .and_then(|n| n.utf8_text(source).ok())
}

/// The bare type name a method's receiver refers to: the first type
/// identifier inside `(w *Widget)`, however the receiver is spelled
/// (pointer, generic, parenthesized). The search never leaves the
/// receiver subtree, so a receiver without one (mid-edit code) yields
/// None rather than a type stolen from elsewhere in the file.
fn receiver_type_name<'a>(method: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    fn first_type_identifier<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
        if node.kind() == "type_identifier" {
            return node.utf8_text(source).ok();
        }
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find_map(|child| first_type_identifier(child, source))
    }
    first_type_identifier(method.child_by_field_name("receiver")?, source)
}

/// One tree entry as the walk found it.
struct FileEntry {
    /// Repo-relative, slash-separated path.
    path: String,
    is_symlink: bool,
}

/// Recursively collect every non-directory tree entry. Symlinks count —
/// they are blobs in the git tree — but are flagged so nothing reads
/// through them.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<FileEntry>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("walking {}", dir.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(root, &entry.path(), out)?;
        } else {
            let path = entry
                .path()
                .strip_prefix(root)
                .expect("walk stays under root")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push(FileEntry {
                path,
                is_symlink: file_type.is_symlink(),
            });
        }
    }
    Ok(())
}
