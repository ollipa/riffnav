use super::model::Node;

/// What a flattened row represents.
#[derive(Debug, Clone, Copy)]
pub enum RowKind {
    Dir { expanded: bool },
    File { diff_index: usize },
}

/// A single visible line in the tree: its name plus how deep it is nested.
#[derive(Debug, Clone)]
pub struct Row {
    pub depth: usize,
    pub name: String,
    pub kind: RowKind,
}

/// Flatten the tree into the rows currently visible, honoring each directory's
/// `expanded` flag (collapsed directories hide their descendants).
pub fn flatten(nodes: &[Node]) -> Vec<Row> {
    let mut rows = Vec::new();
    walk(nodes, 0, &mut rows);
    rows
}

fn walk(nodes: &[Node], depth: usize, rows: &mut Vec<Row>) {
    for node in nodes {
        match node {
            Node::Dir { name, children, expanded } => {
                rows.push(Row {
                    depth,
                    name: name.clone(),
                    kind: RowKind::Dir { expanded: *expanded },
                });
                if *expanded {
                    walk(children, depth + 1, rows);
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
