use std::collections::HashMap;

use yg_shard::Graph;

use crate::resolve::{
    SimpleCall, SimpleExtractionCtx, SimpleFileFacts, SimpleImport, SimpleLanguageTag, site,
};

use super::{
    ExtractedFacts, SimpleLanguage, descendants_of_kind, descendants_of_kind_before,
    extract_simple_language, field_text, first_of_kind, mint_symbol,
};

pub(super) fn extract_python(
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
        SimpleLanguageTag::Python,
        SimpleLanguage {
            imports: extract_python_imports,
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
    match declaration.kind() {
        "class_definition" => {
            let Some(name) = field_text(declaration, "name", source) else {
                return;
            };
            let id = mint_symbol(path, file_id, name, id_uses, graph);
            facts.declarations.push((name.to_string(), id));
        }
        "function_definition" => {
            let Some(name) = field_text(declaration, "name", source) else {
                return;
            };
            let symbol_name = python_function_symbol_name(declaration, source, name);
            let id = mint_symbol(path, file_id, &symbol_name, id_uses, graph);
            facts.declarations.push((name.to_string(), id.clone()));
            collect_python_calls(declaration, source, &id, path, &mut facts.calls);
        }
        "assignment" => {
            if has_python_declaration_ancestor(declaration) {
                return;
            }
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

fn python_function_symbol_name(
    function: tree_sitter::Node<'_>,
    source: &[u8],
    name: &str,
) -> String {
    let mut ancestor = function.parent();
    while let Some(node) = ancestor {
        match node.kind() {
            "function_definition" | "lambda" => break,
            "class_definition" => {
                if let Some(class_name) = field_text(node, "name", source) {
                    return format!("{class_name}.{name}");
                }
                break;
            }
            _ => ancestor = node.parent(),
        }
    }
    name.to_string()
}

fn has_python_declaration_ancestor(node: tree_sitter::Node<'_>) -> bool {
    let mut ancestor = node.parent();
    while let Some(node) = ancestor {
        if matches!(node.kind(), "function_definition" | "class_definition") {
            return true;
        }
        ancestor = node.parent();
    }
    false
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
    for call in descendants_of_kind_before(declaration, "call", |node| {
        matches!(node.kind(), "function_definition" | "class_definition")
    }) {
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
