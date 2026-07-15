use yg_shard::Graph;

use crate::resolve::{SimpleCall, SimpleExtractionCtx, SimpleImport, site};

use super::{
    ExtractedFacts, SimpleLanguage, SimpleWalk, descendants_of_kind, extract_simple_language,
    field_text, mint_symbol, simple_expression_name,
};

pub(super) fn extract_java(
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
