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

/// feat-a with three commits (a.txt v1, v2, then extra.txt), and feat-b on top.
fn three_commits() -> World {
    let w = World::new();
    w.create("feat-a", "a.txt", "v1\n");
    w.write("a.txt", "v2\n");
    w.gt_ok(&["modify", "-a", "--new-commit", "-m", "a.txt v2"]);
    w.write("extra.txt", "x\n");
    w.gt_ok(&["modify", "-a", "-m", "Add extra.txt"]);
    w.create("feat-b", "b.txt", "b\n");
    w.gt_ok(&["down"]);
    w
}

fn tips(v: &Value) -> Vec<bool> {
    v["versions"].as_array().unwrap().iter().map(|v| v["tip"] == true).collect()
}

#[test]
fn each_commit_of_a_multi_commit_branch_is_a_version() {
    let w = three_commits();
    let v = history_json(&w, &[]);
    assert_eq!(versions(&v), vec![(3, true), (2, false), (1, false)]);
    let subjects: Vec<&str> =
        v["versions"].as_array().unwrap().iter().map(|v| v["subject"].as_str().unwrap()).collect();
    assert_eq!(subjects, vec!["Add extra.txt", "a.txt v2", "Add a.txt"]);
    // Each version's delta is what its commit added.
    assert_eq!(v["versions"][0]["delta"]["total"]["doc"], serde_json::json!({ "add": 1, "del": 0 }));
    assert_eq!(v["versions"][1]["delta"]["total"]["doc"], serde_json::json!({ "add": 1, "del": 1 }));

    // `show` is the branch as of that commit, against the branch's base.
    let out = w.gt_ok(&["history", "show", "2", "--json"]);
    let s: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(s["a"]["sha"], w.head("main").unwrap().as_str());
    let paths: Vec<&str> = s["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec!["a.txt"]);
    let out = w.gt_ok(&["history", "compare", "1", "3", "--json"]);
    let c: Value = serde_json::from_str(&out.stdout).unwrap();
    let mut paths: Vec<&str> = c["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    paths.sort();
    assert_eq!(paths, vec!["a.txt", "extra.txt"]);
}

#[test]
fn commits_the_branch_passed_through_are_versions_too() {
    let w = World::new();
    w.create("feat-a", "a.txt", "v1\n");
    // Two commits arrive at once, as a fetch of someone else's pushes would bring them.
    w.git(&["checkout", "--quiet", "--detach"]);
    w.commit("a.txt", "v2\n", "Second");
    let top = w.commit("a.txt", "v3\n", "Third");
    w.git(&["update-ref", "-m", "pull: fast-forward", "refs/heads/feat-a", &top]);
    w.git(&["checkout", "--quiet", "feat-a"]);

    let v = history_json(&w, &[]);
    assert_eq!(versions(&v), vec![(3, true), (2, false), (1, false)]);
    assert_eq!(tips(&v), vec![true, false, true], "p2 was never the tip here");
    assert_eq!(v["versions"][1]["delta"]["vs"], 1);
}

#[test]
fn restacking_a_multi_commit_branch_adds_no_versions() {
    let w = three_commits();
    w.git(&["checkout", "--quiet", "main"]);
    w.advance_main("m.txt", "m\n");
    w.git(&["fetch", "--quiet", "origin"]);
    w.git(&["checkout", "--quiet", "feat-a"]);
    w.gt_ok(&["restack", "--trunk"]);
    let v = history_json(&w, &[]);
    assert_eq!(versions(&v), vec![(3, true), (2, false), (1, false)]);
    // The restacked p2 is what the version points at now.
    assert_eq!(v["versions"][1]["sha"], w.git(&["rev-parse", "feat-a~1"]).as_str());
    assert_eq!(v["versions"][0]["delta"]["total"]["doc"], serde_json::json!({ "add": 1, "del": 0 }));
}

#[test]
fn reverting_a_multi_commit_branch_resets_it_to_that_commit() {
    let w = three_commits();
    let p2 = w.git(&["rev-parse", "feat-a~1"]);
    w.gt_ok(&["history", "revert", "p2", "-y"]);
    assert_eq!(w.head("feat-a").unwrap(), p2);
    assert_eq!(w.count("main..feat-a"), 2);
    assert_eq!(w.parent("feat-b"), p2);
    let v = history_json(&w, &[]);
    assert_eq!(versions(&v), vec![(3, false), (2, true), (1, false)]);
}

#[test]
fn reverting_paths_on_a_multi_commit_branch_adds_a_commit() {
    let w = three_commits();
    let tip = w.head("feat-a").unwrap();
    let out = w.gt_ok(&["history", "revert", "p1", "-y", "--", "a.txt"]);
    assert!(out.stderr.contains("in a new commit"), "{out:?}");
    assert_eq!(w.parent("feat-a"), tip);
    assert_eq!(w.show("feat-a", "a.txt").as_deref(), Some("v1\n"));
    assert_eq!(w.show("feat-a", "extra.txt").as_deref(), Some("x\n"));
    assert_eq!(w.git(&["log", "-1", "--format=%s", "feat-a"]), "Put a.txt back as p1 had it");
}

#[test]
fn squashing_folds_into_the_latest_version() {
    let w = three_commits();
    w.gt_ok(&["squash"]);
    assert_eq!(w.count("main..feat-a"), 1);
    let v = history_json(&w, &[]);
    assert_eq!(versions(&v), vec![(3, true), (2, false), (1, false)]);
}
