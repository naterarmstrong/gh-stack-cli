//! Stacks that aren't (all) on GitHub yet. Branches without PRs live in gt's
//! `.git/gt-stack`, on a trunk or on top of a gh-stack stack, and only move
//! into `.git/gh-stack` when submitted or when their PR is stacked on GitHub.

mod common;

use common::fake_github::FakePr;
use common::{World, line_of};

fn strings(v: &[&[&str]]) -> Vec<Vec<String>> {
    v.iter().map(|s| s.iter().map(|b| b.to_string()).collect()).collect()
}

/// feat-a submitted as PR #1; feat-b and feat-c local on top of it.
fn pr_with_local_branches_above() -> World {
    let w = World::with_github();
    w.create("feat-a", "a.rs", "a\n");
    w.gt_ok(&["submit"]);
    w.create("feat-b", "b.rs", "b\n");
    w.create("feat-c", "c.rs", "c\n");
    w
}

#[test]
fn an_all_local_stack_restacks_and_syncs_without_github() {
    let w = World::with_github();
    w.create("feat-a", "a.rs", "a\n");
    w.create("feat-b", "b.rs", "b\n");
    w.create("feat-c", "c.rs", "c\n");
    assert_eq!(w.local_stacks(), strings(&[&["feat-a", "feat-b", "feat-c"]]));
    assert!(w.shared_stacks().is_empty(), "gh-stack sees nothing");

    let landed = w.advance_main("m.rs", "m\n");
    w.gt_ok(&["sync"]);
    assert_eq!(w.parent("feat-a"), landed);
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());
    assert_eq!(w.parent("feat-c"), w.head("feat-b").unwrap());
    assert!(w.gh().prs().is_empty());
    assert_eq!(w.remote_head("feat-a"), None, "sync never pushes");
    assert_eq!(w.local_stacks(), strings(&[&["feat-a", "feat-b", "feat-c"]]));
    assert!(!w.state_path().exists());
}

#[test]
fn local_branches_stay_stacked_on_a_pr() {
    let w = pr_with_local_branches_above();
    assert_eq!(w.shared_stacks(), strings(&[&["feat-a"]]));
    assert_eq!(w.local_stacks(), strings(&[&["feat-b", "feat-c"]]));
    assert_eq!(w.local_state()["stacks"][0]["on"], "feat-a");
    assert_eq!(w.stacks(), strings(&[&["feat-a", "feat-b", "feat-c"]]));

    // One stack in ls, with the PR at the bottom.
    let ls = w.gt_ok(&["ls"]);
    assert!(line_of(&ls.stdout, "feat-a").contains("#1"), "{ls:?}");
    assert!(line_of(&ls.stdout, "feat-c").starts_with('◉'), "{ls:?}");
    assert!(!ls.stdout.contains('│'), "a single column: {ls:?}");

    // Editing the PR restacks the local branches.
    w.gt_ok(&["bottom"]);
    w.write("a.rs", "a2\n");
    w.gt_ok(&["modify", "-a"]);
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());
    assert_eq!(w.parent("feat-c"), w.head("feat-b").unwrap());
    w.gt_ok(&["top"]);
    w.gt_ok(&["down"]);
    assert_eq!(w.current(), "feat-b");
}

#[test]
fn a_pushed_branch_without_a_pr_is_still_local() {
    let w = pr_with_local_branches_above();
    w.git(&["push", "--quiet", "origin", "feat-b"]);
    let landed = w.advance_main("m.rs", "m\n");
    let out = w.gt_ok(&["sync"]);

    assert_eq!(w.local_stacks(), strings(&[&["feat-b", "feat-c"]]));
    assert_eq!(w.parent("feat-a"), landed);
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());
    assert_eq!(w.parent("feat-c"), w.head("feat-b").unwrap());
    // Only shared branches are listed for `gt submit`.
    assert!(out.stderr.contains("feat-a changed locally"), "{out:?}");
    assert!(!out.stderr.contains("feat-b changed locally"), "{out:?}");
    assert_eq!(w.gh().prs().len(), 1);
}

#[test]
fn local_branches_move_onto_trunk_when_the_pr_below_merges() {
    let w = pr_with_local_branches_above();
    let landed = w.merge_pr(1);
    w.gt_ok(&["sync"]);
    assert_eq!(w.head("feat-a"), None);
    assert_eq!(w.parent("feat-b"), landed);
    assert_eq!(w.stacks(), strings(&[&["feat-a", "feat-b", "feat-c"]]), "feat-a stays as merged history");
    assert_eq!(w.local_stacks(), strings(&[&["feat-b", "feat-c"]]));

    w.gt_ok(&["down"]);
    w.gt_ok(&["submit"]);
    assert_eq!(w.gh().pr_for("feat-b").base, "main");
    assert_eq!(w.local_stacks(), strings(&[&["feat-c"]]));
}

#[test]
fn submit_shares_the_current_branch_and_those_below_only() {
    let w = World::with_github();
    w.create("feat-a", "a.rs", "a\n");
    w.create("feat-b", "b.rs", "b\n");
    w.create("feat-c", "c.rs", "c\n");
    w.gt_ok(&["down"]);
    let out = w.gt_ok(&["submit"]);
    assert!(out.stderr.contains("feat-c stay local"), "{out:?}");

    let a = w.gh().pr_for("feat-a");
    let b = w.gh().pr_for("feat-b");
    assert_eq!((a.base.as_str(), b.base.as_str()), ("main", "feat-a"));
    assert_eq!(w.gh().prs().len(), 2);
    assert_eq!(w.gh().stacks()[0].prs, vec![a.number, b.number]);
    assert_eq!(w.remote_head("feat-c"), None);
    assert_eq!(w.shared_stacks(), strings(&[&["feat-a", "feat-b"]]));
    assert_eq!(w.local_stacks(), strings(&[&["feat-c"]]));
    assert_eq!(w.local_state()["stacks"][0]["on"], "feat-b");
    assert_eq!(w.stacks(), strings(&[&["feat-a", "feat-b", "feat-c"]]));

    // Resubmitting from the bottom pushes the shared branches and restacks feat-c locally.
    w.gt_ok(&["bottom"]);
    w.write("a.rs", "a2\n");
    w.gt_ok(&["modify", "-a"]);
    w.gt_ok(&["submit"]);
    assert_eq!(w.remote_head("feat-a"), w.head("feat-a"));
    assert_eq!(w.remote_head("feat-b"), w.head("feat-b"));
    assert_eq!(w.remote_head("feat-c"), None);
    assert_eq!(w.parent("feat-c"), w.head("feat-b").unwrap());

    // Submitting the top shares the rest.
    w.gt_ok(&["top"]);
    w.gt_ok(&["submit"]);
    let c = w.gh().pr_for("feat-c");
    assert_eq!(c.base, "feat-b");
    assert_eq!(w.gh().stacks()[0].prs, vec![a.number, b.number, c.number]);
    assert_eq!(w.shared_stacks(), strings(&[&["feat-a", "feat-b", "feat-c"]]));
    assert!(w.local_stacks().is_empty());
}

#[test]
fn a_branch_inserted_below_a_shared_one_is_shared() {
    let w = World::with_github();
    w.create("feat-a", "a.rs", "a\n");
    w.create("feat-b", "b.rs", "b\n");
    w.gt_ok(&["submit"]);
    w.gt_ok(&["bottom"]);
    w.create("feat-mid", "m.rs", "m\n");
    // gh-stack can't hold feat-b above a branch it doesn't know.
    assert_eq!(w.shared_stacks(), strings(&[&["feat-a", "feat-mid", "feat-b"]]));
    assert!(w.local_stacks().is_empty());
}

/// feat-a is PR #1; feat-b (local, on feat-a) was pushed and got a PR (#2) some
/// other way, based on feat-a but not stacked with #1 on GitHub. feat-c is local above.
fn unstacked_pr_on_a_local_branch() -> World {
    let w = pr_with_local_branches_above();
    w.git(&["push", "--quiet", "origin", "feat-b"]);
    let n = w.gh().add_pr(FakePr::new("feat-b", "feat-a"));
    assert_eq!(n, 2);
    w
}

const QUESTION: &str = "feat-b has PR #2 on feat-a, but GitHub doesn't stack it with #1";

#[test]
fn sync_offers_to_stack_a_pr_found_on_a_local_branch() {
    let w = unstacked_pr_on_a_local_branch();
    let (out, screen) = w.gt_with_tty(&["sync"], &["y"]);
    assert!(out.ok(), "{out:?}");
    assert!(screen.contains(QUESTION), "{screen}");
    assert!(out.stderr.contains("stacked #2 on #1 on GitHub"), "{out:?}");

    let stacks = w.gh().stacks();
    assert_eq!(stacks.len(), 1);
    assert_eq!(stacks[0].prs, vec![1, 2]);
    assert_eq!(w.state()["stacks"][0]["number"], stacks[0].number);
    assert_eq!(w.shared_stacks(), strings(&[&["feat-a", "feat-b"]]));
    assert_eq!(w.local_stacks(), strings(&[&["feat-c"]]));
    assert_eq!(w.branch_state("feat-b")["pullRequest"]["number"], 2);
}

#[test]
fn declining_keeps_the_branch_local_and_isnt_asked_again() {
    let w = unstacked_pr_on_a_local_branch();
    let (out, screen) = w.gt_with_tty(&["sync"], &["n"]);
    assert!(out.ok(), "{out:?}");
    assert!(screen.contains(QUESTION), "{screen}");
    assert!(out.stderr.contains("keeping feat-b local-only"), "{out:?}");
    assert!(w.gh().stacks().is_empty());
    assert_eq!(w.local_stacks(), strings(&[&["feat-b", "feat-c"]]));
    assert_eq!(w.config("branch.feat-b.gtNoLink").as_deref(), Some("2"));

    let (out, screen) = w.gt_with_tty(&["sync"], &[]);
    assert!(out.ok(), "{out:?}");
    assert!(!screen.contains(QUESTION), "{screen}");

    // Submitting from feat-b still stacks it.
    w.gt_ok(&["down"]);
    w.gt_ok(&["submit"]);
    assert_eq!(w.gh().stacks()[0].prs, vec![1, 2]);
    assert_eq!(w.gh().prs().len(), 2, "reused #2");
}

#[test]
fn without_a_terminal_sync_only_says_how_to_stack_it() {
    let w = unstacked_pr_on_a_local_branch();
    let out = w.gt_ok(&["sync"]);
    assert!(out.stderr.contains("run `gt sync` in a terminal"), "{out:?}");
    assert!(w.gh().stacks().is_empty());
    assert_eq!(w.local_stacks(), strings(&[&["feat-b", "feat-c"]]));
    assert_eq!(w.config("branch.feat-b.gtNoLink"), None);
}

#[test]
fn a_pr_already_stacked_on_github_is_shared_without_asking() {
    let w = unstacked_pr_on_a_local_branch();
    w.gh().add_stack(&[1, 2]);
    let (out, screen) = w.gt_with_tty(&["sync"], &[]);
    assert!(out.ok(), "{out:?}");
    assert!(!screen.contains(QUESTION), "{screen}");
    assert_eq!(w.shared_stacks(), strings(&[&["feat-a", "feat-b"]]));
    assert_eq!(w.local_stacks(), strings(&[&["feat-c"]]));
}

#[test]
fn a_pr_with_another_base_stays_local() {
    let w = pr_with_local_branches_above();
    w.git(&["push", "--quiet", "origin", "feat-b"]);
    w.gh().add_pr(FakePr::new("feat-b", "main"));
    let (out, screen) = w.gt_with_tty(&["sync"], &[]);
    assert!(out.ok(), "{out:?}");
    assert!(screen.is_empty(), "no prompt: {screen}");
    assert!(out.stderr.contains("feat-b's PR #2 is based on main, but it's stacked on feat-a here"), "{out:?}");
    assert_eq!(w.local_stacks(), strings(&[&["feat-b", "feat-c"]]));
}

#[test]
fn a_bottom_local_branch_with_a_pr_on_trunk_is_shared() {
    let w = World::with_github();
    w.create("feat-a", "a.rs", "a\n");
    w.create("feat-b", "b.rs", "b\n");
    w.git(&["push", "--quiet", "origin", "feat-a"]);
    w.gh().add_pr(FakePr::new("feat-a", "main"));
    w.gt_ok(&["sync"]);
    assert_eq!(w.shared_stacks(), strings(&[&["feat-a"]]));
    assert_eq!(w.local_stacks(), strings(&[&["feat-b"]]));
    assert_eq!(w.branch_state("feat-a")["pullRequest"]["number"], 1);
}

#[test]
fn gh_stack_adding_a_branch_takes_it_over() {
    let w = pr_with_local_branches_above();
    // As if `gh stack add feat-b` ran: gh-stack now lists it too.
    let mut s = w.state();
    s["stacks"][0]["branches"].as_array_mut().unwrap().push(serde_json::json!({ "branch": "feat-b" }));
    w.write_state(&s);
    w.gt_ok(&["create", "feat-d", "-m", "d"]);
    assert_eq!(w.shared_stacks(), strings(&[&["feat-a", "feat-b"]]));
    assert_eq!(w.local_stacks(), strings(&[&["feat-c", "feat-d"]]));
    assert_eq!(w.local_state()["stacks"][0]["on"], "feat-b");
}
