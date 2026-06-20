//! What the syntactic pass extracts from a checkout (ADR 0002, M0 scope):
//! Go declarations become Symbols with DEFINES edges; everything else is
//! a File node.

use std::collections::BTreeSet;

use yg_shard::{EdgeKind, Graph, NodeKind, SearchDoc};

/// Run the pass over an in-memory tree laid out in a tempdir, returning
/// the graph and the full-text documents it extracts.
fn pass_full(files: &[(&str, &str)]) -> (Graph, Vec<SearchDoc>) {
    let dir = tempfile::tempdir().unwrap();
    for (path, contents) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }
    yg_index::syntactic_pass(dir.path()).expect("the pass must handle a plain tree")
}

/// Just the graph, for the tests that only assert graph shape.
fn pass_over(files: &[(&str, &str)]) -> Graph {
    pass_full(files).0
}

#[test]
fn search_docs_cover_symbol_names_and_file_content() {
    let (_graph, docs) = pass_full(&[
        (
            "limit.go",
            "package svc\n\n// RateLimit throttles requests.\nfunc RateLimit() {}\n",
        ),
        ("README.md", "# Service\n\nConfigure the rate limit here.\n"),
    ]);

    // The markdown File carries its prose, so content search can reach it.
    let readme = docs
        .iter()
        .find(|d| d.node_id == "file:README.md")
        .expect("a search doc per File node");
    assert_eq!(readme.kind, NodeKind::File);
    assert!(
        readme.content.contains("rate limit"),
        "the File's content is indexed: {:?}",
        readme.content
    );

    // The Go function is searchable by its name.
    let symbol = docs
        .iter()
        .find(|d| d.node_id == "sym:limit.go#RateLimit")
        .expect("a search doc per Symbol node");
    assert_eq!(symbol.kind, NodeKind::Symbol);
    assert_eq!(symbol.name.as_deref(), Some("RateLimit"));

    // Package nodes carry no searchable text; they stay out of the index.
    assert!(
        docs.iter().all(|d| d.kind != NodeKind::Package),
        "Package nodes are not search documents"
    );
}

fn symbol_names(graph: &Graph) -> BTreeSet<&str> {
    graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Symbol)
        .map(|n| n.name.as_deref().expect("Symbols are named"))
        .collect()
}

#[test]
fn go_functions_methods_types_and_consts_become_symbols() {
    let graph = pass_over(&[(
        "widget.go",
        r#"package gadgets

const MaxWidgets = 99

type Widget struct {
	Name string
}

func (w *Widget) Render() string {
	return w.Name
}

func NewWidget(name string) Widget {
	return Widget{Name: name}
}
"#,
    )]);

    assert_eq!(
        symbol_names(&graph),
        BTreeSet::from(["MaxWidgets", "Widget", "Widget.Render", "NewWidget"]),
        "every Go declaration kind must yield a Symbol"
    );

    // Each Symbol is anchored to its file by a DEFINES edge.
    for node in graph.nodes.iter().filter(|n| n.kind == NodeKind::Symbol) {
        assert!(
            graph.edges.iter().any(|e| e.kind == EdgeKind::Defines
                && e.src == "file:widget.go"
                && e.dst == node.id),
            "{} must be DEFINES-linked to widget.go",
            node.id
        );
    }
}

#[test]
fn same_named_methods_on_different_types_stay_distinct_symbols() {
    let graph = pass_over(&[(
        "render.go",
        r#"package gadgets

type Widget struct{}

func (w Widget) Render() string { return "w" }

type Gadget struct{}

func (g Gadget) Render() string { return "g" }
"#,
    )]);

    let renders: BTreeSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Symbol)
        .filter(|n| {
            n.name
                .as_deref()
                .is_some_and(|name| name.ends_with(".Render"))
        })
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(
        renders.len(),
        2,
        "two receivers, two Render Symbols with distinct ids — got {renders:?}"
    );
}

#[test]
fn typescript_exports_imports_and_calls_become_graph_facts() {
    let graph = pass_over(&[(
        "src/widget.ts",
        r#"import { format } from "@acme/format";

export const MAX_WIDGETS = 12;

export interface Renderable {
	render(): string;
}

export type WidgetId = string;

export enum WidgetState {
	Ready,
}

export class Widget implements Renderable {
	render(): string {
		return format("widget");
	}
}

export function buildWidget(): Widget {
	return new Widget();
}
"#,
    )]);

    assert_eq!(
        symbol_names(&graph),
        BTreeSet::from([
            "MAX_WIDGETS",
            "Renderable",
            "Renderable.render",
            "Widget",
            "WidgetId",
            "Widget.render",
            "WidgetState",
            "buildWidget"
        ]),
        "TypeScript declarations must yield Symbols"
    );

    for node in graph.nodes.iter().filter(|n| n.kind == NodeKind::Symbol) {
        assert!(
            graph.edges.iter().any(|e| e.kind == EdgeKind::Defines
                && e.src == "file:src/widget.ts"
                && e.dst == node.id),
            "{} must be DEFINES-linked to src/widget.ts",
            node.id
        );
    }

    assert!(
        graph
            .nodes
            .iter()
            .any(|n| n.kind == NodeKind::Package && n.id == "pkg:@acme/format"),
        "the import source must become a Package node"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Imports
            && e.src == "file:src/widget.ts"
            && e.dst == "pkg:@acme/format"),
        "the import statement must become an IMPORTS edge"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.src == "sym:src/widget.ts#buildWidget"
            && e.dst == "sym:src/widget.ts#Widget"),
        "a call/constructor use of a repo-declared symbol must become a CALLS edge"
    );
}

#[test]
fn javascript_exports_imports_and_calls_become_graph_facts() {
    let graph = pass_over(&[(
        "src/widget.js",
        r#"import format from "@acme/format";

export const MAX_WIDGETS = 12;

export class Widget {}

export function buildWidget() {
	format(new Widget());
}
"#,
    )]);

    assert_eq!(
        symbol_names(&graph),
        BTreeSet::from(["MAX_WIDGETS", "Widget", "buildWidget"]),
        "JavaScript declarations must yield Symbols"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Imports
            && e.src == "file:src/widget.js"
            && e.dst == "pkg:@acme/format"),
        "the import statement must become an IMPORTS edge"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.src == "sym:src/widget.js#buildWidget"
            && e.dst == "sym:src/widget.js#Widget"),
        "constructor use of a repo-declared class must become a CALLS edge"
    );
}

#[test]
fn javascript_class_methods_become_symbols_and_callers() {
    let graph = pass_over(&[(
        "src/widget.js",
        r#"export class Widget {
	helper() {
		return new Widget();
	}

	buildWidget() {
		return this.helper();
	}
}
"#,
    )]);

    assert_eq!(
        symbol_names(&graph),
        BTreeSet::from(["Widget", "Widget.helper", "Widget.buildWidget"]),
        "JavaScript class methods must yield class-qualified Symbols"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.src == "sym:src/widget.js#Widget.buildWidget"
            && e.dst == "sym:src/widget.js#Widget.helper"),
        "a JavaScript method call of a repo-declared method must become a CALLS edge"
    );
    assert!(
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .all(|e| e.src != "sym:src/widget.js#Widget"),
        "the containing class must not be treated as the caller: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect::<Vec<_>>()
    );
}

#[test]
fn javascript_relative_imports_do_not_become_package_imports() {
    let graph = pass_over(&[(
        "src/widget.js",
        r#"import { helper } from "./helper.js";

export function buildWidget() {
	helper();
}
"#,
    )]);

    assert!(
        !graph.nodes.iter().any(|n| n.id == "pkg:./helper.js"),
        "relative imports are internal paths, not Package nodes"
    );
    assert!(
        graph.edges.iter().all(|e| e.dst != "pkg:./helper.js"),
        "relative imports must not produce Package IMPORTS edges: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Imports)
            .collect::<Vec<_>>()
    );
}

#[test]
fn javascript_and_typescript_module_extensions_are_indexed() {
    let graph = pass_over(&[
        (
            "src/widget.mjs",
            r#"export function buildWidget() {
	return new Widget();
}
"#,
        ),
        (
            "src/widget.cjs",
            r#"function loadWidget() {
	return buildWidget();
}
"#,
        ),
        (
            "src/widget.mts",
            r#"export class Widget {}
"#,
        ),
        (
            "src/widget.cts",
            r#"export type WidgetId = string;
"#,
        ),
    ]);

    assert_eq!(
        symbol_names(&graph),
        BTreeSet::from(["Widget", "WidgetId", "buildWidget", "loadWidget"]),
        "common JavaScript and TypeScript module extensions must be covered"
    );
}

#[test]
fn javascript_qualified_external_calls_do_not_resolve_to_same_named_local_methods() {
    let graph = pass_over(&[(
        "src/widget.js",
        r#"export class Widget {
	render() {
		return Format.render("widget");
	}
}
"#,
    )]);

    assert!(
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .all(|e| e.dst != "sym:src/widget.js#Widget.render"),
        "a qualified external call must not resolve to the same-named local method: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect::<Vec<_>>()
    );
}

#[test]
fn python_declarations_imports_and_calls_become_graph_facts() {
    let graph = pass_over(&[(
        "pkg/widget.py",
        r#"from acme.format import format_widget

MAX_WIDGETS = 12

class Widget:
    def render(self):
        return format_widget("widget")

def build_widget():
    return Widget()
"#,
    )]);

    assert_eq!(
        symbol_names(&graph),
        BTreeSet::from(["MAX_WIDGETS", "Widget", "Widget.render", "build_widget"]),
        "Python declarations must yield Symbols"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Imports
            && e.src == "file:pkg/widget.py"
            && e.dst == "pkg:acme.format"),
        "the import statement must become an IMPORTS edge"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.src == "sym:pkg/widget.py#build_widget"
            && e.dst == "sym:pkg/widget.py#Widget"),
        "constructor use of a repo-declared class must become a CALLS edge"
    );
}

#[test]
fn python_class_methods_become_symbols_and_callers() {
    let graph = pass_over(&[(
        "pkg/widget.py",
        r#"class Widget:
    def helper(self):
        return Widget()

    def build_widget(self):
        return self.helper()
"#,
    )]);

    assert_eq!(
        symbol_names(&graph),
        BTreeSet::from(["Widget", "Widget.helper", "Widget.build_widget"]),
        "Python class methods must yield class-qualified Symbols"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.src == "sym:pkg/widget.py#Widget.build_widget"
            && e.dst == "sym:pkg/widget.py#Widget.helper"),
        "a Python method call of a repo-declared method must become a CALLS edge: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect::<Vec<_>>()
    );
    assert!(
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .all(|e| e.src != "sym:pkg/widget.py#Widget"),
        "the containing class must not be treated as the caller: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect::<Vec<_>>()
    );
}

#[test]
fn python_qualified_external_calls_do_not_resolve_to_same_named_local_methods() {
    let graph = pass_over(&[(
        "pkg/widget.py",
        r#"class Widget:
    def render(self):
        return formatter.render("widget")
"#,
    )]);

    assert!(
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .all(|e| e.dst != "sym:pkg/widget.py#Widget.render"),
        "a qualified external call must not resolve to the same-named local method: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect::<Vec<_>>()
    );
}

#[test]
fn python_nested_self_attributes_do_not_resolve_as_direct_method_calls() {
    let graph = pass_over(&[(
        "pkg/widget.py",
        r#"class Widget:
    def info(self):
        pass

    def render(self):
        return self.logger.info("widget")
"#,
    )]);

    assert!(
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .all(|e| e.dst != "sym:pkg/widget.py#Widget.info"),
        "a nested self attribute call must not resolve to the same-named local method: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect::<Vec<_>>()
    );
}

#[test]
fn rust_declarations_imports_and_calls_become_graph_facts() {
    let graph = pass_over(&[(
        "src/lib.rs",
        r#"use serde::Serialize;

const MAX_WIDGETS: usize = 12;

type WidgetId = usize;

trait Renderable {
    fn render(&self) -> String;
}

struct Widget;

fn helper() -> Widget {
    Widget
}

fn build_widget() -> Widget {
    helper()
}
"#,
    )]);

    assert_eq!(
        symbol_names(&graph),
        BTreeSet::from([
            "MAX_WIDGETS",
            "Renderable",
            "Widget",
            "WidgetId",
            "helper",
            "build_widget"
        ]),
        "Rust declarations must yield Symbols"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Imports
            && e.src == "file:src/lib.rs"
            && e.dst == "pkg:serde"),
        "the use declaration must become an IMPORTS edge"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.src == "sym:src/lib.rs#build_widget"
            && e.dst == "sym:src/lib.rs#helper"),
        "a call of a repo-declared function must become a CALLS edge"
    );
}

#[test]
fn rust_impl_methods_become_symbols_and_callers() {
    let graph = pass_over(&[(
        "src/lib.rs",
        r#"struct Widget;

impl Widget {
    fn helper() -> Widget {
        Widget
    }

    fn build_widget() -> Widget {
        Widget::helper()
    }
}
"#,
    )]);

    assert_eq!(
        symbol_names(&graph),
        BTreeSet::from(["Widget", "Widget.helper", "Widget.build_widget"]),
        "Rust impl methods must yield receiver-qualified Symbols"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.src == "sym:src/lib.rs#Widget.build_widget"
            && e.dst == "sym:src/lib.rs#Widget.helper"),
        "an impl method call of a repo-declared method must become a CALLS edge: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect::<Vec<_>>()
    );
}

#[test]
fn rust_impl_methods_on_primitive_types_become_symbols() {
    let graph = pass_over(&[(
        "src/lib.rs",
        r#"trait Renderable {
    fn render(&self) -> String;
}

impl Renderable for str {
    fn render(&self) -> String {
        String::new()
    }
}
"#,
    )]);

    assert!(
        graph
            .nodes
            .iter()
            .any(|n| n.id == "sym:src/lib.rs#str.render"),
        "Rust impl methods on primitive receivers must yield receiver-qualified Symbols: {:?}",
        symbol_names(&graph)
    );
}

#[test]
fn rust_internal_use_paths_do_not_become_package_imports() {
    let graph = pass_over(&[(
        "src/lib.rs",
        r#"use crate::internal::Widget;
use self::local::Thing;
use super::parent::Other;
use crate::{grouped::Thing, nested::Other};

struct Widget;
"#,
    )]);

    assert!(
        !graph.nodes.iter().any(|n| n.kind == NodeKind::Package),
        "internal Rust module paths are not external packages: {:?}",
        graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Package)
            .collect::<Vec<_>>()
    );
    assert!(
        graph.edges.iter().all(|e| e.kind != EdgeKind::Imports),
        "internal Rust module paths must not produce IMPORTS edges: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Imports)
            .collect::<Vec<_>>()
    );
}

#[test]
fn python_relative_from_imports_do_not_become_package_imports() {
    let graph = pass_over(&[(
        "pkg/widget.py",
        r#"from .format import format_widget
from ..shared import build_widget

class Widget:
    pass
"#,
    )]);

    assert!(
        !graph.nodes.iter().any(|n| n.kind == NodeKind::Package),
        "relative Python imports are internal paths, not Package nodes: {:?}",
        graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Package)
            .collect::<Vec<_>>()
    );
    assert!(
        graph.edges.iter().all(|e| e.kind != EdgeKind::Imports),
        "relative Python imports must not produce Package IMPORTS edges: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Imports)
            .collect::<Vec<_>>()
    );
}

#[test]
fn java_this_and_super_method_calls_resolve_to_repo_methods() {
    let graph = pass_over(&[(
        "src/Widget.java",
        r#"class Base {
    void inherited() {}
}

class Widget extends Base {
    void helper() {}

    void buildWidget() {
        this.helper();
        super.inherited();
    }
}
"#,
    )]);

    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.src == "sym:src/Widget.java#Widget.buildWidget"
            && e.dst == "sym:src/Widget.java#Widget.helper"),
        "this-qualified Java method calls must resolve to repo methods: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect::<Vec<_>>()
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.src == "sym:src/Widget.java#Widget.buildWidget"
            && e.dst == "sym:src/Widget.java#Base.inherited"),
        "super-qualified Java method calls must resolve to repo methods: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect::<Vec<_>>()
    );
}

#[test]
fn java_declarations_imports_and_calls_become_graph_facts() {
    let graph = pass_over(&[(
        "src/main/java/Widget.java",
        r#"import com.acme.Format;

interface Renderable {
    String render();
}

enum WidgetState {
    READY
}

class Widget implements Renderable {
    static final int MAX_WIDGETS = 12;

    static Widget helper() {
        return new Widget();
    }

    static Widget buildWidget() {
        return helper();
    }

    public String render() {
        return Format.render("widget");
    }
}
"#,
    )]);

    assert_eq!(
        symbol_names(&graph),
        BTreeSet::from([
            "Renderable",
            "Renderable.render",
            "WidgetState",
            "Widget",
            "Widget.MAX_WIDGETS",
            "Widget.helper",
            "Widget.buildWidget",
            "Widget.render"
        ]),
        "Java declarations must yield Symbols"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Imports
            && e.src == "file:src/main/java/Widget.java"
            && e.dst == "pkg:com.acme.Format"),
        "the import declaration must become an IMPORTS edge"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.src == "sym:src/main/java/Widget.java#Widget.buildWidget"
            && e.dst == "sym:src/main/java/Widget.java#Widget.helper"),
        "a method call of a repo-declared method must become a CALLS edge"
    );
}

#[test]
fn java_class_symbols_do_not_claim_calls_from_their_methods() {
    let graph = pass_over(&[(
        "src/Widget.java",
        r#"class Widget {
    static Widget helper() {
        return new Widget();
    }

    static Widget buildWidget() {
        return helper();
    }
}
"#,
    )]);

    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.src == "sym:src/Widget.java#Widget.buildWidget"
            && e.dst == "sym:src/Widget.java#Widget.helper"),
        "the method-level CALLS edge must exist"
    );
    assert!(
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .all(|e| e.src != "sym:src/Widget.java#Widget"),
        "the containing class must not be treated as the caller: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect::<Vec<_>>()
    );
}

#[test]
fn java_qualified_external_calls_do_not_resolve_to_same_named_local_methods() {
    let graph = pass_over(&[(
        "src/Widget.java",
        r#"import com.acme.Format;

class Widget {
    String render() {
        return Format.render("widget");
    }
}
"#,
    )]);

    assert!(
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .all(|e| e.dst != "sym:src/Widget.java#Widget.render"),
        "a qualified external call must not resolve to the same-named local method: {:?}",
        graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect::<Vec<_>>()
    );
}

#[test]
fn duplicate_declarations_keep_distinct_node_ids() {
    // Multiple `func init()` per file is legal Go; their node ids must
    // not collide (the graph segment's id column is a primary key).
    let graph = pass_over(&[(
        "setup.go",
        r#"package gadgets

func init() { a() }

func init() { b() }
"#,
    )]);

    let inits: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Symbol)
        .filter(|n| n.name.as_deref() == Some("init"))
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(inits.len(), 2, "both init functions are Symbols");
    let unique: BTreeSet<&str> = graph.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(
        unique.len(),
        graph.nodes.len(),
        "node ids must be unique — got duplicates in {unique:?}"
    );
}

#[test]
fn a_method_with_an_unreadable_receiver_does_not_steal_a_type_from_elsewhere() {
    // Mid-edit code: the receiver is empty, so there is no receiver type.
    // The walk must not wander out of the receiver and pick up `Screen`.
    let graph = pass_over(&[(
        "broken.go",
        r#"package gadgets

func () Render(s Screen) string { return "x" }
"#,
    )]);

    let names = symbol_names(&graph);
    assert!(
        !names.contains("Screen.Render"),
        "the parameter type must not be mistaken for the receiver, got: {names:?}"
    );
    assert!(
        names.contains("Render"),
        "the method still yields a bare-named Symbol, got: {names:?}"
    );
}

#[test]
fn symlinks_become_file_nodes_but_are_never_read_through() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("real.go"),
        "package gadgets\n\nfunc A() {}\n",
    )
    .unwrap();
    // A symlinked .go: a File node, but its target (which could point
    // anywhere, even outside the checkout) must not be parsed.
    std::os::unix::fs::symlink("real.go", dir.path().join("link.go")).unwrap();

    let graph = yg_index::syntactic_pass(dir.path())
        .expect("symlinks must not break the pass")
        .0;

    let files: BTreeSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::File)
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(
        files,
        BTreeSet::from(["file:real.go", "file:link.go"]),
        "every tree entry, symlinks included, must be a File node"
    );
    let symbol_files: BTreeSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Symbol)
        .filter_map(|n| n.path.as_deref())
        .collect();
    assert_eq!(
        symbol_files,
        BTreeSet::from(["real.go"]),
        "symbols come only from real files, never through symlinks"
    );
}

#[test]
fn unmapped_files_become_file_nodes_without_symbols() {
    let graph = pass_over(&[
        (
            "docs/guide.md",
            "# guide\n\nfunc looks like Go but is prose\n",
        ),
        ("Makefile", "all:\n\ttrue\n"),
    ]);

    let files: BTreeSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::File)
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(
        files,
        BTreeSet::from(["file:docs/guide.md", "file:Makefile"]),
        "every file in the tree must be a File node"
    );
    assert_eq!(
        symbol_names(&graph),
        BTreeSet::new(),
        "unmapped files degrade gracefully to File nodes"
    );
}
