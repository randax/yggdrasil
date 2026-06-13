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
