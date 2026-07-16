use std::collections::{BTreeSet, HashMap};

use yg_shard::Graph;

use crate::resolve::{
    GoCall, GoEmbed, GoFileFacts, GoImport, GoMethod, GoReference, GoType, InterfaceShape, site,
};

use super::{
    ExtractedFacts, descendants_of_kind, field_text, file_dir, first_of_kind, mint_symbol,
};

/// Phase 1 for one Go file: mint its Symbols and DEFINES edges and distill
/// the already-parsed tree into [`GoFileFacts`].
pub(super) fn extract_go(
    root: tree_sitter::Node<'_>,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<ExtractedFacts> {
    let imports = extract_go_imports(root, path, source);
    let mut facts = GoFileFacts {
        file_id: file_id.to_string(),
        dir: file_dir(path).to_string(),
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
