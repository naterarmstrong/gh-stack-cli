//! Moving around a stack and looking at it offline: up/down/top/bottom/trunk,
//! checkout, ls and loc.

mod common;

use common::{World, line_of};
use serde_json::{Value, json};

fn three_branches() -> World {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.create("feat-b", "b.txt", "b\n");
    w.create("feat-c", "c.txt", "c\n");
    w
}

/// Records a PR number for a stacked branch, as `gt submit` or `gt sync` would.
fn set_pr(w: &World, branch: &str, number: u64, merged: bool) {
    w.edit_branch_state(branch, |b| b["pullRequest"] = json!({ "number": number, "merged": merged }));
}

#[test]
fn navigation_moves_through_the_stack() {
    let w = three_branches();
    w.gt_ok(&["bottom"]);
    assert_eq!(w.current(), "feat-a");
    w.gt_ok(&["up"]);
    assert_eq!(w.current(), "feat-b");
    w.gt_ok(&["top"]);
    assert_eq!(w.current(), "feat-c");
    w.gt_ok(&["down", "2"]);
    assert_eq!(w.current(), "feat-a");
    // Up past the top stops at the top.
    w.gt_ok(&["up", "10"]);
    assert_eq!(w.current(), "feat-c");
    w.gt_ok(&["trunk"]);
    assert_eq!(w.current(), "main");
}

#[test]
fn down_from_the_bottom_goes_to_trunk() {
    let w = three_branches();
    w.gt_ok(&["b"]);
    w.gt_ok(&["d"]);
    assert_eq!(w.current(), "main");
}

#[test]
fn navigation_aliases_work() {
    let w = three_branches();
    w.gt_ok(&["b"]);
    w.gt_ok(&["u"]);
    assert_eq!(w.current(), "feat-b");
    w.gt_ok(&["t"]);
    assert_eq!(w.current(), "feat-c");
    let out = w.gt_ok(&["t"]);
    assert!(out.stderr.contains("already on feat-c"), "{out:?}");
}

#[test]
fn navigation_skips_merged_branches() {
    let w = three_branches();
    set_pr(&w, "feat-b", 2, true);
    w.gt_ok(&["bottom"]);
    w.gt_ok(&["up"]);
    assert_eq!(w.current(), "feat-c");
    w.gt_ok(&["down"]);
    assert_eq!(w.current(), "feat-a");
}

#[test]
fn navigation_outside_a_stack_is_an_error() {
    let w = three_branches();
    w.git(&["checkout", "--quiet", "main"]);
    let out = w.gt_err(&["up"]);
    assert!(out.stderr.contains("main is not part of a stack"), "{out:?}");
    w.git(&["checkout", "--quiet", "--detach", "HEAD"]);
    let out = w.gt_err(&["top"]);
    assert!(out.stderr.contains("HEAD is detached"), "{out:?}");
}

#[test]
fn checkout_takes_a_branch_or_a_tracked_pr() {
    let w = three_branches();
    set_pr(&w, "feat-b", 42, false);
    set_pr(&w, "feat-a", 41, true);

    w.gt_ok(&["co", "feat-a"]);
    assert_eq!(w.current(), "feat-a");
    w.gt_ok(&["checkout", "42"]);
    assert_eq!(w.current(), "feat-b");
    w.gt_ok(&["co", "main"]);
    w.gt_ok(&["co", "#42"]);
    assert_eq!(w.current(), "feat-b");
    w.gt_ok(&["co", "main"]);
    w.gt_ok(&["co", "https://github.com/acme/widgets/pull/42/files"]);
    assert_eq!(w.current(), "feat-b");

    // 41 isn't a branch name, and its PR merged.
    w.git(&["branch", "-D", "feat-a"]);
    let out = w.gt_err(&["co", "41"]);
    assert!(out.stderr.contains("#41 (feat-a) is already merged"), "{out:?}");
    let out = w.gt_err(&["co", "99"]);
    assert!(out.stderr.contains("gt get 99"), "{out:?}");
}

#[test]
fn checkout_refuses_a_branch_in_another_worktree() {
    let w = three_branches();
    w.git(&["checkout", "--quiet", "main"]);
    let wt = w.tmp.path().join("wt");
    w.git(&["worktree", "add", "--quiet", wt.to_str().unwrap(), "feat-b"]);
    let out = w.gt_err(&["co", "feat-b"]);
    assert!(out.stderr.contains("feat-b is checked out in"), "{out:?}");
}

#[test]
fn checkout_without_a_target_needs_a_terminal() {
    let w = three_branches();
    let out = w.gt_err(&["co"]);
    assert!(out.stderr.contains("terminal"), "{out:?}");
}

#[test]
fn ls_draws_the_stack_top_down_over_its_trunk() {
    let w = three_branches();
    w.gt_ok(&["down"]);
    let out = w.gt_ok(&["ls"]);
    let lines: Vec<&str> = out.stdout.lines().collect();
    let pos = |b: &str| lines.iter().position(|l| l.split_whitespace().any(|w| w == b)).unwrap();
    assert!(pos("feat-c") < pos("feat-b") && pos("feat-b") < pos("feat-a") && pos("feat-a") < pos("main"), "{out:?}");

    let b = line_of(&out.stdout, "feat-b");
    assert!(b.starts_with('◉'), "current branch is marked: {b}");
    assert!(line_of(&out.stdout, "feat-a").starts_with('◯'));
    assert!(b.contains("not pushed"), "{b}");
    // The size column: one src line added.
    assert!(b.contains("+1"), "{b}");
    assert!(line_of(&out.stdout, "main").contains("= origin/main"), "{out:?}");
}

#[test]
fn ls_puts_each_stack_on_a_trunk_in_its_own_column() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.git(&["checkout", "--quiet", "main"]);
    w.create("fix-a", "f.txt", "f\n");

    let out = w.gt_ok(&["ls"]);
    assert!(line_of(&out.stdout, "fix-a").starts_with('│'), "second stack is indented: {out:?}");
    assert!(line_of(&out.stdout, "main").starts_with("◯─┘"), "{out:?}");

    let only = w.gt_ok(&["ls", "-s"]);
    assert!(only.stdout.contains("fix-a"), "{only:?}");
    assert!(!only.stdout.contains("feat-a"), "{only:?}");
}

#[test]
fn ls_with_no_stacks_says_how_to_start() {
    let w = World::new();
    let out = w.gt_ok(&["ls"]);
    assert!(out.stdout.contains("No stacks"), "{out:?}");
}

#[test]
fn ls_flags_multi_commit_and_stale_branches() {
    let w = three_branches();
    w.gt_ok(&["co", "feat-b"]);
    w.commit("b.txt", "b2\n", "Second commit");
    let out = w.gt_ok(&["ls"]);
    assert!(line_of(&out.stdout, "feat-b").contains("2 commits (gt squash)"), "{out:?}");
    assert!(line_of(&out.stdout, "feat-c").contains("needs restack"), "{out:?}");

    // Trunk moving on shows as "behind" on the bottom branch.
    w.advance_main("m.txt", "m\n");
    w.git(&["fetch", "--quiet", "origin"]);
    let out = w.gt_ok(&["ls"]);
    assert!(line_of(&out.stdout, "feat-a").contains("1 behind origin/main"), "{out:?}");
    assert!(line_of(&out.stdout, "main").contains("1 behind origin/main"), "{out:?}");
}

#[test]
fn ls_hides_merged_branches_once_deleted() {
    let w = three_branches();
    set_pr(&w, "feat-a", 1, true);
    let out = w.gt_ok(&["ls"]);
    assert!(line_of(&out.stdout, "feat-a").contains("merged"), "{out:?}");
    w.gt_ok(&["co", "feat-b"]);
    w.git(&["branch", "-D", "feat-a"]);
    let out = w.gt_ok(&["ls"]);
    assert!(!out.stdout.contains("feat-a"), "{out:?}");
}

#[test]
fn log_still_renders_when_github_is_unreachable() {
    let w = three_branches();
    let out = w.gt_ok(&["log"]);
    assert!(out.stderr.contains("couldn't load PR status"), "{out:?}");
    assert!(out.stdout.contains("feat-b"), "{out:?}");
    // Detail mode adds each branch's subject.
    assert!(line_of(&out.stdout, "feat-b").contains("Add b.txt"), "{out:?}");
}

fn loc_json(w: &World, args: &[&str]) -> Value {
    let mut full = vec!["loc", "--json"];
    full.extend(args);
    serde_json::from_str(&w.gt_ok(&full).stdout).unwrap()
}

#[test]
fn loc_splits_lines_by_kind() {
    let w = World::new();
    w.write("src/parse.rs", "fn a() {}\nfn b() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n");
    w.write("tests/parse_test.py", "def test_x():\n    pass\n");
    w.write("docs/guide.md", "# Guide\n");
    w.write("uv.lock", "x\ny\nz\n");
    w.gt_ok(&["create", "feat", "-am", "Parser"]);

    let v = loc_json(&w, &[]);
    assert_eq!(v["version"], 1);
    assert_eq!(v["branch"], "feat");
    assert_eq!(v["total"]["src"], json!({ "add": 3, "del": 0 }));
    // Python test file plus the Rust #[cfg(test)] module.
    assert_eq!(v["total"]["test"], json!({ "add": 6, "del": 0 }));
    assert_eq!(v["total"]["doc"], json!({ "add": 1, "del": 0 }));
    assert_eq!(v["total"]["gen"], json!({ "add": 3, "del": 0 }));
    let kinds: Vec<(String, String)> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["path"].as_str().unwrap().into(), f["kind"].as_str().unwrap().into()))
        .collect();
    assert!(kinds.contains(&("tests/parse_test.py".into(), "test".into())), "{kinds:?}");
    assert!(kinds.contains(&("uv.lock".into(), "gen".into())), "{kinds:?}");

    let text = w.gt_ok(&["loc"]);
    assert!(text.stdout.starts_with("feat"), "{text:?}");
    assert!(text.stdout.contains("src/parse.rs"), "{text:?}");
}

#[test]
fn loc_measures_only_the_branch_not_what_is_below_it() {
    let w = World::new();
    w.create("feat-a", "a.rs", "1\n2\n3\n");
    w.create("feat-b", "b.rs", "1\n");
    let v = loc_json(&w, &["feat-b"]);
    assert_eq!(v["total"]["src"]["add"], 1);
    assert_eq!(v["files"].as_array().unwrap().len(), 1);
}

#[test]
fn loc_sizes_an_untracked_branch_by_all_its_own_commits() {
    let w = World::new();
    w.git(&["checkout", "--quiet", "-b", "loose"]);
    w.commit("x.rs", "1\n", "One");
    w.commit("y.rs", "1\n2\n", "Two");
    let v = loc_json(&w, &[]);
    assert_eq!(v["branch"], "loose");
    assert_eq!(v["total"]["src"]["add"], 3);
}

#[test]
fn loc_takes_any_commit() {
    let w = World::new();
    let sha = w.commit("x.rs", "1\n2\n", "Direct on main");
    w.commit("y.rs", "1\n", "Another");
    let v = loc_json(&w, &[&sha[..10]]);
    assert_eq!(v["branch"], Value::Null);
    assert_eq!(v["commit"], sha.as_str());
    assert_eq!(v["total"]["src"]["add"], 2);
    let out = w.gt_err(&["loc", "no-such-thing"]);
    assert!(out.stderr.contains("no local branch or tracked PR matches"), "{out:?}");
}
