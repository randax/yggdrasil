use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::Context;
use yg_shard::{Edge, EdgeKind, Graph, Node, NodeKind, Provenance, SearchDoc};

use crate::resolve::{
    self, GoCall, GoEmbed, GoFileFacts, GoImport, GoMethod, GoReference, GoType, InterfaceShape,
    SimpleCall, SimpleExtractionCtx, SimpleFileFacts, SimpleImport, site,
};

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
        extractor: extract_go,
        grammar_name: "Go",
    },
    LanguagePack {
        extensions: &[".ts", ".mts", ".cts"],
        grammar: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        extractor: extract_ecmascript,
        grammar_name: "TypeScript",
    },
    LanguagePack {
        extensions: &[".tsx", ".jsx"],
        grammar: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        extractor: extract_ecmascript,
        grammar_name: "TSX",
    },
    LanguagePack {
        extensions: &[".js", ".mjs", ".cjs"],
        grammar: || tree_sitter_javascript::LANGUAGE.into(),
        extractor: extract_ecmascript,
        grammar_name: "JavaScript",
    },
    LanguagePack {
        extensions: &[".py"],
        grammar: || tree_sitter_python::LANGUAGE.into(),
        extractor: extract_python,
        grammar_name: "Python",
    },
    LanguagePack {
        extensions: &[".rs"],
        grammar: || tree_sitter_rust::LANGUAGE.into(),
        extractor: extract_rust,
        grammar_name: "Rust",
    },
    LanguagePack {
        extensions: &[".java"],
        grammar: || tree_sitter_java::LANGUAGE.into(),
        extractor: extract_java,
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

/// Phase 1 for one Go file: mint its Symbols and DEFINES edges and distill
/// the already-parsed tree into [`GoFileFacts`].
fn extract_go(
    root: tree_sitter::Node<'_>,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<ExtractedFacts> {
    let imports = extract_go_imports(root, path, source);
    let mut facts = GoFileFacts {
        file_id: file_id.to_string(),
        dir: package_dir(path).to_string(),
        has_dot_import: imports.iter().any(|import| import.dot),
        imports,
        calls: Vec::new(),
        embeds: Vec::new(),
        functions: Vec::new(),
        methods: Vec::new(),
        types: Vec::new(),
    };
    // Duplicate names (multiple `func init()`, redeclarations mid-edit)
    // must still mint unique node ids — the graph segment keys on them.
    let mut id_uses: HashMap<String, u32> = HashMap::new();
    let mut cursor = root.walk();
    for declaration in root.children(&mut cursor) {
        // CONTEXT.md's Symbol: function, method, type, constant. Each
        // top-level Go declaration of those kinds names one or more.
        match declaration.kind() {
            "function_declaration" => {
                let Some(name) = field_text(declaration, "name", source) else {
                    continue;
                };
                let id = mint_symbol(path, file_id, name, &mut id_uses, graph);
                facts.functions.push((name.to_string(), id.clone()));
                collect_call_sites(
                    declaration,
                    source,
                    &id,
                    &facts.imports,
                    path,
                    &mut facts.calls,
                );
            }
            // Methods are receiver-qualified (Widget.Render): two types'
            // same-named methods are different Symbols.
            "method_declaration" => {
                let Some(name) = field_text(declaration, "name", source) else {
                    continue;
                };
                let receiver = receiver_type_name(declaration, source);
                let qualified = match receiver {
                    Some(receiver) => format!("{receiver}.{name}"),
                    None => name.to_string(),
                };
                let id = mint_symbol(path, file_id, &qualified, &mut id_uses, graph);
                facts.methods.push(GoMethod {
                    receiver: receiver.map(str::to_string),
                    name: name.to_string(),
                    id: id.clone(),
                });
                collect_call_sites(
                    declaration,
                    source,
                    &id,
                    &facts.imports,
                    path,
                    &mut facts.calls,
                );
            }
            // One declaration can hold many specs: type ( A …; B … ),
            // const ( X = 1; Y = 2 ) — and one const spec many names.
            "type_declaration" | "const_declaration" => {
                let mut specs = declaration.walk();
                for spec in declaration.children(&mut specs) {
                    if !matches!(spec.kind(), "type_spec" | "type_alias" | "const_spec") {
                        continue;
                    }
                    let mut names = spec.walk();
                    let names: Vec<String> = spec
                        .children_by_field_name("name", &mut names)
                        .filter_map(|n| n.utf8_text(source).ok().map(str::to_string))
                        .collect();
                    for name in names {
                        let id = mint_symbol(path, file_id, &name, &mut id_uses, graph);
                        if matches!(spec.kind(), "type_spec" | "type_alias") {
                            collect_type_facts(
                                spec,
                                source,
                                &name,
                                &id,
                                &facts.imports,
                                path,
                                &mut facts.types,
                                &mut facts.embeds,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Some(ExtractedFacts::Go(facts))
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

fn extract_ecmascript(
    root: tree_sitter::Node<'_>,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<ExtractedFacts> {
    extract_simple_language(
        root,
        path,
        file_id,
        source,
        graph,
        SimpleLanguage {
            imports: extract_ecmascript_imports,
            walk: SimpleWalk::TopLevel,
            collect: |declaration, context: &mut SimpleExtractionCtx<'_, '_>| {
                collect_ecmascript_top_level_declaration(
                    declaration,
                    context.source,
                    context.path,
                    context.file_id,
                    context.graph,
                    context.id_uses,
                    context.facts,
                );
            },
        },
    )
}

fn collect_ecmascript_top_level_declaration(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    path: &str,
    file_id: &str,
    graph: &mut Graph,
    id_uses: &mut HashMap<String, u32>,
    facts: &mut SimpleFileFacts,
) {
    if declaration.kind() == "export_statement" {
        let mut cursor = declaration.walk();
        for child in declaration.children(&mut cursor) {
            collect_ecmascript_top_level_declaration(
                child, source, path, file_id, graph, id_uses, facts,
            );
        }
        return;
    }
    match declaration.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "type_alias_declaration"
        | "enum_declaration" => {
            let Some(name) = field_text(declaration, "name", source) else {
                return;
            };
            let id = mint_symbol(path, file_id, name, id_uses, graph);
            facts.declarations.push((name.to_string(), id.clone()));
            collect_ecmascript_calls(declaration, source, &id, path, &mut facts.calls);
        }
        "interface_declaration" => {
            let Some(name) = field_text(declaration, "name", source) else {
                return;
            };
            let id = mint_symbol(path, file_id, name, id_uses, graph);
            facts.declarations.push((name.to_string(), id));
            let mut ctx = SimpleExtractionCtx {
                source,
                path,
                file_id,
                graph,
                id_uses,
                facts,
            };
            collect_ecmascript_interface_methods(declaration, name, &mut ctx);
        }
        "class_declaration" | "abstract_class_declaration" => {
            let Some(name) = field_text(declaration, "name", source) else {
                return;
            };
            let id = mint_symbol(path, file_id, name, id_uses, graph);
            facts.declarations.push((name.to_string(), id));
            let mut ctx = SimpleExtractionCtx {
                source,
                path,
                file_id,
                graph,
                id_uses,
                facts,
            };
            collect_ecmascript_class_methods(declaration, name, &mut ctx);
        }
        "lexical_declaration" | "variable_declaration" => {
            for declarator in descendants_of_kind(declaration, "variable_declarator") {
                let Some(name_node) = declarator
                    .child_by_field_name("name")
                    .filter(|n| n.kind() == "identifier")
                else {
                    continue;
                };
                let Some(name) = name_node.utf8_text(source).ok() else {
                    continue;
                };
                let id = mint_symbol(path, file_id, name, id_uses, graph);
                facts.declarations.push((name.to_string(), id.clone()));
                collect_ecmascript_calls(declarator, source, &id, path, &mut facts.calls);
            }
        }
        _ => {}
    }
}

fn collect_ecmascript_class_methods(
    class_declaration: tree_sitter::Node<'_>,
    class_name: &str,
    ctx: &mut SimpleExtractionCtx<'_, '_>,
) {
    let Some(body) = class_declaration.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for item in body.children(&mut cursor) {
        if item.kind() != "method_definition" {
            continue;
        }
        let Some(name) = field_text(item, "name", ctx.source).map(clean_property_name) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let qualified = format!("{class_name}.{name}");
        let id = mint_symbol(ctx.path, ctx.file_id, &qualified, ctx.id_uses, ctx.graph);
        ctx.facts.declarations.push((name.to_string(), id.clone()));
        collect_ecmascript_calls(item, ctx.source, &id, ctx.path, &mut ctx.facts.calls);
    }
}

fn collect_ecmascript_interface_methods(
    interface_declaration: tree_sitter::Node<'_>,
    interface_name: &str,
    ctx: &mut SimpleExtractionCtx<'_, '_>,
) {
    let Some(body) = interface_declaration.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for item in body.children(&mut cursor) {
        if item.kind() != "method_signature" {
            continue;
        }
        let Some(name) = field_text(item, "name", ctx.source).map(clean_property_name) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let qualified = format!("{interface_name}.{name}");
        let id = mint_symbol(ctx.path, ctx.file_id, &qualified, ctx.id_uses, ctx.graph);
        ctx.facts.declarations.push((name.to_string(), id));
    }
}

fn clean_property_name(name: &str) -> &str {
    name.trim_matches(['"', '\''])
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

fn extract_ecmascript_imports(
    root: tree_sitter::Node<'_>,
    path: &str,
    source: &[u8],
) -> Vec<SimpleImport> {
    descendants_of_kind(root, "import_statement")
        .into_iter()
        .filter_map(|statement| {
            let import_path = field_text(statement, "source", source)?
                .trim_matches(['"', '\''])
                .to_string();
            if import_path.is_empty() || import_path.starts_with('.') {
                return None;
            }
            Some(SimpleImport {
                path: import_path,
                location: site(path, statement),
            })
        })
        .collect()
}

fn collect_ecmascript_calls(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    caller_id: &str,
    path: &str,
    calls: &mut Vec<SimpleCall>,
) {
    for call in descendants_of_kind(declaration, "call_expression") {
        let Some(function) = call.child_by_field_name("function") else {
            continue;
        };
        let Some(callee) = ecmascript_callee_name(function, source) else {
            continue;
        };
        calls.push(SimpleCall {
            caller_id: caller_id.to_string(),
            callee: callee.to_string(),
            location: site(path, call),
        });
    }
    for call in descendants_of_kind(declaration, "new_expression") {
        let Some(constructor) = call.child_by_field_name("constructor") else {
            continue;
        };
        let Some(callee) = simple_expression_name(constructor, source) else {
            continue;
        };
        calls.push(SimpleCall {
            caller_id: caller_id.to_string(),
            callee: callee.to_string(),
            location: site(path, call),
        });
    }
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

fn ecmascript_callee_name<'a>(
    expression: tree_sitter::Node<'_>,
    source: &'a [u8],
) -> Option<&'a str> {
    match expression.kind() {
        "identifier" => expression.utf8_text(source).ok(),
        "member_expression" => {
            let object = expression
                .child_by_field_name("object")
                .and_then(|node| node.utf8_text(source).ok());
            if object == Some("this") {
                field_text(expression, "property", source)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn extract_rust(
    root: tree_sitter::Node<'_>,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<ExtractedFacts> {
    extract_simple_language(
        root,
        path,
        file_id,
        source,
        graph,
        SimpleLanguage {
            imports: extract_rust_imports,
            walk: SimpleWalk::TopLevel,
            collect: |declaration: tree_sitter::Node<'_>,
                      context: &mut SimpleExtractionCtx<'_, '_>| {
                match declaration.kind() {
                    "function_item" | "struct_item" | "enum_item" | "trait_item" | "const_item"
                    | "static_item" | "type_item" => {
                        let Some(name) = field_text(declaration, "name", context.source) else {
                            return;
                        };
                        let id = mint_symbol(
                            context.path,
                            context.file_id,
                            name,
                            context.id_uses,
                            context.graph,
                        );
                        context
                            .facts
                            .declarations
                            .push((name.to_string(), id.clone()));
                        collect_rust_calls(
                            declaration,
                            context.source,
                            &id,
                            context.path,
                            &mut context.facts.calls,
                        );
                    }
                    "impl_item" => collect_rust_impl_item(
                        declaration,
                        context.source,
                        context.path,
                        context.file_id,
                        context.graph,
                        context.id_uses,
                        context.facts,
                    ),
                    _ => {}
                }
            },
        },
    )
}

fn collect_rust_impl_item(
    impl_item: tree_sitter::Node<'_>,
    source: &[u8],
    path: &str,
    file_id: &str,
    graph: &mut Graph,
    id_uses: &mut HashMap<String, u32>,
    facts: &mut SimpleFileFacts,
) {
    let Some(receiver) = impl_item
        .child_by_field_name("type")
        .and_then(|node| rust_type_name(node, source))
    else {
        return;
    };
    let Some(body) = impl_item.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for item in body.children(&mut cursor) {
        if item.kind() != "function_item" {
            continue;
        }
        let Some(name) = field_text(item, "name", source) else {
            continue;
        };
        let qualified = format!("{receiver}.{name}");
        let id = mint_symbol(path, file_id, &qualified, id_uses, graph);
        facts.declarations.push((name.to_string(), id.clone()));
        collect_rust_calls(item, source, &id, path, &mut facts.calls);
    }
}

fn rust_type_name<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    match node.kind() {
        "type_identifier" | "primitive_type" => node.utf8_text(source).ok(),
        "generic_type" => field_text(node, "type", source),
        _ => first_of_kind(node, "type_identifier")
            .or_else(|| first_of_kind(node, "primitive_type"))
            .and_then(|n| n.utf8_text(source).ok()),
    }
}

fn extract_rust_imports(
    root: tree_sitter::Node<'_>,
    path: &str,
    source: &[u8],
) -> Vec<SimpleImport> {
    descendants_of_kind(root, "use_declaration")
        .into_iter()
        .filter_map(|declaration| {
            let argument = declaration.child_by_field_name("argument")?;
            if argument
                .utf8_text(source)
                .ok()
                .is_some_and(is_rust_internal_use_path)
            {
                return None;
            }
            let package = rust_use_root(argument, source)?;
            Some(SimpleImport {
                path: package.to_string(),
                location: site(path, declaration),
            })
        })
        .collect()
}

fn is_rust_internal_use_path(path: &str) -> bool {
    matches!(path, "crate" | "self" | "super")
        || path.starts_with("crate::")
        || path.starts_with("self::")
        || path.starts_with("super::")
}

fn rust_use_root<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    match node.kind() {
        "identifier" => node.utf8_text(source).ok(),
        "scoped_identifier" => node
            .child_by_field_name("path")
            .and_then(|path| rust_use_root(path, source)),
        "use_as_clause" | "scoped_use_list" => first_of_kind(node, "identifier")
            .and_then(|identifier| identifier.utf8_text(source).ok()),
        _ => None,
    }
}

fn collect_rust_calls(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    caller_id: &str,
    path: &str,
    calls: &mut Vec<SimpleCall>,
) {
    for call in descendants_of_kind(declaration, "call_expression") {
        let Some(function) = call.child_by_field_name("function") else {
            continue;
        };
        let Some(callee) = rust_callee_name(function, source) else {
            continue;
        };
        calls.push(SimpleCall {
            caller_id: caller_id.to_string(),
            callee: callee.to_string(),
            location: site(path, call),
        });
    }
}

fn rust_callee_name<'a>(expression: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    simple_expression_name(expression, source).or_else(|| last_identifier_name(expression, source))
}

fn last_identifier_name<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    descendants_of_kind(node, "identifier")
        .into_iter()
        .last()
        .and_then(|identifier| identifier.utf8_text(source).ok())
}

fn extract_java(
    root: tree_sitter::Node<'_>,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<ExtractedFacts> {
    extract_simple_language(
        root,
        path,
        file_id,
        source,
        graph,
        SimpleLanguage {
            imports: extract_java_imports,
            walk: SimpleWalk::Descendants,
            collect: |declaration: tree_sitter::Node<'_>,
                      context: &mut SimpleExtractionCtx<'_, '_>| {
                if is_java_declaration_kind(declaration.kind()) {
                    collect_java_declaration(declaration, context);
                }
            },
        },
    )
}

fn is_java_declaration_kind(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "enum_declaration"
            | "interface_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "field_declaration"
            | "method_declaration"
    )
}

fn collect_java_declaration(
    declaration: tree_sitter::Node<'_>,
    ctx: &mut SimpleExtractionCtx<'_, '_>,
) {
    match declaration.kind() {
        "class_declaration"
        | "enum_declaration"
        | "interface_declaration"
        | "record_declaration"
        | "annotation_type_declaration" => {
            let Some(name) = field_text(declaration, "name", ctx.source) else {
                return;
            };
            let id = mint_symbol(ctx.path, ctx.file_id, name, ctx.id_uses, ctx.graph);
            ctx.facts.declarations.push((name.to_string(), id));
        }
        "method_declaration" => {
            let Some(name) = field_text(declaration, "name", ctx.source) else {
                return;
            };
            let symbol_name = java_member_symbol_name(declaration, ctx.source, name);
            let id = mint_symbol(ctx.path, ctx.file_id, &symbol_name, ctx.id_uses, ctx.graph);
            ctx.facts.declarations.push((name.to_string(), id.clone()));
            collect_java_calls(declaration, ctx.source, &id, ctx.path, &mut ctx.facts.calls);
        }
        "field_declaration" => {
            let mut cursor = declaration.walk();
            for declarator in declaration.children_by_field_name("declarator", &mut cursor) {
                let Some(name) = field_text(declarator, "name", ctx.source) else {
                    continue;
                };
                let symbol_name = java_member_symbol_name(declaration, ctx.source, name);
                let id = mint_symbol(ctx.path, ctx.file_id, &symbol_name, ctx.id_uses, ctx.graph);
                ctx.facts.declarations.push((name.to_string(), id.clone()));
                collect_java_calls(declarator, ctx.source, &id, ctx.path, &mut ctx.facts.calls);
            }
        }
        _ => {}
    }
}

fn java_member_symbol_name(member: tree_sitter::Node<'_>, source: &[u8], name: &str) -> String {
    match java_containing_type_name(member, source) {
        Some(container) => format!("{container}.{name}"),
        None => name.to_string(),
    }
}

fn java_containing_type_name<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let mut current = node.parent();
    while let Some(node) = current {
        if matches!(
            node.kind(),
            "class_declaration"
                | "enum_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "annotation_type_declaration"
        ) {
            return field_text(node, "name", source);
        }
        current = node.parent();
    }
    None
}

fn extract_java_imports(
    root: tree_sitter::Node<'_>,
    path: &str,
    source: &[u8],
) -> Vec<SimpleImport> {
    descendants_of_kind(root, "import_declaration")
        .into_iter()
        .filter_map(|declaration| {
            let mut cursor = declaration.walk();
            let imported = declaration
                .named_children(&mut cursor)
                .find(|child| matches!(child.kind(), "identifier" | "scoped_identifier"))?
                .utf8_text(source)
                .ok()?;
            Some(SimpleImport {
                path: imported.to_string(),
                location: site(path, declaration),
            })
        })
        .collect()
}

fn collect_java_calls(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    caller_id: &str,
    path: &str,
    calls: &mut Vec<SimpleCall>,
) {
    for call in descendants_of_kind(declaration, "method_invocation") {
        if let Some(object) = call.child_by_field_name("object") {
            let object = object.utf8_text(source).ok();
            if !matches!(object, Some("this" | "super")) {
                continue;
            }
        }
        let Some(callee) = field_text(call, "name", source) else {
            continue;
        };
        calls.push(SimpleCall {
            caller_id: caller_id.to_string(),
            callee: callee.to_string(),
            location: site(path, call),
        });
    }
    for call in descendants_of_kind(declaration, "object_creation_expression") {
        let Some(created_type) = call.child_by_field_name("type") else {
            continue;
        };
        let Some(callee) = simple_expression_name(created_type, source) else {
            continue;
        };
        calls.push(SimpleCall {
            caller_id: caller_id.to_string(),
            callee: callee.to_string(),
            location: site(path, call),
        });
    }
}

fn extract_python(
    root: tree_sitter::Node<'_>,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<ExtractedFacts> {
    extract_simple_language(
        root,
        path,
        file_id,
        source,
        graph,
        SimpleLanguage {
            imports: extract_python_imports,
            walk: SimpleWalk::TopLevel,
            collect: |declaration, context: &mut SimpleExtractionCtx<'_, '_>| {
                collect_python_top_level_declaration(
                    declaration,
                    context.source,
                    context.path,
                    context.file_id,
                    context.graph,
                    context.id_uses,
                    context.facts,
                );
            },
        },
    )
}

fn collect_python_top_level_declaration(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    path: &str,
    file_id: &str,
    graph: &mut Graph,
    id_uses: &mut HashMap<String, u32>,
    facts: &mut SimpleFileFacts,
) {
    if declaration.kind() == "expression_statement" {
        let mut cursor = declaration.walk();
        for child in declaration.children(&mut cursor) {
            collect_python_top_level_declaration(
                child, source, path, file_id, graph, id_uses, facts,
            );
        }
        return;
    }
    match declaration.kind() {
        "class_definition" => {
            let Some(name) = field_text(declaration, "name", source) else {
                return;
            };
            let id = mint_symbol(path, file_id, name, id_uses, graph);
            facts.declarations.push((name.to_string(), id));
            let mut ctx = SimpleExtractionCtx {
                source,
                path,
                file_id,
                graph,
                id_uses,
                facts,
            };
            collect_python_class_methods(declaration, name, &mut ctx);
        }
        "function_definition" => {
            let Some(name) = field_text(declaration, "name", source) else {
                return;
            };
            let id = mint_symbol(path, file_id, name, id_uses, graph);
            facts.declarations.push((name.to_string(), id.clone()));
            collect_python_calls(declaration, source, &id, path, &mut facts.calls);
        }
        "assignment" => {
            let Some(left) = declaration
                .child_by_field_name("left")
                .filter(|n| n.kind() == "identifier")
            else {
                return;
            };
            let Some(name) = left.utf8_text(source).ok() else {
                return;
            };
            let id = mint_symbol(path, file_id, name, id_uses, graph);
            facts.declarations.push((name.to_string(), id.clone()));
            collect_python_calls(declaration, source, &id, path, &mut facts.calls);
        }
        _ => {}
    }
}

fn collect_python_class_methods(
    class_definition: tree_sitter::Node<'_>,
    class_name: &str,
    ctx: &mut SimpleExtractionCtx<'_, '_>,
) {
    let Some(body) = class_definition.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for item in body.children(&mut cursor) {
        if item.kind() != "function_definition" {
            continue;
        }
        let Some(name) = field_text(item, "name", ctx.source) else {
            continue;
        };
        let qualified = format!("{class_name}.{name}");
        let id = mint_symbol(ctx.path, ctx.file_id, &qualified, ctx.id_uses, ctx.graph);
        ctx.facts.declarations.push((name.to_string(), id.clone()));
        collect_python_calls(item, ctx.source, &id, ctx.path, &mut ctx.facts.calls);
    }
}

fn extract_python_imports(
    root: tree_sitter::Node<'_>,
    path: &str,
    source: &[u8],
) -> Vec<SimpleImport> {
    let mut imports = Vec::new();
    for statement in descendants_of_kind(root, "import_from_statement") {
        if let Some(module) =
            field_text(statement, "module_name", source).filter(|module| !module.starts_with('.'))
        {
            imports.push(SimpleImport {
                path: module.to_string(),
                location: site(path, statement),
            });
        }
    }
    for statement in descendants_of_kind(root, "import_statement") {
        let mut cursor = statement.walk();
        for name in statement.children_by_field_name("name", &mut cursor) {
            let package = match name.kind() {
                "dotted_name" | "identifier" => name.utf8_text(source).ok(),
                "aliased_import" => first_of_kind(name, "dotted_name")
                    .or_else(|| first_of_kind(name, "identifier"))
                    .and_then(|n| n.utf8_text(source).ok()),
                _ => None,
            };
            let Some(package) = package.filter(|package| !package.is_empty()) else {
                continue;
            };
            imports.push(SimpleImport {
                path: package.to_string(),
                location: site(path, statement),
            });
        }
    }
    imports
}

fn collect_python_calls(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    caller_id: &str,
    path: &str,
    calls: &mut Vec<SimpleCall>,
) {
    for call in descendants_of_kind(declaration, "call") {
        let Some(function) = call.child_by_field_name("function") else {
            continue;
        };
        let Some(callee) = python_callee_name(function, source) else {
            continue;
        };
        calls.push(SimpleCall {
            caller_id: caller_id.to_string(),
            callee: callee.to_string(),
            location: site(path, call),
        });
    }
}

fn python_callee_name<'a>(expression: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    match expression.kind() {
        "identifier" => expression.utf8_text(source).ok(),
        "attribute" => {
            let object = expression
                .child_by_field_name("object")
                .and_then(|node| node.utf8_text(source).ok());
            if matches!(object, Some("self" | "cls")) {
                field_text(expression, "attribute", source)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Every call site inside one declaration's subtree, attributed to that
/// declaration's symbol and classified against the file's imports.
fn collect_call_sites(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    caller_id: &str,
    imports: &[GoImport],
    path: &str,
    calls: &mut Vec<GoCall>,
) {
    for call in descendants_of_kind(declaration, "call_expression") {
        let Some(function) = call.child_by_field_name("function") else {
            continue;
        };
        let callee = match function.kind() {
            // An unqualified call: a function name.
            "identifier" => match function.utf8_text(source).ok() {
                Some(name) => GoReference::Unqualified(name.to_string()),
                None => continue,
            },
            "selector_expression" => {
                let Some(name) = field_text(function, "field", source) else {
                    continue;
                };
                let base = function
                    .child_by_field_name("operand")
                    .filter(|operand| operand.kind() == "identifier")
                    .and_then(|operand| operand.utf8_text(source).ok());
                match base.and_then(|base| import_named(imports, base)) {
                    // `util.Reset()` where util names an import: a
                    // package-qualified call — never a method.
                    Some(import) => GoReference::Imported {
                        import_path: import.path.clone(),
                        name: name.to_string(),
                    },
                    // `x.Render()`: a method call.
                    None => GoReference::Method(name.to_string()),
                }
            }
            _ => continue,
        };
        calls.push(GoCall {
            caller_id: caller_id.to_string(),
            callee,
            location: site(path, call),
        });
    }
}

/// Facts of one type spec: the declared type itself (with an
/// interface's direct method names) plus its embedded-type references.
#[allow(clippy::too_many_arguments)]
fn collect_type_facts(
    spec: tree_sitter::Node<'_>,
    source: &[u8],
    name: &str,
    id: &str,
    imports: &[GoImport],
    path: &str,
    types: &mut Vec<GoType>,
    embeds: &mut Vec<GoEmbed>,
) {
    let type_node = spec.child_by_field_name("type");
    let interface = type_node
        .filter(|node| node.kind() == "interface_type")
        .map(|node| interface_shape(node, source));
    let subject_is_interface = interface.is_some();
    types.push(GoType {
        name: name.to_string(),
        id: id.to_string(),
        interface,
    });
    let embedded: Vec<tree_sitter::Node> = match type_node.map(|node| (node.kind(), node)) {
        // An embedded struct field is a field_declaration with no name
        // — only its type, possibly behind a pointer. Direct fields
        // only: a nested anonymous struct's fields embed into that
        // struct, not this type.
        Some(("struct_type", node)) => {
            let mut cursor = node.walk();
            let fields = node
                .children(&mut cursor)
                .find(|n| n.kind() == "field_declaration_list");
            match fields {
                Some(fields) => {
                    let mut cursor = fields.walk();
                    fields
                        .children(&mut cursor)
                        .filter(|field| field.kind() == "field_declaration")
                        .filter(|field| field.child_by_field_name("name").is_none())
                        .collect()
                }
                None => Vec::new(),
            }
        }
        // An embedded interface is a type_elem naming exactly one type
        // (`io.Reader`, `Base`). A type_elem that is a constraint union
        // (`A | B`) or approximation (`~int`) is generics, not
        // embedding — `embedded_interface_type` returns None for those,
        // so they yield no EXTENDS edge (and `A | B` never collapses to
        // a spurious edge to just `A`).
        Some(("interface_type", node)) => {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .filter(|elem| elem.kind() == "type_elem")
                .filter_map(embedded_interface_type)
                .collect()
        }
        _ => Vec::new(),
    };
    for embed in embedded {
        let Some((package, type_name)) = embedded_type_reference(embed, source) else {
            continue;
        };
        let reference = match package {
            // `util.Base`: the package half resolves through this
            // file's imports, like a qualified call; a qualifier that
            // names no import resolves to nothing.
            Some(package) => match import_named(imports, package) {
                Some(import) => GoReference::Imported {
                    import_path: import.path.clone(),
                    name: type_name.to_string(),
                },
                None => continue,
            },
            None => GoReference::Unqualified(type_name.to_string()),
        };
        embeds.push(GoEmbed {
            subject_id: id.to_string(),
            subject_is_interface,
            reference,
            location: site(path, embed),
        });
    }
}

/// An interface declaration's method set and whether it is complete.
/// Any `type_elem` (an embedded interface, or a `A | B` / `~int`
/// constraint) makes the set incomplete: the embedded methods aren't
/// resolved here, and a constraint isn't a regular interface at all.
fn interface_shape(interface: tree_sitter::Node<'_>, source: &[u8]) -> InterfaceShape {
    let mut direct_methods = BTreeSet::new();
    let mut complete = true;
    let mut cursor = interface.walk();
    for elem in interface.children(&mut cursor) {
        match elem.kind() {
            "method_elem" => {
                if let Some(name) = field_text(elem, "name", source) {
                    direct_methods.insert(name.to_string());
                }
            }
            "type_elem" => complete = false,
            _ => {}
        }
    }
    InterfaceShape {
        direct_methods,
        complete,
    }
}

/// The single type an interface `type_elem` embeds — `Some` only when
/// the element is a lone type name (`io.Reader`, `Base`). A union
/// (`A | B`, several named children) or an approximation (`~int`, a
/// `negated_type` child) is a generic constraint, not an embedding, and
/// yields `None`.
fn embedded_interface_type(type_elem: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut cursor = type_elem.walk();
    let named: Vec<tree_sitter::Node> = type_elem.named_children(&mut cursor).collect();
    match named.as_slice() {
        [only] if matches!(only.kind(), "type_identifier" | "qualified_type") => Some(*only),
        _ => None,
    }
}

/// The type name an embedded field or interface element refers to:
/// `(package, name)` for `util.Base`, `(None, name)` for `Base` or
/// `*Base` — the first qualified or bare type identifier in the
/// embedding, however it is wrapped (pointer, generic instantiation).
fn embedded_type_reference<'a>(
    embed: tree_sitter::Node<'_>,
    source: &'a [u8],
) -> Option<(Option<&'a str>, &'a str)> {
    if let Some(qualified) = first_of_kind(embed, "qualified_type") {
        let package = field_text(qualified, "package", source)?;
        let name = field_text(qualified, "name", source)?;
        return Some((Some(package), name));
    }
    first_of_kind(embed, "type_identifier")
        .and_then(|n| n.utf8_text(source).ok())
        .map(|name| (None, name))
}

/// A Go file's imports: every spec under every import declaration,
/// whether single (`import "fmt"`) or grouped (`import ( … )`). A spec
/// with an empty path (`import ""` — illegal Go, mid-edit garbage) is
/// skipped whole: a `pkg:` node with no path could never round-trip
/// through the external id grammar.
fn extract_go_imports(root: tree_sitter::Node<'_>, path: &str, source: &[u8]) -> Vec<GoImport> {
    let mut imports = Vec::new();
    let mut cursor = root.walk();
    for declaration in root.children(&mut cursor) {
        if declaration.kind() != "import_declaration" {
            continue;
        }
        for spec in descendants_of_kind(declaration, "import_spec") {
            // The path literal keeps its quotes in the parse tree.
            let Some(import_path) = field_text(spec, "path", source)
                .map(|quoted| quoted.trim_matches(['"', '`']).to_string())
                .filter(|p| !p.is_empty())
            else {
                continue;
            };
            let alias = field_text(spec, "name", source);
            let local_name = match alias {
                // Blank and dot imports introduce no qualifying name —
                // but stay in the list: they are witnessed imports.
                Some("_") | Some(".") => None,
                Some(alias) => Some(alias.to_string()),
                // Unaliased: qualified by the path's last segment. (The
                // package's declared name can differ from its directory;
                // a heuristic pass accepts conflating them.)
                None => import_path.rsplit('/').next().map(str::to_string),
            };
            imports.push(GoImport {
                local_name: local_name.filter(|name| !name.is_empty()),
                path: import_path,
                location: site(path, spec),
                dot: alias == Some("."),
            });
        }
    }
    imports
}

/// The file's import a local name refers to — the one lookup both
/// reference classification and resolution share.
fn import_named<'i>(imports: &'i [GoImport], name: &str) -> Option<&'i GoImport> {
    imports
        .iter()
        .find(|import| import.local_name.as_deref() == Some(name))
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
