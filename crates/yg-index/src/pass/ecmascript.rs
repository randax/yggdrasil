use std::collections::HashMap;

use yg_shard::Graph;

use crate::resolve::{
    SimpleCall, SimpleExtractionCtx, SimpleFileFacts, SimpleImport, SimpleLanguageTag, site,
};

use super::{
    ExtractedFacts, SimpleLanguage, descendants_of_kind, descendants_of_kind_before,
    extract_simple_language, field_text, mint_symbol, simple_expression_name,
};

pub(super) fn extract_typescript(
    root: tree_sitter::Node<'_>,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<ExtractedFacts> {
    extract_ecmascript(
        root,
        path,
        file_id,
        source,
        graph,
        SimpleLanguageTag::TypeScript,
    )
}

pub(super) fn extract_tsx(
    root: tree_sitter::Node<'_>,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<ExtractedFacts> {
    extract_ecmascript(root, path, file_id, source, graph, SimpleLanguageTag::Tsx)
}

pub(super) fn extract_javascript(
    root: tree_sitter::Node<'_>,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<ExtractedFacts> {
    extract_ecmascript(
        root,
        path,
        file_id,
        source,
        graph,
        SimpleLanguageTag::JavaScript,
    )
}

fn extract_ecmascript(
    root: tree_sitter::Node<'_>,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
    language: SimpleLanguageTag,
) -> Option<ExtractedFacts> {
    extract_simple_language(
        root,
        path,
        file_id,
        source,
        graph,
        language,
        SimpleLanguage {
            imports: extract_ecmascript_imports,
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
            let mut cursor = declaration.walk();
            for declarator in declaration
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "variable_declarator")
            {
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
    for call in descendants_of_kind_before(
        declaration,
        "call_expression",
        is_ecmascript_declaration_boundary,
    ) {
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
    for call in descendants_of_kind_before(
        declaration,
        "new_expression",
        is_ecmascript_declaration_boundary,
    ) {
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

fn is_ecmascript_declaration_boundary(node: tree_sitter::Node<'_>) -> bool {
    match node.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "type_alias_declaration"
        | "enum_declaration"
        | "class_declaration"
        | "abstract_class_declaration"
        | "method_definition" => true,
        "variable_declarator" => node
            .child_by_field_name("name")
            .is_some_and(|name| name.kind() == "identifier"),
        _ => false,
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
