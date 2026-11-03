//! `gt history`: versions of a branch's commit from its reflog, comparing
//! them, and reverting to an earlier one.

mod common;

use common::World;
use serde_json::Value;

fn history_json(w: &World, args: &[&str]) -> Value {
    let mut full = vec!["history", "--json"];
    full.extend(args);
    serde_json::from_str(&w.gt_ok(&full).stdout).unwrap()
}

fn versions(v: &Value) -> Vec<(u64, bool)> {
    v["versions"].as_array().unwrap().iter().map(|v| (v["version"].as_u64().unwrap(), v["current"] == true)).collect()
}

/// feat-a with three versions of a.txt (v1, v2, v3), and feat-b on top.
fn three_versions() -> World {
    let w = World::new();
    w.create("feat-a", "a.txt", "v1\n");
    w.create("feat-b", "b.txt", "b\n");
    w.gt_ok(&["down"]);
    w.write("a.txt", "v2\n");
    w.gt_ok(&["modify", "-a"]);
    w.write("a.txt", "v3\n");
    w.write("extra.txt", "x\n");
    w.gt_ok(&["modify", "-a"]);
    w
}

#[test]
fn history_lists_each_amend_as_a_version() {
    let w = three_versions();
    let v = history_json(&w, &[]);
    assert_eq!(v["branch"], "feat-a");
    assert_eq!(v["parent"], "main");
    assert_eq!(v["tip"], w.head("feat-a").unwrap().as_str());
    // Newest first.
    assert_eq!(versions(&v), vec![(3, true), (2, false), (1, false)]);
    let actions: Vec<&str> = v["versions"].as_array().unwrap().iter().map(|v| v["action"].as_str().unwrap()).collect();
    assert_eq!(actions, vec!["amend", "amend", "commit"]);
    // p3 changed a.txt and added extra.txt over p2 (.txt files count as docs).
    assert_eq!(v["versions"][0]["delta"]["vs"], 2);
    assert_eq!(v["versions"][0]["delta"]["total"]["doc"], serde_json::json!({ "add": 2, "del": 1 }));

    let text = w.gt_ok(&["history"]);
    assert!(text.stdout.contains("p3*"), "{text:?}");
    assert!(text.stdout.contains("p1 "), "{text:?}");
    let text = w.gt_ok(&["h", "feat-a"]);
    assert!(text.stdout.contains("p3*"), "{text:?}");
}

#[test]
fn a_restack_is_not_a_new_version() {
    let w = three_versions();
    // feat-b was restacked twice, but its patch never changed.
    let v = history_json(&w, &["feat-b"]);
    assert_eq!(versions(&v), vec![(1, true)]);
    let moves = v["versions"][0]["moves"].as_array().unwrap();
    assert_eq!(moves.len(), 3, "{v:#}");
    assert_eq!(moves.last().unwrap()["action"], "restack");
}

#[test]
fn compare_defaults_to_previous_against_current() {
    let w = three_versions();
    let out = w.gt_ok(&["history", "compare", "--json"]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["a"]["label"], "p2");
    assert_eq!(v["b"]["label"], "p3");
    let mut paths: Vec<&str> = v["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    paths.sort();
    assert_eq!(paths, vec!["a.txt", "extra.txt"]);

    let out = w.gt_ok(&["history", "compare", "p1", "p2"]);
    assert!(out.stdout.contains("-v1"), "{out:?}");
    assert!(out.stdout.contains("+v2"), "{out:?}");
}

#[test]
fn show_is_a_version_against_its_own_base() {
    let w = three_versions();
    let out = w.gt_ok(&["history", "show", "1", "--json"]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["a"]["label"], "base");
    assert_eq!(v["b"]["label"], "p1");
    let paths: Vec<&str> = v["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec!["a.txt"]);
}

#[test]
fn compare_on_a_first_version_explains_itself() {
    let w = World::new();
    w.create("feat-a", "a.txt", "v1\n");
    let out = w.gt_err(&["history", "compare"]);
    assert!(out.stderr.contains("first version"), "{out:?}");
    let out = w.gt_err(&["history", "show", "p7"]);
    assert!(out.stderr.contains("has no p7"), "{out:?}");
}

#[test]
fn revert_puts_the_branch_back_and_restacks() {
    let w = three_versions();
    let out = w.gt_ok(&["history", "revert", "p1", "-y"]);
    assert!(out.stderr.contains("reverting feat-a to p1"), "{out:?}");
    assert!(out.stderr.contains("gt history revert p3"), "undo hint: {out:?}");

    assert_eq!(w.show("feat-a", "a.txt").as_deref(), Some("v1\n"));
    assert_eq!(w.show("feat-a", "extra.txt"), None);
    assert_eq!(w.count("main..feat-a"), 1);
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());
    // Back on p1's patch, so it folds into p1 rather than adding p4.
    let v = history_json(&w, &[]);
    assert_eq!(versions(&v), vec![(3, false), (2, false), (1, true)]);
}

#[test]
fn revert_can_be_limited_to_paths() {
    let w = three_versions();
    w.gt_ok(&["history", "revert", "p2", "-y", "--", "a.txt"]);
    assert_eq!(w.show("feat-a", "a.txt").as_deref(), Some("v2\n"));
    assert_eq!(w.show("feat-a", "extra.txt").as_deref(), Some("x\n"), "other paths keep p3");
}

#[test]
fn revert_needs_confirmation_or_yes() {
    let w = three_versions();
    let tip = w.head("feat-a").unwrap();
    let out = w.gt_err(&["history", "revert", "p1"]);
    assert!(out.stderr.contains("no terminal"), "{out:?}");
    assert_eq!(w.head("feat-a").unwrap(), tip);
    let out = w.gt_err(&["history", "revert", "p3", "-y"]);
    assert!(out.stderr.contains("already feat-a's tip"), "{out:?}");
}
