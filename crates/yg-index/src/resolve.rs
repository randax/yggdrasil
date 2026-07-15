//! Distilled syntactic facts and repo-wide heuristic edge resolution.

use std::collections::{BTreeSet, HashMap, HashSet};

use yg_shard::{Edge, EdgeKind, Graph, Node, Provenance};

/// Confidence of a syntactic name resolution with a single candidate —
/// high, but never the 1.0 of a witnessed fact (a DEFINES declaration,
/// an IMPORTS statement): which symbol a name refers to is still a
/// guess. N candidates split it N ways (ADR 0006).
const SYNTACTIC_MATCH: f64 = 0.9;

/// Confidence cap for IMPLEMENTS, which matches method *names* only —
/// signatures are invisible to a reasonable syntactic pass, so even a
/// unique match stays a coin flip (ADR 0006).
const NAME_ONLY_MATCH: f64 = 0.5;

/// What phase 1 distills one Go file into: every name phase 2 must
/// resolve, every site it cites — and nothing else. The parse tree and
/// source are gone by the time this exists.
pub(crate) struct GoFileFacts {
    /// The File node's id, exactly as phase 1 minted it.
    pub(crate) file_id: String,
    /// The file's directory — Go's package boundary for scoping (one
    /// package per directory, with rare exceptions like `_test`
    /// packages that heuristic resolution accepts conflating).
    pub(crate) dir: String,
    /// Imports in declaration order.
    pub(crate) imports: Vec<GoImport>,
    /// Call sites in document order.
    pub(crate) calls: Vec<GoCall>,
    /// Embedded-type references in document order.
    pub(crate) embeds: Vec<GoEmbed>,
    /// Declared functions, `(bare name, symbol id)`, declaration order.
    pub(crate) functions: Vec<(String, String)>,
    /// Declared methods, declaration order.
    pub(crate) methods: Vec<GoMethod>,
    /// Declared types, declaration order.
    pub(crate) types: Vec<GoType>,
    /// Whether the file has a dot import (`import . "…"`), which brings
    /// another package's names into this file's scope unqualified. When
    /// set, an unqualified name with no same-package declaration may be
    /// the dot-imported package's, so repo-wide fallback resolution is
    /// suppressed rather than guessing a far-off same-named repo symbol.
    pub(crate) has_dot_import: bool,
}

/// One import spec: the path it names, where it sits, and the name call
/// sites qualify with — None for blank (`_`) and dot (`.`) imports,
/// which are witnessed imports like any other but introduce no
/// qualifying name.
pub(crate) struct GoImport {
    pub(crate) local_name: Option<String>,
    pub(crate) path: String,
    pub(crate) location: String,
    /// A dot import (`import . "…"`): no qualifying name, but it does
    /// pull the package's exported names into unqualified scope.
    pub(crate) dot: bool,
}

/// One call site, attributed to its enclosing declared symbol.
pub(crate) struct GoCall {
    pub(crate) caller_id: String,
    pub(crate) callee: GoReference,
    pub(crate) location: String,
}

/// One embedded type inside a struct or interface declaration.
pub(crate) struct GoEmbed {
    pub(crate) subject_id: String,
    /// Whether the embedding type is an interface. An interface embeds
    /// only interfaces, so when this is set a target that resolves to a
    /// concrete repo type is a generic constraint (`interface { MyInt }`),
    /// not an embedding, and yields no EXTENDS edge.
    pub(crate) subject_is_interface: bool,
    pub(crate) reference: GoReference,
    pub(crate) location: String,
}

/// A name reference, classified at parse time against its spelling and
/// the file's own imports — everything per-file is settled in phase 1;
/// only the repo-wide resolution is left for phase 2.
pub(crate) enum GoReference {
    /// A bare name: resolved with package scoping.
    Unqualified(String),
    /// `pkg.Name` where pkg names one of the file's imports: resolved
    /// inside that import's package alone.
    Imported { import_path: String, name: String },
    /// `x.Name(…)` whose base names no import: a method reference,
    /// resolved repo-wide by bare name — the receiver's type is
    /// invisible syntactically.
    Method(String),
}

/// One declared method. `receiver` is None when the receiver is
/// unreadable (mid-edit code): such a method still answers method-call
/// resolution by name, but cannot join a type's method set.
pub(crate) struct GoMethod {
    pub(crate) receiver: Option<String>,
    pub(crate) name: String,
    pub(crate) id: String,
}

/// One declared type. `interface` describes an interface's method set;
/// None marks a concrete type.
pub(crate) struct GoType {
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) interface: Option<InterfaceShape>,
}

/// Minimal facts for syntactic language packs whose M0 contract is
/// Symbols, DEFINES, package IMPORTS, and name-based CALLS.
pub(crate) struct SimpleFileFacts {
    pub(crate) file_id: String,
    pub(crate) imports: Vec<SimpleImport>,
    pub(crate) calls: Vec<SimpleCall>,
    pub(crate) declarations: Vec<(String, String)>,
}

pub(crate) struct SimpleImport {
    pub(crate) path: String,
    pub(crate) location: String,
}

pub(crate) struct SimpleCall {
    pub(crate) caller_id: String,
    pub(crate) callee: String,
    pub(crate) location: String,
}

pub(crate) struct SimpleExtractionCtx<'a, 'b> {
    pub(crate) source: &'a [u8],
    pub(crate) path: &'a str,
    pub(crate) file_id: &'a str,
    pub(crate) graph: &'b mut Graph,
    pub(crate) id_uses: &'b mut HashMap<String, u32>,
    pub(crate) facts: &'b mut SimpleFileFacts,
}

/// An interface declaration's shape, for IMPLEMENTS matching.
pub(crate) struct InterfaceShape {
    /// Method names declared directly in the interface body.
    pub(crate) direct_methods: BTreeSet<String>,
    /// Whether `direct_methods` is the interface's *whole* method set.
    /// False when the interface embeds another interface (whose methods
    /// we can't resolve syntactically) or carries a type constraint
    /// (`A | B`, `~int` — a generic constraint, not a regular
    /// interface). An incomplete set must not drive IMPLEMENTS: matching
    /// on a subset of the required methods would emit false edges for
    /// types that satisfy only the directly-named methods. M1's precise
    /// pass resolves embedded method sets; until then, honest silence.
    pub(crate) complete: bool,
}

/// Everything the repo declares, by the name a reference would spell.
/// Candidate lists hold `(symbol id, package dir)` in declaration order
/// — file walk order, then document order within a file — which is
/// deterministic because edge output is checksummed.
#[derive(Default)]
pub(crate) struct SymbolIndex {
    pub(crate) functions: HashMap<String, Vec<(String, String)>>,
    /// Methods by bare name: `x.Render()` can't see its receiver's
    /// type, so every `*.Render` is a candidate.
    pub(crate) methods: HashMap<String, Vec<(String, String)>>,
    /// Types by name, for embedded-type references.
    pub(crate) types: HashMap<String, Vec<(String, String)>>,
    /// Symbol ids of types that are interfaces — so an interface
    /// embedding only keeps EXTENDS targets that are themselves
    /// interfaces (a concrete target is a generic constraint, not an
    /// embed).
    pub(crate) interface_ids: HashSet<String>,
    /// Go files (their node ids) per directory: the file half of
    /// in-repo IMPORTS edges.
    pub(crate) files_by_dir: HashMap<String, Vec<String>>,
    /// Repo directories per import path, resolved through go.mod module
    /// paths once per distinct path — phase 2 asks per call site.
    pub(crate) import_dirs: HashMap<String, Vec<String>>,
}

#[derive(Default)]
pub(crate) struct SimpleSymbolIndex {
    pub(crate) symbols: HashMap<String, Vec<String>>,
}

impl SimpleSymbolIndex {
    fn build(files: &[SimpleFileFacts]) -> Self {
        let mut index = Self::default();
        for file in files {
            for (name, id) in &file.declarations {
                index
                    .symbols
                    .entry(name.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
        index
    }

    fn resolve(&self, name: &str) -> Vec<&str> {
        self.symbols
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(String::as_str)
            .collect()
    }
}

impl SymbolIndex {
    fn build(files: &[GoFileFacts], modules: &[(String, String)]) -> Self {
        let mut index = Self::default();
        for file in files {
            index
                .files_by_dir
                .entry(file.dir.clone())
                .or_default()
                .push(file.file_id.clone());
            for (name, id) in &file.functions {
                index
                    .functions
                    .entry(name.clone())
                    .or_default()
                    .push((id.clone(), file.dir.clone()));
            }
            for method in &file.methods {
                index
                    .methods
                    .entry(method.name.clone())
                    .or_default()
                    .push((method.id.clone(), file.dir.clone()));
            }
            for declared in &file.types {
                index
                    .types
                    .entry(declared.name.clone())
                    .or_default()
                    .push((declared.id.clone(), file.dir.clone()));
                if declared.interface.is_some() {
                    index.interface_ids.insert(declared.id.clone());
                }
            }
            for import in &file.imports {
                if !index.import_dirs.contains_key(&import.path) {
                    let dirs = resolve_import_dirs(modules, &import.path);
                    index.import_dirs.insert(import.path.clone(), dirs);
                }
            }
        }
        index
    }

    /// The repo directories an import path names; empty for external
    /// imports (stdlib, other modules).
    fn dirs_of(&self, import_path: &str) -> &[String] {
        self.import_dirs
            .get(import_path)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Candidates for a call site's callee: functions for plain and
    /// import-qualified names, methods for the rest. `allow_repo_wide`
    /// is false when the file has a dot import (an unqualified name may
    /// be the dot-imported package's, not a far-off repo function's).
    fn resolve_callee(
        &self,
        callee: &GoReference,
        from_dir: &str,
        allow_repo_wide: bool,
    ) -> Vec<&str> {
        match callee {
            GoReference::Unqualified(name) => {
                Self::scoped(&self.functions, name, from_dir, allow_repo_wide)
            }
            GoReference::Imported { import_path, name } => {
                self.in_import(&self.functions, import_path, name)
            }
            GoReference::Method(name) => candidates(&self.methods, name)
                .iter()
                .map(|(id, _)| id.as_str())
                .collect(),
        }
    }

    /// Candidates for an embedded-type reference. Embeddings are never
    /// classified as method references, so that arm resolves to nothing.
    fn resolve_type(
        &self,
        reference: &GoReference,
        from_dir: &str,
        allow_repo_wide: bool,
    ) -> Vec<&str> {
        match reference {
            GoReference::Unqualified(name) => {
                Self::scoped(&self.types, name, from_dir, allow_repo_wide)
            }
            GoReference::Imported { import_path, name } => {
                self.in_import(&self.types, import_path, name)
            }
            GoReference::Method(_) => Vec::new(),
        }
    }

    /// Go scoping for a bare name: candidates in the referencing file's
    /// own package shadow the rest; a name with no local candidate falls
    /// back to repo-wide matching, unless `allow_repo_wide` is false (a
    /// dot import means the name could be external, so don't reach for a
    /// same-named symbol in an unrelated package).
    fn scoped<'i>(
        by_name: &'i HashMap<String, Vec<(String, String)>>,
        name: &str,
        from_dir: &str,
        allow_repo_wide: bool,
    ) -> Vec<&'i str> {
        let all = candidates(by_name, name);
        let same_package: Vec<&str> = all
            .iter()
            .filter(|(_, dir)| dir == from_dir)
            .map(|(id, _)| id.as_str())
            .collect();
        if !same_package.is_empty() {
            same_package
        } else if allow_repo_wide {
            all.iter().map(|(id, _)| id.as_str()).collect()
        } else {
            Vec::new()
        }
    }

    /// A qualified name inside an imported package: candidates from the
    /// import's resolved directories alone. An import the repo doesn't
    /// contain yields nothing.
    fn in_import<'i>(
        &'i self,
        by_name: &'i HashMap<String, Vec<(String, String)>>,
        import_path: &str,
        name: &str,
    ) -> Vec<&'i str> {
        let dirs = self.dirs_of(import_path);
        candidates(by_name, name)
            .iter()
            .filter(|(_, dir)| dirs.contains(dir))
            .map(|(id, _)| id.as_str())
            .collect()
    }
}

/// All declarations of `name`, however scoped.
fn candidates<'i>(
    by_name: &'i HashMap<String, Vec<(String, String)>>,
    name: &str,
) -> &'i [(String, String)] {
    by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
}

/// The repo directories an import path names, via the repo's go.mod
/// module paths: an import of `<module>/<rest>` lives at
/// `<module dir>/<rest>`. The most specific (longest) module path wins
/// — a nested go.mod owns its subtree, so the parent module never also
/// claims it. An import no module path covers is external: no
/// directories.
fn resolve_import_dirs(modules: &[(String, String)], import_path: &str) -> Vec<String> {
    let matches: Vec<(usize, String)> = modules
        .iter()
        .filter_map(|(dir, module)| {
            let rest = if import_path == module {
                Some("")
            } else {
                import_path
                    .strip_prefix(module.as_str())
                    .and_then(|rest| rest.strip_prefix('/'))
            };
            rest.map(|rest| {
                let resolved = match (dir.is_empty(), rest.is_empty()) {
                    (_, true) => dir.clone(),
                    (true, false) => rest.to_string(),
                    (false, false) => format!("{dir}/{rest}"),
                };
                (module.len(), resolved)
            })
        })
        .collect();
    let Some(most_specific) = matches.iter().map(|(len, _)| *len).max() else {
        return Vec::new();
    };
    let mut dirs: Vec<String> = matches
        .into_iter()
        .filter(|(len, _)| *len == most_specific)
        .map(|(_, dir)| dir)
        .collect();
    // Two go.mods declaring the same module path (mid-edit) can tie:
    // ambiguity is kept, duplicates are not.
    dirs.sort_unstable();
    dirs.dedup();
    dirs
}

/// One edge per candidate at split confidence — ADR 0006's ambiguity
/// policy in one place: N candidates share SYNTACTIC_MATCH equally,
/// recorded rather than dropped; no candidates, no edges.
fn push_candidate_edges(
    graph: &mut Graph,
    src: &str,
    candidates: &[&str],
    kind: EdgeKind,
    location: &str,
) {
    if candidates.is_empty() {
        return;
    }
    let confidence = SYNTACTIC_MATCH / candidates.len() as f64;
    for target in candidates {
        graph.edges.push(Edge {
            src: src.to_string(),
            dst: (*target).to_string(),
            kind,
            provenance: Provenance::Syntactic,
            confidence,
            location: Some(location.to_string()),
        });
    }
}

/// The `<path>:<line>:<col>` (1-based) site of a parse-tree node. The
/// column is a byte offset within the line (what tree-sitter reports),
/// not a display column — it primarily disambiguates two sites on one
/// line, which would otherwise be byte-identical rows; a consumer that
/// needs a display column maps bytes→characters against the source.
pub(crate) fn site(path: &str, node: tree_sitter::Node<'_>) -> String {
    let position = node.start_position();
    format!("{path}:{}:{}", position.row + 1, position.column + 1)
}

/// IMPORTS edges for one file (phase 2): each import spec connects the
/// File to its package's node (minted once per import path — node ids
/// are the segment's primary key) at confidence 1.0: the statement is
/// witnessed in the source, not guessed; only the pass is syntactic.
/// An import that go.mod places inside this repo additionally connects
/// the File to the package's Go files (RFC 0001 §5: IMPORTS is File →
/// File/Package) — the directory resolution is the heuristic part — but
/// never to the importing file itself: an external `_test` package
/// lives in the very directory it imports. When an import path resolves
/// to several candidate directories (tied module-path declarations),
/// those directories are alternatives, so confidence spreads across
/// them per ADR 0006; the files within one resolved package are all
/// genuinely imported, so they share that directory's confidence rather
/// than splitting it further. The common single-directory case keeps
/// SYNTACTIC_MATCH.
fn emit_import_edges(
    file: &GoFileFacts,
    index: &SymbolIndex,
    imported: &mut HashSet<String>,
    graph: &mut Graph,
) {
    for import in &file.imports {
        let dirs = index.dirs_of(&import.path);
        let confidence = SYNTACTIC_MATCH / dirs.len().max(1) as f64;
        let mut targets = Vec::new();
        for dir in dirs {
            for target in index
                .files_by_dir
                .get(dir)
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                if *target == file.file_id {
                    continue;
                }
                targets.push(target.as_str());
            }
        }
        emit_import(
            &file.file_id,
            &import.path,
            &import.location,
            &targets,
            confidence,
            imported,
            graph,
        );
    }
}

/// CALLS edges for one file (phase 2): each collected call site
/// resolves against the repo's declarations — N candidates at
/// SYNTACTIC_MATCH/N each (ADR 0006); a name the repo doesn't declare
/// (stdlib, external packages, builtins) yields nothing.
fn emit_call_edges(file: &GoFileFacts, index: &SymbolIndex, graph: &mut Graph) {
    let allow_repo_wide = !file.has_dot_import;
    for call in &file.calls {
        let candidates = index.resolve_callee(&call.callee, &file.dir, allow_repo_wide);
        push_candidate_edges(
            graph,
            &call.caller_id,
            &candidates,
            EdgeKind::Calls,
            &call.location,
        );
    }
}

/// EXTENDS edges for one file (phase 2): every embedded type in a
/// struct or interface declaration extends the embedding type, resolved
/// like a call target. An interface subject keeps only interface
/// targets — a concrete target named by an interface is a generic
/// constraint (`interface { MyInt }`), not an embedding.
fn emit_extends_edges(file: &GoFileFacts, index: &SymbolIndex, graph: &mut Graph) {
    let allow_repo_wide = !file.has_dot_import;
    for embed in &file.embeds {
        let mut candidates = index.resolve_type(&embed.reference, &file.dir, allow_repo_wide);
        if embed.subject_is_interface {
            candidates.retain(|id| index.interface_ids.contains(*id));
        }
        push_candidate_edges(
            graph,
            &embed.subject_id,
            &candidates,
            EdgeKind::Extends,
            &embed.location,
        );
    }
}

/// IMPLEMENTS edges (phase 2): a type whose method names cover an
/// interface's directly declared method names IMPLEMENTS it (RFC 0001
/// §5), repo-wide — Go interfaces are satisfied across package
/// boundaries. Matching is by name only (signatures are invisible to a
/// reasonable syntactic pass), so confidence is capped at
/// NAME_ONLY_MATCH however unique the match; an interface with no
/// direct methods (`any`, `interface{}`, embeddings only) matches
/// nothing — everything satisfies it, so edges to it would be noise.
/// No location: the relationship has no single site.
///
/// Candidates come from an inverted method-name index, seeded by each
/// interface's rarest method, so cost tracks actual near-matches
/// instead of types × interfaces.
fn emit_implements_edges(files: &[GoFileFacts], graph: &mut Graph) {
    // Method sets per (package dir, receiver type name) — Go only
    // permits methods in the receiver type's own package, so the pair
    // identifies the type. Slot-indexed so everything downstream
    // iterates in first-declaration order: edge output is checksummed,
    // and HashMap order must never leak into it.
    let mut receiver_slots: HashMap<(&str, &str), usize> = HashMap::new();
    let mut receivers: Vec<((&str, &str), BTreeSet<&str>)> = Vec::new();
    for file in files {
        for method in &file.methods {
            let Some(receiver) = &method.receiver else {
                continue;
            };
            let key = (file.dir.as_str(), receiver.as_str());
            let slot = *receiver_slots.entry(key).or_insert_with(|| {
                receivers.push((key, BTreeSet::new()));
                receivers.len() - 1
            });
            receivers[slot].1.insert(&method.name);
        }
    }
    // Inverted: method name → receivers declaring it, declaration order.
    let mut by_method: HashMap<&str, Vec<usize>> = HashMap::new();
    for (slot, (_, names)) in receivers.iter().enumerate() {
        for name in names {
            by_method.entry(name).or_default().push(slot);
        }
    }
    // Concrete types by (dir, name): the IMPLEMENTS sources (every type
    // that is not an interface).
    let mut concrete: HashMap<(&str, &str), Vec<&str>> = HashMap::new();
    for file in files {
        for declared in &file.types {
            if declared.interface.is_none() {
                concrete
                    .entry((file.dir.as_str(), declared.name.as_str()))
                    .or_default()
                    .push(&declared.id);
            }
        }
    }
    for file in files {
        for declared in &file.types {
            // Only interfaces whose *whole* method set is known here can
            // be matched: an interface that embeds another (or is a
            // generic constraint) would match on a subset and emit false
            // edges (see `InterfaceShape::complete`). Empty interfaces
            // (`any`) match nothing — everything satisfies them.
            let Some(shape) = &declared.interface else {
                continue;
            };
            if !shape.complete || shape.direct_methods.is_empty() {
                continue;
            }
            let needed = &shape.direct_methods;
            // Every needed method must have declarers at all; then the
            // rarest one's posting list seeds the candidate set.
            let postings: Option<Vec<&Vec<usize>>> = needed
                .iter()
                .map(|name| by_method.get(name.as_str()))
                .collect();
            let Some(postings) = postings else {
                continue;
            };
            let seed = postings
                .iter()
                .min_by_key(|posting| posting.len())
                .expect("needed is non-empty");
            for &slot in *seed {
                let (key, methods) = &receivers[slot];
                if !needed.iter().all(|name| methods.contains(name.as_str())) {
                    continue;
                }
                for type_id in concrete.get(key).map(Vec::as_slice).unwrap_or(&[]) {
                    graph.edges.push(Edge {
                        src: type_id.to_string(),
                        dst: declared.id.clone(),
                        kind: EdgeKind::Implements,
                        provenance: Provenance::Syntactic,
                        confidence: NAME_ONLY_MATCH,
                        location: None,
                    });
                }
            }
        }
    }
}

fn emit_simple_import_edges(
    file: &SimpleFileFacts,
    imported: &mut HashSet<String>,
    graph: &mut Graph,
) {
    for import in &file.imports {
        emit_import(
            &file.file_id,
            &import.path,
            &import.location,
            &[],
            SYNTACTIC_MATCH,
            imported,
            graph,
        );
    }
}

/// Emit one witnessed package import and any heuristically resolved file
/// targets. This is the single home for IMPORTS node deduplication, edge
/// provenance, confidence, and ordering.
fn emit_import(
    file_id: &str,
    import_path: &str,
    location: &str,
    targets: &[&str],
    target_confidence: f64,
    imported: &mut HashSet<String>,
    graph: &mut Graph,
) {
    let package = Node::package(import_path);
    let package_id = package.id.clone();
    if imported.insert(package_id.clone()) {
        graph.nodes.push(package);
    }
    graph.edges.push(Edge {
        src: file_id.to_string(),
        dst: package_id,
        kind: EdgeKind::Imports,
        provenance: Provenance::Syntactic,
        confidence: 1.0,
        location: Some(location.to_string()),
    });
    for target in targets {
        graph.edges.push(Edge {
            src: file_id.to_string(),
            dst: (*target).to_string(),
            kind: EdgeKind::Imports,
            provenance: Provenance::Syntactic,
            confidence: target_confidence,
            location: Some(location.to_string()),
        });
    }
}

fn emit_simple_call_edges(file: &SimpleFileFacts, index: &SimpleSymbolIndex, graph: &mut Graph) {
    for call in &file.calls {
        let candidates = index.resolve(&call.callee);
        push_candidate_edges(
            graph,
            &call.caller_id,
            &candidates,
            EdgeKind::Calls,
            &call.location,
        );
    }
}

pub(super) fn emit_edges(
    go_files: &[GoFileFacts],
    simple_files: &[SimpleFileFacts],
    modules: &[(String, String)],
    graph: &mut Graph,
) {
    let index = SymbolIndex::build(go_files, modules);
    let mut imported = HashSet::new();
    for file in go_files {
        emit_import_edges(file, &index, &mut imported, graph);
        emit_call_edges(file, &index, graph);
        emit_extends_edges(file, &index, graph);
    }
    emit_implements_edges(go_files, graph);

    let simple_index = SimpleSymbolIndex::build(simple_files);
    for file in simple_files {
        emit_simple_import_edges(file, &mut imported, graph);
        emit_simple_call_edges(file, &simple_index, graph);
    }
}
