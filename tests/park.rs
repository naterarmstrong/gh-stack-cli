//! `gt park`: a worktree left on its trunk's tip with no branch checked out,
//! which `gt sync` keeps on the tip and `gt co`/`gt create` pick back up.

mod common;

use common::World;

fn head(w: &World, dir: &std::path::Path) -> String {
    w.git_in(dir, &["rev-parse", "HEAD"])
}

#[test]
fn park_frees_the_branch_and_sync_keeps_the_worktree_on_the_tip() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");

    let out = w.gt_ok(&["park"]);
    assert_eq!(w.current(), "", "no branch checked out");
    assert_eq!(head(&w, &w.repo), w.git(&["rev-parse", "origin/main"]));
    assert!(out.stderr.contains("parked on origin/main"), "{out:?}");
    assert!(out.stderr.contains("left feat-a"), "{out:?}");

    // The branch it left is free for another worktree.
    let wt = w.tmp.path().join("wt");
    w.git(&["worktree", "add", "--quiet", wt.to_str().unwrap(), "feat-a"]);
    let out = w.gt_in(&wt, &["ls"]);
    assert!(out.stdout.contains("parked: repo"), "{out:?}");

    let landed = w.advance_main("m.txt", "m\n");
    let out = w.gt_ok(&["sync", "--offline"]);
    assert_eq!(head(&w, &w.repo), landed, "{out:?}");
    assert!(out.stderr.contains("moved parked worktree"), "{out:?}");
    assert!(w.repo.join("m.txt").exists());
    assert_eq!(w.current(), "", "still parked");

    let out = w.gt_ok(&["ls"]);
    assert!(out.stderr.contains(&format!("parked on origin/main at {}", &landed[..8])), "{out:?}");
    assert!(out.stdout.contains("parked: here"), "{out:?}");
}

#[test]
fn sync_in_a_parked_worktree_with_nothing_else_to_sync() {
    let w = World::new();
    w.gt_ok(&["park"]);
    let landed = w.advance_main("m.txt", "m\n");
    let out = w.gt_ok(&["sync", "--offline"]);
    assert_eq!(head(&w, &w.repo), landed, "{out:?}");
}

#[test]
fn parked_worktree_explains_itself_and_co_dash_goes_back() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.gt_ok(&["park"]);

    let out = w.gt_err(&["modify"]);
    assert!(out.stderr.contains("this worktree is parked on main"), "{out:?}");

    w.gt_ok(&["co", "-"]);
    assert_eq!(w.current(), "feat-a");
    assert!(!w.common_dir().join("gt-parked").exists(), "back on a branch, the mark is gone");
}

#[test]
fn create_from_a_parked_worktree_starts_a_stack_on_its_trunk() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.gt_ok(&["park"]);
    let landed = w.advance_main("m.txt", "m\n");
    w.gt_ok(&["sync", "--offline"]);

    w.create("feat-b", "b.txt", "b\n");
    assert_eq!(w.current(), "feat-b");
    assert_eq!(w.parent("feat-b"), landed, "starts where the worktree was parked, not on a stale local main");
    assert_eq!(w.stacks(), vec![vec!["feat-a"], vec!["feat-b"]]);
}

#[test]
fn sync_leaves_a_parked_head_that_moved() {
    let w = World::new();
    w.gt_ok(&["park"]);
    let mine = w.commit("x.txt", "x\n", "Made while parked");
    w.advance_main("m.txt", "m\n");

    let out = w.gt_ok(&["sync", "--offline"]);
    assert_eq!(head(&w, &w.repo), mine, "{out:?}");
    assert!(out.stderr.contains("HEAD moved off where gt parked it"), "{out:?}");

    // `gt create` keeps the commit on a branch.
    w.create("keep", "y.txt", "y\n");
    assert_eq!(w.parent("keep"), mine);
}

#[test]
fn park_refuses_uncommitted_changes() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.write("a.txt", "changed\n");
    let out = w.gt_err(&["park"]);
    assert!(out.stderr.contains("uncommitted changes"), "{out:?}");
    assert_eq!(w.current(), "feat-a");
}

#[test]
fn park_in_a_linked_worktree_marks_only_that_one() {
    let w = World::new();
    let wt = w.tmp.path().join("wt-one");
    w.git(&["worktree", "add", "--quiet", "-b", "scratch", wt.to_str().unwrap(), "main"]);
    let out = w.gt_in(&wt, &["park"]);
    assert!(out.ok(), "{out:?}");
    assert_eq!(w.git_in(&wt, &["branch", "--show-current"]), "");
    assert_eq!(w.current(), "main", "the main checkout is untouched");

    let landed = w.advance_main("m.txt", "m\n");
    let out = w.gt_in(&wt, &["sync", "--offline"]);
    assert!(out.ok(), "{out:?}");
    assert_eq!(head(&w, &wt), landed);
}
