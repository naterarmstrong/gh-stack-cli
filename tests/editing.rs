//! Building and reshaping stacks locally: create, modify, restack, squash,
//! continue/abort, track and delete. None of these talk to GitHub.

mod common;

use common::World;

#[test]
fn create_from_trunk_starts_a_stack() {
    let w = World::new();
    let main = w.head("main").unwrap();
    w.create("feat-a", "a.txt", "a\n");

    assert_eq!(w.current(), "feat-a");
    assert_eq!(w.parent("feat-a"), main);
    assert_eq!(w.show("feat-a", "a.txt").as_deref(), Some("a\n"));
    assert_eq!(w.stacks(), vec![vec!["feat-a"]]);
    // Not submitted, so it's gt's alone: gh-stack doesn't see it.
    assert!(!w.state_path().exists());
    let state = w.local_state();
    assert_eq!(state["schemaVersion"], 1);
    assert_eq!(state["stacks"][0]["trunk"]["branch"], "main");
    assert_eq!(state["stacks"][0].get("on"), None);
    let b = w.branch_state("feat-a");
    assert_eq!(b["head"], w.head("feat-a").unwrap().as_str());
    assert_eq!(b["base"], main.as_str());
}

#[test]
fn create_stacks_each_branch_on_the_last() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.create("feat-b", "b.txt", "b\n");
    w.create("feat-c", "c.txt", "c\n");

    assert_eq!(w.stacks(), vec![vec!["feat-a", "feat-b", "feat-c"]]);
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());
    assert_eq!(w.parent("feat-c"), w.head("feat-b").unwrap());
}

#[test]
fn create_names_the_branch_after_the_message() {
    let w = World::new();
    w.write("a.txt", "a\n");
    w.gt_ok(&["create", "-am", "Add the parser: v2!\n\nLonger body"]);
    assert_eq!(w.current(), "add_the_parser_v2");
    assert_eq!(w.git(&["log", "-1", "--format=%s"]), "Add the parser: v2!");
}

#[test]
fn create_in_the_middle_restacks_the_branches_above() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.create("feat-c", "c.txt", "c\n");
    w.git(&["checkout", "--quiet", "feat-a"]);
    w.create("feat-b", "b.txt", "b\n");

    assert_eq!(w.stacks(), vec![vec!["feat-a", "feat-b", "feat-c"]]);
    assert_eq!(w.parent("feat-c"), w.head("feat-b").unwrap());
    // feat-c keeps its own change and now also sees feat-b's.
    assert_eq!(w.show("feat-c", "c.txt").as_deref(), Some("c\n"));
    assert_eq!(w.show("feat-c", "b.txt").as_deref(), Some("b\n"));
    assert_eq!(w.count("feat-a..feat-c"), 2);
}

#[test]
fn create_with_nothing_staged_makes_an_empty_branch() {
    let w = World::new();
    let out = w.gt_ok(&["create", "empty"]);
    assert!(out.stderr.contains("nothing staged"), "{out:?}");
    assert_eq!(w.head("empty"), w.head("main"));
    assert_eq!(w.stacks(), vec![vec!["empty"]]);
}

#[test]
fn create_refuses_an_existing_branch_or_no_name() {
    let w = World::new();
    w.git(&["branch", "taken"]);
    let out = w.gt_err(&["create", "taken", "-m", "x"]);
    assert!(out.stderr.contains("branch taken already exists"), "{out:?}");
    let out = w.gt_err(&["create"]);
    assert!(out.stderr.contains("give a branch name or a message"), "{out:?}");
    let out = w.gt_err(&["create", "-m", "!!!"]);
    assert!(out.stderr.contains("give a branch name or a message"), "{out:?}");
    assert!(!w.state_path().exists());
}

#[test]
fn create_on_an_untracked_branch_tracks_it_as_the_bottom() {
    let w = World::new();
    w.git(&["checkout", "--quiet", "-b", "loose"]);
    w.commit("l.txt", "l\n", "Loose work");
    let out = w.gt_ok(&["create", "above", "-m", "x"]);
    assert!(out.stderr.contains("tracking loose as the bottom of a new stack"), "{out:?}");
    assert_eq!(w.stacks(), vec![vec!["loose", "above"]]);
}

#[test]
fn create_with_base_starts_a_stack_on_trunk_from_anywhere() {
    let w = World::new();
    let main = w.head("main").unwrap();
    w.create("feat-a", "a.txt", "a\n");
    w.write("x.txt", "x\n");
    w.gt_ok(&["create", "--base", "main", "-am", "Other work"]);

    assert_eq!(w.current(), "other_work");
    assert_eq!(w.parent("other_work"), main);
    assert_eq!(w.show("other_work", "a.txt"), None);
    assert_eq!(w.show("other_work", "x.txt").as_deref(), Some("x\n"));
    assert_eq!(w.stacks(), vec![vec!["feat-a"], vec!["other_work"]]);
}

#[test]
fn create_with_base_inserts_above_that_branch() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.create("feat-c", "c.txt", "c\n");
    w.write("b.txt", "b\n");
    w.gt_ok(&["create", "feat-b", "--base", "feat-a", "-am", "b"]);

    assert_eq!(w.current(), "feat-b");
    assert_eq!(w.stacks(), vec![vec!["feat-a", "feat-b", "feat-c"]]);
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());
    assert_eq!(w.parent("feat-c"), w.head("feat-b").unwrap());
    assert_eq!(w.show("feat-c", "b.txt").as_deref(), Some("b\n"));
}

#[test]
fn create_with_an_unknown_base_changes_nothing() {
    let w = World::new();
    let out = w.gt_err(&["create", "x", "--base", "nope", "-m", "x"]);
    assert!(out.stderr.contains("no local branch or tracked PR matches nope"), "{out:?}");
    assert_eq!(w.current(), "main");
    assert_eq!(w.head("x"), None);
}

#[test]
fn modify_amends_and_restacks_everything_above() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.create("feat-b", "b.txt", "b\n");
    w.create("feat-c", "c.txt", "c\n");
    w.git(&["checkout", "--quiet", "feat-a"]);
    let old_a = w.head("feat-a").unwrap();

    w.write("a.txt", "a2\n");
    let out = w.gt_ok(&["modify", "-a"]);
    assert!(out.stderr.contains("restacked feat-b"), "{out:?}");

    let new_a = w.head("feat-a").unwrap();
    assert_ne!(new_a, old_a);
    assert_eq!(w.parent("feat-a"), w.head("main").unwrap(), "amended, not a second commit");
    assert_eq!(w.parent("feat-b"), new_a);
    assert_eq!(w.parent("feat-c"), w.head("feat-b").unwrap());
    assert_eq!(w.show("feat-c", "a.txt").as_deref(), Some("a2\n"));
    assert_eq!(w.git(&["log", "-1", "--format=%s", "feat-a"]), "Add a.txt");
    assert_eq!(w.current(), "feat-a");
    assert_eq!(w.branch_state("feat-b")["base"], new_a.as_str());
}

#[test]
fn modify_message_replaces_the_commit_message() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.gt_ok(&["modify", "-m", "Better subject"]);
    assert_eq!(w.git(&["log", "-1", "--format=%s"]), "Better subject");
    assert_eq!(w.count("main..feat-a"), 1);
}

#[test]
fn modify_on_an_empty_branch_makes_its_first_commit() {
    let w = World::new();
    w.gt_ok(&["create", "empty"]);
    w.write("e.txt", "e\n");
    w.gt_ok(&["modify", "-a", "-m", "First"]);
    assert_eq!(w.count("main..empty"), 1);
    assert_eq!(w.parent("empty"), w.head("main").unwrap());
    assert_eq!(w.git(&["log", "-1", "--format=%s"]), "First");
}

#[test]
fn modify_with_nothing_staged_just_restacks() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.create("feat-b", "b.txt", "b\n");
    w.git(&["checkout", "--quiet", "feat-a"]);
    // Amend behind gt's back so feat-b is left on the old commit.
    w.write("a.txt", "a2\n");
    w.git(&["commit", "--quiet", "-a", "--amend", "--no-edit"]);
    let a = w.head("feat-a").unwrap();

    let out = w.gt_ok(&["modify"]);
    assert!(out.stderr.contains("nothing staged; just restacking"), "{out:?}");
    assert_eq!(w.head("feat-a").unwrap(), a);
    assert_eq!(w.parent("feat-b"), a);
}

#[test]
fn modify_outside_a_stack_only_amends() {
    let w = World::new();
    w.git(&["checkout", "--quiet", "-b", "loose"]);
    w.commit("l.txt", "l\n", "Loose");
    w.write("l.txt", "l2\n");
    w.gt_ok(&["modify", "-a"]);
    assert_eq!(w.count("main..loose"), 1);
    assert_eq!(w.show("loose", "l.txt").as_deref(), Some("l2\n"));
    assert!(!w.state_path().exists(), "no stack should be created");

    // A loose branch with nothing on top of trunk gets a first commit, not an amend of trunk.
    w.git(&["checkout", "--quiet", "-b", "fresh", "main"]);
    w.write("f.txt", "f\n");
    w.gt_ok(&["modify", "-a", "-m", "Fresh"]);
    assert_eq!(w.parent("fresh"), w.head("main").unwrap());
}

#[test]
fn restack_moves_children_after_an_out_of_band_amend() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.create("feat-b", "b.txt", "b\n");
    w.git(&["checkout", "--quiet", "feat-a"]);
    w.write("a.txt", "a2\n");
    w.git(&["commit", "--quiet", "-a", "--amend", "--no-edit"]);
    w.git(&["checkout", "--quiet", "feat-b"]);

    let ls = w.gt_ok(&["ls"]);
    assert!(common::line_of(&ls.stdout, "feat-b").contains("needs restack"), "{ls:?}");

    w.gt_ok(&["restack"]);
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());
    // The working tree followed the checked-out branch.
    assert_eq!(w.read("a.txt"), "a2\n");
    let out = w.gt_ok(&["restack"]);
    assert!(out.stderr.contains("already restacked"), "{out:?}");
}

#[test]
fn restack_trunk_moves_the_bottom_onto_the_remote_trunk() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.create("feat-b", "b.txt", "b\n");
    let landed = w.advance_main("m.txt", "m\n");
    w.git(&["fetch", "--quiet", "origin"]);

    // Without --trunk the bottom stays put.
    let out = w.gt_ok(&["restack"]);
    assert!(out.stderr.contains("already restacked"), "{out:?}");

    w.gt_ok(&["restack", "--trunk"]);
    assert_eq!(w.parent("feat-a"), landed);
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());
    assert_eq!(w.show("feat-b", "m.txt").as_deref(), Some("m\n"));
}

#[test]
fn restack_refuses_a_branch_with_several_commits() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.create("feat-b", "b.txt", "b\n");
    w.commit("b.txt", "b2\n", "More b");
    w.git(&["checkout", "--quiet", "feat-a"]);
    w.write("a.txt", "a2\n");
    w.git(&["commit", "--quiet", "-a", "--amend", "--no-edit"]);
    let b = w.head("feat-b").unwrap();

    let out = w.gt_err(&["restack"]);
    assert!(out.stderr.contains("feat-b has 2 commits of its own"), "{out:?}");
    assert!(out.stderr.contains("gt squash"), "{out:?}");
    assert_eq!(w.head("feat-b").unwrap(), b);
    // Squashing can't help either: feat-b no longer sits on feat-a's tip.
    let out = w.gt_err(&["squash"]);
    assert!(out.stderr.contains("doesn't contain its parent's current tip"), "{out:?}");
}

#[test]
fn squash_folds_extra_commits_so_restack_works() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.create("feat-b", "b.txt", "b\n");
    w.commit("b.txt", "b2\n", "More b");

    let tree = w.git(&["rev-parse", "feat-b^{tree}"]);
    let out = w.gt_ok(&["squash"]);
    assert!(out.stderr.contains("folding 2 commits on feat-b"), "{out:?}");
    assert_eq!(w.git(&["rev-parse", "feat-b^{tree}"]), tree, "squash keeps the content");
    assert_eq!(w.git(&["log", "-1", "--format=%s", "feat-b"]), "Add b.txt", "first commit's message");
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());

    w.git(&["checkout", "--quiet", "feat-a"]);
    w.write("a.txt", "a2\n");
    w.gt_ok(&["modify", "-a"]);
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());
    assert_eq!(w.show("feat-b", "b.txt").as_deref(), Some("b2\n"));
    assert_eq!(w.show("feat-b", "a.txt").as_deref(), Some("a2\n"));

    let out = w.gt_ok(&["squash"]);
    assert!(out.stderr.contains("every branch already has one commit"), "{out:?}");
}

/// feat-a and feat-b both edit shared.txt, then feat-a is amended so feat-b no longer applies.
fn conflicted_stack() -> World {
    let w = World::new();
    w.create("feat-a", "shared.txt", "one\n");
    w.create("feat-b", "shared.txt", "two\n");
    w.create("feat-c", "c.txt", "c\n");
    w.git(&["checkout", "--quiet", "feat-a"]);
    w.write("shared.txt", "uno\n");
    w.git(&["commit", "--quiet", "-a", "--amend", "--no-edit"]);
    w
}

#[test]
fn restack_conflict_stops_mid_rebase_and_continue_finishes_it() {
    let w = conflicted_stack();
    let out = w.gt(&["restack"]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("conflict restacking feat-b onto feat-a"), "{out:?}");
    assert!(out.stderr.contains("shared.txt"), "{out:?}");
    assert!(w.common_dir().join("rebase-merge").exists() || w.common_dir().join("rebase-apply").exists());

    w.write("shared.txt", "dos\n");
    w.git(&["add", "shared.txt"]);
    let out = w.gt_ok(&["continue"]);
    assert!(out.stderr.contains("stack restacked"), "{out:?}");

    assert_eq!(w.current(), "feat-a", "back where the restack started");
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());
    assert_eq!(w.parent("feat-c"), w.head("feat-b").unwrap());
    assert_eq!(w.show("feat-c", "shared.txt").as_deref(), Some("dos\n"));
    assert!(!w.common_dir().join("gt-rebase").exists());
}

#[test]
fn continue_with_unresolved_conflicts_fails() {
    let w = conflicted_stack();
    assert_eq!(w.gt(&["restack"]).code, 1);
    let out = w.gt_err(&["continue"]);
    assert!(out.stderr.contains("still has conflicts"), "{out:?}");
}

#[test]
fn abort_marks_the_branch_and_returns_to_where_you_were() {
    let w = conflicted_stack();
    let b = w.head("feat-b").unwrap();
    assert_eq!(w.gt(&["restack"]).code, 1);

    let out = w.gt_ok(&["abort"]);
    assert!(out.stderr.contains("marked it as needing a rebase"), "{out:?}");
    assert_eq!(w.current(), "feat-a");
    assert_eq!(w.head("feat-b").unwrap(), b, "feat-b is left where it was");
    assert_eq!(w.config("branch.feat-b.gtNeedsRebase").as_deref(), Some(b.as_str()));
    let ls = w.gt_ok(&["ls"]);
    assert!(common::line_of(&ls.stdout, "feat-b").contains("needs rebase"), "{ls:?}");

    let out = w.gt_err(&["abort"]);
    assert!(out.stderr.contains("no rebase in progress"), "{out:?}");
}

#[test]
fn a_clean_restack_clears_the_needs_rebase_mark() {
    let w = conflicted_stack();
    assert_eq!(w.gt(&["restack"]).code, 1);
    w.gt_ok(&["abort"]);
    // Undo the conflicting change; now feat-b replays cleanly again.
    w.write("shared.txt", "one\n");
    w.gt_ok(&["modify", "-a"]);
    assert_eq!(w.config("branch.feat-b.gtNeedsRebase"), None);
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());
}

#[test]
fn track_adopts_existing_branches_bottom_first() {
    let w = World::new();
    w.git(&["checkout", "--quiet", "-b", "x"]);
    w.commit("x.txt", "x\n", "X");
    w.git(&["checkout", "--quiet", "-b", "y"]);
    w.commit("y.txt", "y\n", "Y");

    let out = w.gt_ok(&["track", "x", "y"]);
    assert!(out.stderr.contains("tracking x ← y on main"), "{out:?}");
    assert_eq!(w.stacks(), vec![vec!["x", "y"]]);
    assert_eq!(w.branch_state("y")["head"], w.head("y").unwrap().as_str());

    assert!(w.gt_err(&["track", "x"]).stderr.contains("x is already in a stack"));
    assert!(w.gt_err(&["track", "main"]).stderr.contains("main is the trunk"));
    assert!(w.gt_err(&["track", "nope"]).stderr.contains("no local branch nope"));
}

#[test]
fn track_defaults_to_the_current_branch_and_takes_a_trunk() {
    let w = World::new();
    w.git(&["checkout", "--quiet", "-b", "release"]);
    w.git(&["checkout", "--quiet", "-b", "fix"]);
    w.commit("f.txt", "f\n", "Fix");
    w.gt_ok(&["track", "--trunk", "release"]);
    assert_eq!(w.stacks(), vec![vec!["fix"]]);
    assert_eq!(w.local_state()["stacks"][0]["trunk"]["branch"], "release");
}

#[test]
fn delete_removes_a_branch_and_everything_above_it() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.create("feat-b", "b.txt", "b\n");
    w.create("feat-c", "c.txt", "c\n");

    let out = w.gt_ok(&["delete", "feat-b", "-f"]);
    assert!(out.stderr.contains("moved to feat-a"), "{out:?}");
    assert_eq!(w.current(), "feat-a");
    assert_eq!(w.head("feat-b"), None);
    assert_eq!(w.head("feat-c"), None);
    assert_eq!(w.stacks(), vec![vec!["feat-a"]]);

    w.gt_ok(&["delete", "feat-a", "-f"]);
    assert_eq!(w.current(), "main");
    assert!(w.stacks().is_empty(), "the emptied stack is dropped");
}

#[test]
fn delete_without_force_needs_a_terminal() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    let out = w.gt_err(&["delete", "feat-a"]);
    assert!(out.stderr.contains("never pushed"), "{out:?}");
    assert!(out.stderr.contains("no terminal"), "{out:?}");
    assert!(w.head("feat-a").is_some(), "nothing deleted");
}

#[test]
fn delete_refuses_branches_checked_out_elsewhere() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.create("feat-b", "b.txt", "b\n");
    w.git(&["checkout", "--quiet", "main"]);
    let wt = w.tmp.path().join("wt");
    w.git(&["worktree", "add", "--quiet", wt.to_str().unwrap(), "feat-b"]);

    let out = w.gt_err(&["delete", "feat-a", "-f"]);
    assert!(out.stderr.contains("checked out in other worktrees: feat-b"), "{out:?}");
    assert!(w.head("feat-a").is_some());
}

#[test]
fn mutating_commands_respect_gh_stack_locks() {
    use std::os::fd::AsRawFd;
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(w.common_dir().join("gh-stack.lock"))
        .unwrap();
    assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);

    let out = w.gt_err(&["create", "feat-b", "-m", "b"]);
    assert!(out.stderr.contains("another gh-stack/gt write is in progress"), "{out:?}");
    // Read-only commands don't take the lock.
    w.gt_ok(&["ls"]);
    drop(f);
    w.gt_ok(&["create", "feat-b", "-m", "b"]);
}

#[test]
fn unknown_state_fields_survive_a_rewrite() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    // A stack gh-stack made, with fields gt doesn't know.
    w.git(&["branch", "theirs", "main"]);
    w.write_state(&serde_json::json!({
        "schemaVersion": 1,
        "repository": "acme/widgets",
        "fromGhStack": "keep me",
        "stacks": [{ "trunk": { "branch": "main" }, "branches": [{ "branch": "theirs", "custom": { "n": 1 } }] }],
    }));
    w.edit_branch_state("feat-a", |b| b["mine"] = "too".into());

    w.create("feat-b", "b.txt", "b\n");
    let s = w.state();
    assert_eq!(s["fromGhStack"], "keep me");
    assert_eq!(s["repository"], "acme/widgets");
    assert_eq!(s["stacks"][0]["branches"][0]["custom"]["n"], 1);
    assert_eq!(w.branch_state("feat-a")["mine"], "too");
    assert_eq!(w.stacks(), vec![vec!["theirs"], vec!["feat-a", "feat-b"]]);
}

#[test]
fn newer_state_schema_is_refused() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    let mut s = w.local_state();
    s["schemaVersion"] = 2.into();
    w.write_local_state(&s);
    let out = w.gt_err(&["ls"]);
    assert!(out.stderr.contains(".git/gt-stack has schema version 2"), "{out:?}");
    std::fs::remove_file(w.local_state_path()).unwrap();
    w.write_state(&serde_json::json!({ "schemaVersion": 2, "stacks": [] }));
    let out = w.gt_err(&["ls"]);
    assert!(out.stderr.contains(".git/gh-stack has schema version 2"), "{out:?}");
}

#[test]
fn outside_a_repository_is_an_error() {
    let w = World::new();
    let out = w.gt_in(w.tmp.path(), &["ls"]);
    assert!(!out.ok());
    assert!(out.stderr.contains("not inside a git repository"), "{out:?}");
}
