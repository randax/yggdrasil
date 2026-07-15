use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use yg_shard::{Edge, EdgeKind, Graph, Node, NodeKind, Provenance, SearchDoc};

use crate::resolve::{self, GoFileFacts, SimpleExtractionCtx, SimpleFileFacts, SimpleImport};

mod ecmascript;
mod go;
mod java;
mod python;
mod rust;

/// Cap on the text indexed per file. Oversized content remains searchable by
/// file name and is truncated on a character boundary.
const MAX_BODY_BYTES: usize = 512 * 1024;

type Grammar = fn() -> tree_sitter::Language;
type Extractor = for<'tree> fn(
    tree_sitter::Node<'tree>,
    &str,
    &str,
    &[u8],
    &mut Graph,
) -> Option<ExtractedFacts>;

/// A language's complete syntactic-pass registration. Adding support for a
/// grammar means adding one entry here and its extractor function.
struct LanguagePack {
    extensions: &'static [&'static str],
    grammar: Grammar,
    extractor: Extractor,
    grammar_name: &'static str,
}

struct LoadedLanguagePack {
    pack: &'static LanguagePack,
    parser: tree_sitter::Parser,
}

enum ExtractedFacts {
    Go(GoFileFacts),
    Simple(SimpleFileFacts),
}

impl LanguagePack {
    fn load(&'static self) -> anyhow::Result<LoadedLanguagePack> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&(self.grammar)())
            .with_context(|| format!("loading the {} grammar", self.grammar_name))?;
        Ok(LoadedLanguagePack { parser, pack: self })
    }
}

impl LoadedLanguagePack {
    fn handles(&self, path: &str) -> bool {
        self.pack
            .extensions
            .iter()
            .any(|extension| path.ends_with(extension))
    }

    fn extract(
        &mut self,
        path: &str,
        file_id: &str,
        source: &[u8],
        graph: &mut Graph,
    ) -> Option<ExtractedFacts> {
        let Some(tree) = self.parser.parse(source, None) else {
            tracing::warn!(path, "tree-sitter produced no tree; skipping symbols");
            return None;
        };
        (self.pack.extractor)(tree.root_node(), path, file_id, source, graph)
    }
}

const LANGUAGE_PACKS: &[LanguagePack] = &[
    LanguagePack {
        extensions: &[".go"],
        grammar: || tree_sitter_go::LANGUAGE.into(),
        extractor: go::extract_go,
        grammar_name: "Go",
    },
    LanguagePack {
        extensions: &[".ts", ".mts", ".cts"],
        grammar: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        extractor: ecmascript::extract_ecmascript,
        grammar_name: "TypeScript",
    },
    LanguagePack {
        extensions: &[".tsx", ".jsx"],
        grammar: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        extractor: ecmascript::extract_ecmascript,
        grammar_name: "TSX",
    },
    LanguagePack {
        extensions: &[".js", ".mjs", ".cjs"],
        grammar: || tree_sitter_javascript::LANGUAGE.into(),
        extractor: ecmascript::extract_ecmascript,
        grammar_name: "JavaScript",
    },
    LanguagePack {
        extensions: &[".py"],
        grammar: || tree_sitter_python::LANGUAGE.into(),
        extractor: python::extract_python,
        grammar_name: "Python",
    },
    LanguagePack {
        extensions: &[".rs"],
        grammar: || tree_sitter_rust::LANGUAGE.into(),
        extractor: rust::extract_rust,
        grammar_name: "Rust",
    },
    LanguagePack {
        extensions: &[".java"],
        grammar: || tree_sitter_java::LANGUAGE.into(),
        extractor: java::extract_java,
        grammar_name: "Java",
    },
];

/// The syntactic pass: walk a materialized checkout and build its graph
/// segment. Every file becomes a File node; Go files additionally yield
/// Symbols and DEFINES edges via tree-sitter (ADR 0002) plus heuristic
/// CALLS / IMPORTS / EXTENDS / IMPLEMENTS edges (ADR 0006).
///
/// Phase 1 parses one file at a time, mints its Symbols, and distills
/// the parse tree into compact `GoFileFacts` — tree and source are
/// released before the next file parses, so memory scales with facts
/// (names and positions), never with a monorepo's worth of parse trees.
/// Phase 2 resolves the facts repo-wide; it cannot run until every file
/// is parsed.
pub fn syntactic_pass(root: &Path) -> anyhow::Result<(Graph, Vec<SearchDoc>)> {
    let mut graph = Graph::default();
    let mut paths = Vec::new();
    collect_files(root, root, &mut paths)?;
    // Walk order must not depend on the filesystem: the graph segment is
    // checksummed, so identical trees should yield identical artifacts.
    paths.sort_by(|a, b| a.path.cmp(&b.path));
    let mut packs = LANGUAGE_PACKS
        .iter()
        .map(LanguagePack::load)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut files = Vec::new();
    let mut simple_files = Vec::new();
    let mut modules = Vec::new();
    // The text of each file, for the full-text segment — valid UTF-8 only
    // (a binary blob is searchable by name alone), keyed by repo-relative
    // path so the Symbol/File documents can be assembled once the graph is
    // built.
    let mut file_text: HashMap<String, String> = HashMap::new();
    for FileEntry { path, is_symlink } in paths {
        let file = Node::file(&path);
        let file_id = file.id.clone();
        graph.nodes.push(file);
        // Symlinks stay content-unread: their target can point anywhere,
        // including outside the checkout.
        if is_symlink {
            continue;
        }
        // Read once: the bytes feed both the Go parse and the full-text
        // body. Other passes read only Go and go.mod, but the segment
        // indexes every file's text (code and markdown alike).
        let bytes = std::fs::read(root.join(&path))
            .with_context(|| format!("reading {path} from the checkout"))?;
        if let Some(pack) = packs.iter_mut().find(|pack| pack.handles(&path)) {
            match pack.extract(&path, &file_id, &bytes, &mut graph) {
                Some(ExtractedFacts::Go(facts)) => files.push(facts),
                Some(ExtractedFacts::Simple(facts)) => simple_files.push(facts),
                None => {}
            }
        } else if path == "go.mod" || path.ends_with("/go.mod") {
            // Lossy, never fail: synced repos are arbitrary, and a junk
            // file named go.mod must cost module resolution at worst —
            // a failed pass would retry forever, identically.
            if let Some(module) = go_mod_module(&String::from_utf8_lossy(&bytes)) {
                modules.push((package_dir(&path).to_string(), module));
            }
        }
        if let Ok(text) = String::from_utf8(bytes) {
            if text.len() > MAX_BODY_BYTES {
                tracing::debug!(
                    path,
                    bytes = text.len(),
                    cap = MAX_BODY_BYTES,
                    "truncating an oversized file body for the full-text segment"
                );
            }
            file_text.insert(path, cap_body(text));
        }
    }
    resolve::emit_edges(&files, &simple_files, &modules, &mut graph);
    let search_docs = build_search_docs(&graph, &file_text);
    Ok((graph, search_docs))
}

/// The full-text documents for a built graph: one per Symbol (searchable
/// by name) and one per File (searchable by its text), assembled from the
/// graph's nodes and the file text gathered during the walk. Package nodes
/// carry no searchable text and are skipped.
fn build_search_docs(graph: &Graph, file_text: &HashMap<String, String>) -> Vec<SearchDoc> {
    graph
        .nodes
        .iter()
        .filter_map(|node| match node.kind {
            NodeKind::Symbol => Some(SearchDoc {
                node_id: node.id.clone(),
                kind: NodeKind::Symbol,
                name: node.name.clone(),
                path: node.path.clone(),
                content: String::new(),
            }),
            NodeKind::File => {
                let path = node.path.as_deref();
                Some(SearchDoc {
                    node_id: node.id.clone(),
                    kind: NodeKind::File,
                    // A File node carries no name; its file name (the last
                    // path segment) is what a query would spell.
                    name: path.map(|p| file_name(p).to_string()),
                    path: node.path.clone(),
                    content: path
                        .and_then(|p| file_text.get(p))
                        .cloned()
                        .unwrap_or_default(),
                })
            }
            // Package, Commit, and Contributor nodes carry no searchable
            // body; they stay out of the full-text segment.
            NodeKind::Package | NodeKind::Commit | NodeKind::Contributor => None,
        })
        .collect()
}

/// Truncate text to [`MAX_BODY_BYTES`] on a char boundary — search reaches
/// the head of an oversized file, never a torn UTF-8 sequence.
fn cap_body(mut text: String) -> String {
    if text.len() > MAX_BODY_BYTES {
        let mut end = MAX_BODY_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// The last path segment of a repo-relative path — a File's searchable
/// name (`README.md`, `main.go`).
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The module path a go.mod declares, if any.
fn go_mod_module(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        // Trailing line comments are legal on the directive:
        // `module example.com/m // renamed in v2`.
        let line = line.split("//").next().unwrap_or(line);
        line.trim()
            .strip_prefix("module")
            // "module" must be the whole directive word, not a prefix of
            // an identifier ("modules_test" says nothing about a module).
            .filter(|rest| rest.starts_with([' ', '\t']))
            .map(|rest| rest.trim().trim_matches('"').to_string())
            .filter(|module| !module.is_empty())
    })
}

enum SimpleWalk {
    TopLevel,
    Descendants,
}

struct SimpleLanguage<I, C> {
    imports: I,
    walk: SimpleWalk,
    collect: C,
}

fn extract_simple_language<'tree, I, C>(
    root: tree_sitter::Node<'tree>,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
    language: SimpleLanguage<I, C>,
) -> Option<ExtractedFacts>
where
    I: FnOnce(tree_sitter::Node<'tree>, &str, &[u8]) -> Vec<SimpleImport>,
    C: FnMut(tree_sitter::Node<'tree>, &mut SimpleExtractionCtx<'_, '_>),
{
    let SimpleLanguage {
        imports: extract_imports,
        walk,
        mut collect,
    } = language;
    let mut facts = SimpleFileFacts {
        file_id: file_id.to_string(),
        imports: extract_imports(root, path, source),
        calls: Vec::new(),
        declarations: Vec::new(),
    };
    let mut id_uses = HashMap::new();
    let mut context = SimpleExtractionCtx {
        source,
        path,
        file_id,
        graph,
        id_uses: &mut id_uses,
        facts: &mut facts,
    };
    match walk {
        SimpleWalk::TopLevel => {
            let mut cursor = root.walk();
            for declaration in root.children(&mut cursor) {
                collect(declaration, &mut context);
            }
        }
        SimpleWalk::Descendants => {
            let mut cursor = root.walk();
            if cursor.goto_first_child() {
                loop {
                    collect(cursor.node(), &mut context);
                    if cursor.goto_first_child() {
                        continue;
                    }
                    loop {
                        if cursor.goto_next_sibling() {
                            break;
                        }
                        if !cursor.goto_parent() || cursor.node() == root {
                            return Some(ExtractedFacts::Simple(facts));
                        }
                    }
                }
            }
        }
    }
    Some(ExtractedFacts::Simple(facts))
}

fn mint_symbol(
    path: &str,
    file_id: &str,
    name: &str,
    id_uses: &mut HashMap<String, u32>,
    graph: &mut Graph,
) -> String {
    let uses = id_uses.entry(name.to_string()).or_insert(0);
    *uses += 1;
    let symbol = Node::symbol(path, name, *uses);
    let symbol_id = symbol.id.clone();
    graph.nodes.push(symbol);
    graph.edges.push(Edge {
        src: file_id.to_string(),
        dst: symbol_id.clone(),
        kind: EdgeKind::Defines,
        provenance: Provenance::Syntactic,
        confidence: 1.0,
        location: None,
    });
    symbol_id
}

fn simple_expression_name<'a>(
    expression: tree_sitter::Node<'_>,
    source: &'a [u8],
) -> Option<&'a str> {
    match expression.kind() {
        "identifier" | "type_identifier" => expression.utf8_text(source).ok(),
        "scoped_identifier" => field_text(expression, "name", source),
        _ => None,
    }
}

/// The directory holding a repo-relative file path — Go's package
/// boundary for scoping purposes (one package per directory, with rare
/// exceptions like `_test` packages that heuristic resolution accepts
/// conflating).
fn package_dir(path: &str) -> &str {
    path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

/// Every descendant of `node` (excluding itself) with the given kind,
/// in document order — one cursor walk, no per-node allocation.
fn descendants_of_kind<'t>(node: tree_sitter::Node<'t>, kind: &str) -> Vec<tree_sitter::Node<'t>> {
    let mut found = Vec::new();
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return found;
    }
    loop {
        if cursor.node().kind() == kind {
            found.push(cursor.node());
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() || cursor.node() == node {
                return found;
            }
        }
    }
}

/// `node` itself, or its first descendant of the given kind.
fn first_of_kind<'t>(node: tree_sitter::Node<'t>, kind: &str) -> Option<tree_sitter::Node<'t>> {
    if node.kind() == kind {
        return Some(node);
    }
    descendants_of_kind(node, kind).into_iter().next()
}

/// Text of a named field on a node, when present and valid UTF-8.
fn field_text<'a>(node: tree_sitter::Node<'_>, field: &str, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name(field)
        .and_then(|n| n.utf8_text(source).ok())
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
    // Materialize the listing before descending: recursing with the
    // ReadDir handle open holds one directory fd per nesting level, and
    // a deep-enough committed path chain would run the whole process —
    // API listener included — out of file descriptors.
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("walking {}", dir.display()))? {
        let entry = entry?;
        entries.push((entry.path(), entry.file_type()?));
    }
    for (full, file_type) in entries {
        if file_type.is_dir() {
            collect_files(root, &full, out)?;
        } else {
            let relative = full.strip_prefix(root).expect("walk stays under root");
            // A non-UTF-8 name can't round-trip through the graph's
            // string ids — converting it lossily would point the id at a
            // path that doesn't exist (and could collide with a sibling
            // differing only in the invalid bytes). Skip such entries;
            // skipping is deterministic, so identical trees still yield
            // identical artifacts.
            let Some(components) = relative
                .components()
                .map(|c| c.as_os_str().to_str())
                .collect::<Option<Vec<_>>>()
            else {
                tracing::warn!(
                    path = %relative.display(),
                    "skipping a checkout path that is not valid UTF-8"
                );
                continue;
            };
            out.push(FileEntry {
                path: components.join("/"),
                is_symlink: file_type.is_symlink(),
            });
        }
    }
    Ok(())
}
