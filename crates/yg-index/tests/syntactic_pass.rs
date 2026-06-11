//! What the syntactic pass extracts from a checkout (ADR 0002, M0 scope):
//! Go declarations become Symbols with DEFINES edges; everything else is
//! a File node.

use std::collections::BTreeSet;

use yg_shard::{EdgeKind, Graph, NodeKind};

/// Run the pass over an in-memory tree laid out in a tempdir.
fn pass_over(files: &[(&str, &str)]) -> Graph {
    let dir = tempfile::tempdir().unwrap();
    for (path, contents) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }
    yg_index::syntactic_pass(dir.path()).expect("the pass must handle a plain tree")
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
fn non_go_files_become_file_nodes_without_symbols() {
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
        "nothing outside the Go grammar may produce Symbols in M0"
    );
}
