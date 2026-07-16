use yg_shard::Graph;

use crate::resolve::{SimpleCall, SimpleExtractionCtx, SimpleImport, SimpleLanguageTag, site};

use super::{
    ExtractedFacts, SimpleLanguage, descendants_of_kind, descendants_of_kind_before,
    extract_simple_language, field_text, first_of_kind, mint_symbol, simple_expression_name,
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
        SimpleLanguageTag::Rust,
        SimpleLanguage {
            imports: extract_rust_imports,
            collect: |declaration: tree_sitter::Node<'_>,
                      context: &mut SimpleExtractionCtx<'_, '_>| {
                match declaration.kind() {
                    "function_item" | "struct_item" | "enum_item" | "trait_item" | "const_item"
                    | "static_item" | "type_item" => {
                        let Some(name) = field_text(declaration, "name", context.source) else {
                            return;
                        };
                        let symbol_name =
                            rust_declaration_symbol_name(declaration, context.source, name);
                        let id = mint_symbol(
                            context.path,
                            context.file_id,
                            &symbol_name,
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
                    _ => {}
                }
            },
        },
    )
}

fn rust_declaration_symbol_name(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    name: &str,
) -> String {
    if declaration.kind() == "function_item" {
        let mut ancestor = declaration.parent();
        while let Some(node) = ancestor {
            match node.kind() {
                "function_item" => break,
                "impl_item" => {
                    if let Some(receiver) = node
                        .child_by_field_name("type")
                        .and_then(|node| rust_type_name(node, source))
                    {
                        return format!("{receiver}.{name}");
                    }
                    break;
                }
                _ => ancestor = node.parent(),
            }
        }
    }
    name.to_string()
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
    for call in
        descendants_of_kind_before(declaration, "call_expression", is_rust_declaration_boundary)
    {
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

fn is_rust_declaration_boundary(node: tree_sitter::Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "const_item"
            | "static_item"
            | "type_item"
            | "impl_item"
    )
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
