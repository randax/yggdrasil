use std::collections::HashMap;

use yg_shard::Graph;

use crate::resolve::{SimpleCall, SimpleExtractionCtx, SimpleFileFacts, SimpleImport, site};

use super::{
    ExtractedFacts, SimpleLanguage, SimpleWalk, descendants_of_kind, extract_simple_language,
    field_text, first_of_kind, mint_symbol,
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
