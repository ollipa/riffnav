use super::*;
use crate::diff::parse;
use crate::diff::testing::header;
use crate::render::starts::tests::Repo;

/// Real code, with nothing in common from line to line.
const FUNCTION: [&str; 6] = [
    "fn load_settings(path: &Path) -> Result<Settings> {",
    "    let text = fs::read_to_string(path).context(\"reading settings\")?;",
    "    let parsed = toml::from_str(&text).context(\"parsing settings\")?;",
    "    validate_settings(&parsed).context(\"checking settings\")?;",
    "    Ok(apply_defaults(parsed))",
    "}",
];

/// A file with `context` around one added block. The first line of each is
/// numbered from 1.
fn file(name: &str, context: &[&str], added: &[&str]) -> String {
    let mut raw = format!(
        "{}@@ -1,{} +1,{} @@\n",
        header(name),
        context.len(),
        context.len() + added.len(),
    );
    for line in context {
        raw.push_str(&format!(" {line}\n"));
    }
    for line in added {
        raw.push_str(&format!("+{line}\n"));
    }
    raw
}

fn notes_in(raw: &str) -> Vec<(usize, String)> {
    let files = parse(raw);
    let moves = MoveIndex::build(&files);
    let index = build(&files, &moves, None, &AtomicBool::new(false)).unwrap();
    let notes = index.notes().map(|(_, row, note)| (row, note.to_string()));
    notes.collect()
}

fn fires(raw: &str) -> bool {
    !notes_in(raw).is_empty()
}

#[test]
fn a_block_copied_within_a_file_names_the_line() {
    let got = notes_in(&file("a.rs", &FUNCTION, &FUNCTION));
    // The hunk header is row 0.
    assert_eq!(got, [(7, "copied from line 1".to_string())]);
}

#[test]
fn a_block_copied_from_another_file_names_the_file() {
    let raw = file("a.rs", &FUNCTION, &[]) + &file("b.rs", &["kept"], &FUNCTION);
    assert_eq!(notes_in(&raw)[0].1, "copied from a.rs:1");
}

#[test]
fn indentation_doesnt_stop_a_match() {
    let indented = FUNCTION.map(|line| format!("\t\t{line}"));
    let indented: Vec<&str> = indented.iter().map(String::as_str).collect();
    assert!(fires(&file("a.rs", &FUNCTION, &indented)));
}

#[test]
fn a_blank_line_lost_on_the_way_doesnt_stop_a_match() {
    let mut spaced = FUNCTION.to_vec();
    spaced.insert(3, "");
    assert!(fires(&file("a.rs", &spaced, &FUNCTION)));
}

#[test]
fn a_moved_block_is_a_move_even_with_a_copy_left_behind() {
    let body = FUNCTION
        .iter()
        .map(|l| format!("-{l}\n"))
        .collect::<String>();
    let kept = FUNCTION
        .iter()
        .map(|l| format!(" {l}\n"))
        .collect::<String>();
    let added = FUNCTION
        .iter()
        .map(|l| format!("+{l}\n"))
        .collect::<String>();
    let raw = format!(
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,12 +1,12 @@\n{kept}{body}{added}"
    );
    assert!(!fires(&raw));
}

#[test]
fn a_removed_block_is_no_source() {
    let raw = format!(
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,6 +1,0 @@\n{}",
        FUNCTION
            .iter()
            .map(|l| format!("-{l}\n"))
            .collect::<String>()
    ) + &file("b.rs", &["kept"], &FUNCTION[..4]);
    // That's a move of what it could match, and nothing is left for a copy.
    assert!(!fires(&raw));
}

#[test]
fn a_short_block_isnt_a_copy() {
    assert!(!fires(&file("a.rs", &FUNCTION, &FUNCTION[..3])));
}

#[test]
fn lines_must_add_up_to_enough_code() {
    let short = [
        "f_one(1);",
        "g_two(2);",
        "h_tri(3);",
        "i_for(4);",
        "j_fiv(5);",
    ];
    assert!(!fires(&file("a.rs", &short, &short)));
}

#[test]
fn every_kind_of_filler_stays_quiet() {
    let cases: [&[&str]; 8] = [
        &[
            "// Copyright 2024 The Project Authors. All rights reserved.",
            "// Licensed under the Apache License, Version 2.0 (the License);",
            "// you may not use this file except in compliance with the License.",
            "// You may obtain a copy of the License at the address below.",
            "// Unless required by applicable law (or agreed to in writing),",
        ],
        &[
            "use std::collections::{BTreeMap, HashMap, HashSet};",
            "use std::io::{BufRead, BufReader, Read, Write};",
            "use std::path::{Path, PathBuf};",
            "use std::process::{Child, ChildStdin, ChildStdout};",
            "use crate::comment::store::{Anchor, Side};",
        ],
        &[
            "import { readFile, writeFile } from \"node:fs/promises\";",
            "import { resolve, join, dirname } from \"node:path\";",
            "import type { Settings, Options } from \"./settings.js\";",
            "export { Parser, Lexer, Scanner } from \"./parser.js\";",
            "const pieces = require(\"./pieces-of-the-program\");",
        ],
        &[
            "Color::Black => write!(f, \"black\"),",
            "Color::Green => write!(f, \"green\"),",
            "Color::Yellow => write!(f, \"yellow\"),",
            "Color::Purple => write!(f, \"purple\"),",
            "Color::Orange => write!(f, \"orange\"),",
        ],
        &[
            "pub first_field_name: Vec<String>,",
            "pub second_field_name: Option<u32>,",
            "pub third_field_name: HashMap<u32, u32>,",
            "pub fourth_field_name: Arc<Mutex<u32>>,",
            "pub fifth_field_name: PathBuf,",
        ],
        &[
            "(\"j / k\", \"move selection / scroll diff (per focus)\"),",
            "(\"g / G\", \"go to the top or the bottom of the diff\"),",
            "(\"Ctrl-d\", \"half a page down, where the focus is\"),",
            "(\"Ctrl-u\", \"half a page up, where the focus is\"),",
            "(\"PgDn\", \"a page down, where the focus is\"),",
        ],
        &[
            "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
            "#[serde(rename_all = \"kebab-case\")]",
            "#[cfg(all(test, feature = \"integration\"))]",
            "#[allow(clippy::too_many_arguments)]",
            "#[must_use = \"what it returns is the whole point\"]",
        ],
        &[
            "- wifiMacs (string): The wifi addresses of the devices.",
            "- serials (string): The serial numbers of the devices.",
            "- scope (string): The scope of the devices, one of many.",
            "- timespan (number): The timespan to fetch the data for.",
            "- resolution (integer): The resolution of the data, in seconds.",
        ],
    ];
    for case in cases {
        assert!(!fires(&file("a.rs", case, case)), "{case:?}");
    }
}

#[test]
fn a_table_of_calls_stays_quiet() {
    let table = [
        "self.organizations = ActionBatchOrganizations()",
        "self.networks = ActionBatchNetworks()",
        "self.devices = ActionBatchDevices()",
        "self.appliance = ActionBatchAppliance()",
        "self.switches = ActionBatchSwitches()",
        "self.wireless = ActionBatchWireless()",
    ];
    assert!(!fires(&file("a.py", &table, &table)));
}

#[test]
fn a_rust_test_module_stays_quiet() {
    let mut lines = vec!["#[cfg(test)]", "mod tests {"];
    lines.extend(FUNCTION);
    assert!(!fires(&file("a.rs", &FUNCTION, &lines)));
    let raw = format!(
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,6 +1,12 @@ mod tests {{\n{}{}",
        FUNCTION
            .iter()
            .map(|l| format!(" {l}\n"))
            .collect::<String>(),
        FUNCTION
            .iter()
            .map(|l| format!("+{l}\n"))
            .collect::<String>(),
    );
    assert!(!fires(&raw));
}

#[test]
fn what_isnt_hand_written_source_stays_quiet() {
    for path in [
        "Cargo.lock",
        "package-lock.json",
        "README.md",
        "config.yaml",
        "tests/fixtures/a.rs",
        "src/testdata/a.go",
        "src/__snapshots__/a.js",
        "vendor/lib/a.go",
        "node_modules/x/a.js",
        "api/client.pb.go",
        "api/client_pb2.py",
        "dist/app.min.js",
        "db/migrations/0001_init.ts",
        "tests/test_a.py",
        "src/a.test.ts",
        "pkg/a_test.go",
    ] {
        assert!(!fires(&file(path, &FUNCTION, &FUNCTION)), "{path}");
    }
}

#[test]
fn a_file_that_says_its_generated_stays_quiet() {
    let mut lines = vec!["// Code generated by protoc. DO NOT EDIT."];
    lines.extend(FUNCTION);
    assert!(!fires(&file("a.rs", &lines, &FUNCTION)));
}

#[test]
fn a_source_in_a_file_thats_only_data_doesnt_count() {
    let raw = file("a.json", &FUNCTION, &[]) + &file("b.rs", &["kept"], &FUNCTION);
    assert!(!fires(&raw));
}

#[test]
fn copies_inside_an_added_block_are_no_copies() {
    let mut twice = FUNCTION.to_vec();
    twice.extend(FUNCTION);
    assert!(!fires(&file("a.rs", &["kept"], &twice)));
}

#[test]
fn a_line_found_all_over_proves_nothing() {
    // A line in six places can't count, which leaves the block a line short.
    let common = FUNCTION[1];
    let mut context = vec![common; 6];
    context.extend(&FUNCTION[..4]);
    assert!(fires(&file("a.rs", &FUNCTION[..4], &FUNCTION[..4])));
    assert!(!fires(&file("a.rs", &context, &FUNCTION[..4])));
}

#[test]
fn a_block_found_all_over_is_a_pattern() {
    let mut context = Vec::new();
    for _ in 0..=MAX_BLOCK_PLACES {
        context.extend(FUNCTION);
        context.push("between_the_copies();");
    }
    assert!(!fires(&file("a.rs", &context, &FUNCTION)));
}

#[test]
fn a_block_cant_span_a_change_in_its_source() {
    let raw = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,4 +1,5 @@\n";
    let mut raw = raw.to_string();
    for line in &FUNCTION[..2] {
        raw.push_str(&format!(" {line}\n"));
    }
    raw.push_str("+an_edit_right_here(1);\n");
    for line in &FUNCTION[2..4] {
        raw.push_str(&format!(" {line}\n"));
    }
    raw += &file("b.rs", &["kept"], &FUNCTION[..4]);
    assert!(!fires(&raw));
}

#[test]
fn a_copy_jumps_to_a_source_the_diff_shows_and_back() {
    let raw = file("a.rs", &FUNCTION, &[]) + &file("b.rs", &["kept"], &FUNCTION);
    let files = parse(&raw);
    let moves = MoveIndex::build(&files);
    let index = build(&files, &moves, None, &AtomicBool::new(false)).unwrap();
    let new = |line| Anchor {
        side: Side::New,
        line,
    };
    let there = Jump::Copied(Target {
        file: 0,
        anchor: new(2),
    });
    assert_eq!(index.file(1).jump(new(3)), Some(&there));
    let back = Jump::Copied(Target {
        file: 1,
        anchor: new(3),
    });
    assert_eq!(index.file(0).jump(new(2)), Some(&back));
}

#[test]
fn a_huge_diff_isnt_tried() {
    let mut files = parse(&file("a.rs", &FUNCTION, &FUNCTION));
    assert!(worth_trying(&files));
    files[0].additions = MAX_CHANGED_LINES as u32 + 1;
    assert!(!worth_trying(&files));
}

/// `lines` with a filler function above them, so it's more than the block.
fn source_file(lines: &[&str]) -> String {
    let padding: String = (0..40)
        .map(|at| format!("const C{at}: u32 = {at};\n"))
        .collect();
    padding + &lines.join("\n") + "\n"
}

fn git_notes(repo: &Repo, args: &[&str], tree: Option<Tree>) -> Vec<String> {
    let out = repo.run(args);
    let files = parse(&String::from_utf8_lossy(&out.stdout));
    let moves = MoveIndex::build(&files);
    let git = Git {
        repo: repo.0.clone(),
        tree,
    };
    let index = build(&files, &moves, Some(&git), &AtomicBool::new(false)).unwrap();
    index.notes().map(|(_, _, note)| note.to_string()).collect()
}

/// A merge of two edits to line 2 of `old`, resolved as `merged`.
fn merged(old: &str, merged: &str) -> Repo {
    let repo = Repo::new();
    repo.commit("a.rs", old);
    repo.git(&["checkout", "-qb", "side"]);
    repo.commit("a.rs", &old.replacen("\n", "\n// side\n", 1));
    repo.git(&["checkout", "-q", "-"]);
    repo.commit("a.rs", &old.replacen("\n", "\n// main\n", 1));
    repo.run(&["merge", "side"]);
    repo.write("a.rs", merged);
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "merge"]);
    repo
}

#[test]
fn a_merge_has_only_its_context_to_find_a_source_in() {
    let top = "// top\n// bottom\n".to_string();
    let pad = "const PAD: u32 = 1;\n".repeat(30);
    let block = FUNCTION.join("\n") + "\n";
    // Outside the hunk it takes the blob to see it, and a merge has no one blob.
    let old = format!("{top}{pad}{block}");
    let new = format!("// top\n// both\n// bottom\n{pad}{block}\n{block}");
    let repo = merged(&old, &new);
    let got = git_notes(&repo, &["show", "HEAD"], None);
    assert!(got.is_empty(), "{got:?}");

    // In the hunk it's context, which is there to see.
    let raw = "diff --cc a.rs\nindex 1,2..3\n--- a/a.rs\n+++ b/a.rs\n@@@ -1,12 -1,12 +1,24 @@@\n";
    let mut raw = raw.to_string();
    for line in FUNCTION {
        raw.push_str(&format!("  {line}\n"));
    }
    for line in FUNCTION {
        raw.push_str(&format!("++{line}\n"));
    }
    let got: Vec<String> = notes_in(&raw).into_iter().map(|(_, note)| note).collect();
    assert_eq!(got, ["copied from line 1"]);
}

#[test]
fn the_old_blob_finds_a_source_outside_the_hunks() {
    let repo = Repo::new();
    let old = source_file(&FUNCTION) + &"const PAD: u32 = 1;\n".repeat(30);
    repo.commit("a.rs", &old);
    let new = old.clone() + "\n" + &FUNCTION.join("\n") + "\n";
    repo.commit("a.rs", &new);
    let got = git_notes(&repo, &["show", "HEAD"], None);
    assert_eq!(got, ["copied from line 41"]);
}

#[test]
fn line_numbers_follow_the_edits_above_the_source() {
    let repo = Repo::new();
    let tail = "const PAD: u32 = 1;\n".repeat(30);
    let old = "const TOP: u32 = 1;\n".to_string() + &source_file(&FUNCTION) + &tail;
    repo.commit("a.rs", &old);
    // Two lines added at the top move the source down by two.
    let new = "const ONE: u32 = 1;\nconst TWO: u32 = 2;\n".to_string()
        + &old
        + "\n"
        + &FUNCTION.join("\n")
        + "\n";
    repo.commit("a.rs", &new);
    let got = git_notes(&repo, &["show", "HEAD"], None);
    assert_eq!(got, ["copied from line 44"]);
}

#[test]
fn a_line_the_diff_removed_is_no_source_in_the_old_blob_either() {
    let repo = Repo::new();
    repo.commit("a.rs", &source_file(&FUNCTION));
    let new = source_file(&[]) + "\n" + &FUNCTION.join("\n") + "\n";
    repo.write("b.rs", &source_file(&[]));
    repo.commit("a.rs", &new);
    assert!(git_notes(&repo, &["show", "HEAD"], None).is_empty());
}

#[test]
fn git_grep_finds_a_source_in_a_file_the_diff_leaves_alone() {
    let repo = Repo::new();
    repo.commit("src/lib/untouched.rs", &source_file(&FUNCTION));
    repo.commit("src/new.rs", &FUNCTION.join("\n"));
    let tree = Some(Tree::Rev("HEAD".to_string()));
    let got = git_notes(&repo, &["show", "HEAD"], tree);
    assert_eq!(got, ["copied from src/lib/untouched.rs:41"]);
}

#[test]
fn without_a_tree_the_rest_of_the_repository_isnt_read() {
    let repo = Repo::new();
    repo.commit("src/lib/untouched.rs", &source_file(&FUNCTION));
    repo.commit("src/new.rs", &FUNCTION.join("\n"));
    assert!(git_notes(&repo, &["show", "HEAD"], None).is_empty());
}

#[test]
fn the_index_stands_in_for_the_tree_of_a_working_diff() {
    let repo = Repo::new();
    repo.commit("src/untouched.rs", &source_file(&[]));
    repo.commit("src/new.rs", "const A: u32 = 1;\n");
    // Only staged, so the source isn't in any commit.
    repo.write("src/untouched.rs", &source_file(&FUNCTION));
    repo.git(&["add", "-A"]);
    repo.write(
        "src/new.rs",
        &format!("const A: u32 = 1;\n{}\n", FUNCTION.join("\n")),
    );
    let got = git_notes(&repo, &["diff"], Some(Tree::Index));
    assert_eq!(got, ["copied from src/untouched.rs:41"]);
}

#[test]
fn a_file_found_with_a_common_line_isnt_read_for_it() {
    let repo = Repo::new();
    // In every file, so `git grep` finds it far too often to mean anything.
    for at in 0..8 {
        repo.write(&format!("src/other{at}.rs"), &source_file(&FUNCTION));
    }
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-qm", "x"]);
    repo.commit("src/new.rs", &FUNCTION.join("\n"));
    let tree = Some(Tree::Rev("HEAD".to_string()));
    assert!(git_notes(&repo, &["show", "HEAD"], tree).is_empty());
}

#[test]
fn a_path_with_spaces_can_be_read() {
    let repo = Repo::new();
    repo.commit("src/some dir/un touched.rs", &source_file(&FUNCTION));
    repo.commit("src/new.rs", &FUNCTION.join("\n"));
    let tree = Some(Tree::Rev("HEAD".to_string()));
    let got = git_notes(&repo, &["show", "HEAD"], tree);
    assert_eq!(got, ["copied from src/some dir/un touched.rs:41"]);
}

/// What `git show` finds for a copy of an outside file, under each given name.
fn notes_with_origins(names: &[&str], tree: Tree) -> Vec<String> {
    let repo = Repo::new();
    for name in names {
        let at = repo.0.join(path::to_os(name));
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, source_file(&FUNCTION)).unwrap();
    }
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-qm", "x"]);
    repo.commit("src/new.rs", &FUNCTION.join("\n"));
    git_notes(&repo, &["show", "HEAD"], Some(tree))
}

#[test]
fn a_quoted_name_can_be_read_and_shows_as_itself() {
    let head = || Tree::Rev("HEAD".to_string());
    assert_eq!(
        notes_with_origins(&["日本語/ファイル.rs"], head()),
        ["copied from 日本語/ファイル.rs:41"]
    );
    assert_eq!(
        notes_with_origins(&["t\tb.rs"], head()),
        [r"copied from t\tb.rs:41"]
    );
    let latin1 = path::from_bytes(b"caf\xe9.rs");
    assert_eq!(
        notes_with_origins(&[&latin1], head()),
        [r"copied from caf\351.rs:41"]
    );
}

#[test]
fn a_name_like_an_index_stage_is_read_from_the_index() {
    assert_eq!(
        notes_with_origins(&["1:x.rs"], Tree::Index),
        ["copied from 1:x.rs:41"]
    );
}

#[test]
fn a_name_with_a_newline_cant_be_mistaken_for_another_file() {
    let head = || Tree::Rev("HEAD".to_string());
    let lone = notes_with_origins(&["a\nb.rs"], head());
    assert!(lone.is_empty(), "{lone:?}");
    // The other answers from `git cat-file` stay in step without it.
    assert_eq!(
        notes_with_origins(&["a\nb.rs", "z.rs"], head()),
        ["copied from z.rs:41"]
    );
}

#[test]
fn crlf_doesnt_stop_a_match() {
    let repo = Repo::new();
    let crlf = source_file(&FUNCTION).replace('\n', "\r\n");
    repo.commit("src/untouched.rs", &crlf);
    repo.commit("src/new.rs", &FUNCTION.join("\n"));
    let tree = Some(Tree::Rev("HEAD".to_string()));
    let got = git_notes(&repo, &["show", "HEAD"], tree);
    assert_eq!(got, ["copied from src/untouched.rs:41"]);
}

#[test]
fn a_renamed_file_is_the_source_under_its_new_name() {
    let repo = Repo::new();
    let old = source_file(&FUNCTION) + &"const PAD: u32 = 1;\n".repeat(60);
    repo.commit("a.rs", &old);
    repo.git(&["mv", "a.rs", "b.rs"]);
    let new = old.clone() + "\n" + &FUNCTION.join("\n") + "\n";
    repo.commit("b.rs", &new);
    let got = git_notes(&repo, &["show", "-M", "HEAD"], None);
    assert_eq!(got, ["copied from line 41"]);
}

#[test]
fn a_renamed_files_old_path_isnt_read_from_the_index() {
    let repo = Repo::new();
    let old = source_file(&FUNCTION) + &"const PAD: u32 = 1;\n".repeat(60);
    repo.commit("src/a.rs", &old);
    repo.commit("src/c.rs", "const A: u32 = 1;\n");
    repo.write(
        "src/c.rs",
        &format!("const A: u32 = 1;\n{}\n", FUNCTION.join("\n")),
    );
    // A rename in the work tree alone, with the old path still in the index,
    // and the source removed in the new file: only the index has it now.
    std::fs::rename(repo.0.join("src/a.rs"), repo.0.join("src/b.rs")).unwrap();
    repo.git(&["add", "-N", "src/b.rs"]);
    repo.write(
        "src/b.rs",
        &(source_file(&[]) + &"const PAD: u32 = 1;\n".repeat(60)),
    );
    let got = git_notes(&repo, &["diff", "-M", "HEAD"], Some(Tree::Index));
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn a_diff_with_spaces_ignored_still_numbers_the_source() {
    let repo = Repo::new();
    let old = "const TOP: u32 = 1;\n".to_string()
        + &source_file(&FUNCTION)
        + &"const PAD: u32 = 1;\n".repeat(30);
    repo.commit("a.rs", &old);
    let new = old.replace("const TOP", "  const   TOP") + "\n" + &FUNCTION.join("\n") + "\n";
    repo.commit("a.rs", &new);
    let got = git_notes(&repo, &["show", "-w", "HEAD"], None);
    assert_eq!(got, ["copied from line 42"]);
}

#[test]
fn a_blob_that_isnt_what_the_diff_shows_is_left_alone() {
    let repo = Repo::new();
    repo.commit("a.rs", &source_file(&FUNCTION));
    repo.commit("a.rs", &(source_file(&FUNCTION) + &FUNCTION.join("\n")));
    let out = repo.run(&["show", "HEAD"]);
    // The same diff with a context line that isn't what the blob has.
    let text = String::from_utf8_lossy(&out.stdout).replace(
        "     Ok(apply_defaults(parsed))",
        "     Ok(something_else(parsed))",
    );
    let files = parse(&text);
    let moves = MoveIndex::build(&files);
    let git = Git {
        repo: repo.0.clone(),
        tree: None,
    };
    let index = build(&files, &moves, Some(&git), &AtomicBool::new(false)).unwrap();
    // The context is too short to find the source in, so nothing is claimed.
    assert_eq!(index.notes().count(), 0);
}

#[test]
fn cancelling_stops_it() {
    let repo = Repo::new();
    repo.commit("a.rs", &source_file(&FUNCTION));
    repo.commit("a.rs", &(source_file(&FUNCTION) + &FUNCTION.join("\n")));
    let out = repo.run(&["show", "HEAD"]);
    let files = parse(&String::from_utf8_lossy(&out.stdout));
    let git = Git {
        repo: repo.0.clone(),
        tree: None,
    };
    let moves = MoveIndex::build(&files);
    assert!(build(&files, &moves, Some(&git), &AtomicBool::new(true)).is_none());
}
