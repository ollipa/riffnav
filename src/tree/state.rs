use std::collections::HashSet;

use super::model::Node;

/// What a flattened row represents.
#[derive(Debug, Clone)]
pub enum RowKind {
    Dir { expanded: bool, path: String },
    File { diff_index: usize },
}

/// A single visible line in the tree: its name plus how deep it is nested.
#[derive(Debug, Clone)]
pub struct Row {
    pub depth: usize,
    pub name: String,
    pub kind: RowKind,
}

/// Flatten the tree into the rows currently visible. A directory whose path is
/// in `collapsed` is rendered but its descendants are hidden.
pub fn flatten(nodes: &[Node], collapsed: &HashSet<String>) -> Vec<Row> {
    let mut rows = Vec::new();
    walk(nodes, 0, collapsed, &mut rows);
    rows
}

fn walk(nodes: &[Node], depth: usize, collapsed: &HashSet<String>, rows: &mut Vec<Row>) {
    for node in nodes {
        match node {
            Node::Dir { name, path, children } => {
                let expanded = !collapsed.contains(path);
                rows.push(Row {
                    depth,
                    name: name.clone(),
                    kind: RowKind::Dir { expanded, path: path.clone() },
                });
                if expanded {
                    walk(children, depth + 1, collapsed, rows);
                }
            }
            Node::File { name, diff_index } => rows.push(Row {
                depth,
                name: name.clone(),
                kind: RowKind::File { diff_index: *diff_index },
            }),
        }
    }
}
