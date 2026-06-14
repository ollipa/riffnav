use crate::diff::FileDiff;

/// A node in the file tree. Directories carry their full path (used as the key
/// for expand/collapse state); files point back at their index in `Vec<FileDiff>`.
#[derive(Debug)]
pub enum Node {
    Dir {
        name: String,
        path: String,
        children: Vec<Node>,
    },
    File {
        name: String,
        diff_index: usize,
    },
}

/// Build a sorted directory tree from the changed files' paths.
pub fn build(files: &[FileDiff]) -> Vec<Node> {
    let mut roots = Vec::new();
    for (idx, file) in files.iter().enumerate() {
        let parts: Vec<&str> = file.path().split('/').filter(|p| !p.is_empty()).collect();
        insert(&mut roots, &parts, idx, "");
    }
    sort(&mut roots);
    roots
}

fn insert(nodes: &mut Vec<Node>, parts: &[&str], diff_index: usize, parent: &str) {
    match parts {
        [] => {}
        [name] => nodes.push(Node::File {
            name: (*name).to_string(),
            diff_index,
        }),
        [head, tail @ ..] => {
            let path = if parent.is_empty() {
                (*head).to_string()
            } else {
                format!("{parent}/{head}")
            };
            let pos = nodes
                .iter()
                .position(|n| matches!(n, Node::Dir { name, .. } if name == head));
            let dir = match pos {
                Some(p) => p,
                None => {
                    nodes.push(Node::Dir {
                        name: (*head).to_string(),
                        path: path.clone(),
                        children: Vec::new(),
                    });
                    nodes.len() - 1
                }
            };
            if let Node::Dir { children, .. } = &mut nodes[dir] {
                insert(children, tail, diff_index, &path);
            }
        }
    }
}

/// Directories first, then files, alphabetical within each group.
fn sort(nodes: &mut [Node]) {
    nodes.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| name(a).cmp(name(b))));
    for node in nodes.iter_mut() {
        if let Node::Dir { children, .. } = node {
            sort(children);
        }
    }
}

fn rank(node: &Node) -> u8 {
    match node {
        Node::Dir { .. } => 0,
        Node::File { .. } => 1,
    }
}

fn name(node: &Node) -> &str {
    match node {
        Node::Dir { name, .. } | Node::File { name, .. } => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::FileStatus;

    fn file(path: &str) -> FileDiff {
        FileDiff {
            old_path: None,
            new_path: Some(path.to_string()),
            status: FileStatus::Added,
            additions: 0,
            deletions: 0,
            raw: String::new(),
        }
    }

    #[test]
    fn nests_and_sorts_dirs_before_files() {
        let files = vec![
            file("README.md"),
            file("src/main.rs"),
            file("src/diff/parser.rs"),
        ];
        let roots = build(&files);
        assert!(matches!(&roots[0], Node::Dir { name, .. } if name == "src"));
        assert!(matches!(&roots[1], Node::File { name, .. } if name == "README.md"));
    }

    #[test]
    fn dirs_carry_full_paths() {
        let files = vec![file("src/diff/parser.rs")];
        let roots = build(&files);
        let Node::Dir { path, children, .. } = &roots[0] else {
            panic!("expected src dir");
        };
        assert_eq!(path, "src");
        let Node::Dir { path, .. } = &children[0] else {
            panic!("expected src/diff dir");
        };
        assert_eq!(path, "src/diff");
    }
}
