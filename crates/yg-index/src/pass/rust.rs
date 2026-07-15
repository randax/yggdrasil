use std::collections::HashMap;

use yg_shard::Graph;

use crate::resolve::{SimpleCall, SimpleExtractionCtx, SimpleFileFacts, SimpleImport, site};

use super::{
    ExtractedFacts, SimpleLanguage, SimpleWalk, descendants_of_kind, extract_simple_language,
    field_text, first_of_kind, mint_symbol, simple_expression_name,
};

pub(super) fn extract_rust(
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
