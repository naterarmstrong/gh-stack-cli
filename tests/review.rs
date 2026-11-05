//! `gt review` and `gt cloud`: pulling down PRs read-only, and how sync and ls treat them.

mod common;

use common::fake_github::FakePr;
use common::{World, line_of, write_in};

/// Pushes `branch` from the teammate clone, one commit on top of `base`.
fn push_branch(w: &World, branch: &str, base: &str, file: &str) -> String {
    let dir = w.teammate();
    w.git_in(&dir, &["checkout", "--quiet", "-B", branch, &format!("origin/{base}")]);
    write_in(&dir, file, &format!("{branch}\n"));
    w.git_in(&dir, &["add", "-A"]);
    w.git_in(&dir, &["commit", "--quiet", "-m", &format!("{branch} work")]);
    w.git_in(&dir, &["push", "--quiet", "origin", branch]);
    w.git_in(&dir, &["rev-parse", "HEAD"])
}

fn their_pr(w: &World, branch: &str, base: &str, file: &str) -> u64 {
    push_branch(w, branch, base, file);
    w.gh().add_pr(FakePr::new(branch, base))
}

#[test]
fn review_checks_out_a_pr_marked_read_only() {
    let w = World::with_github();
    let tip = push_branch(&w, "their-feature", "main", "t.rs");
    let n = w.gh().add_pr(FakePr::new("their-feature", "main"));

    let out = w.gt_ok(&["review", &n.to_string()]);
    assert!(out.stderr.contains("#1 Change their-feature by someone"), "{out:?}");
    assert_eq!(w.current(), "their-feature");
    assert_eq!(w.head("their-feature").as_deref(), Some(tip.as_str()));
    assert_eq!(w.config("branch.their-feature.gtReview").as_deref(), Some("1"));
    assert_eq!(w.config("branch.their-feature.gtReviewBase").as_deref(), Some("main"));
    assert_eq!(w.config("branch.their-feature.github-pr-owner-number").as_deref(), Some("acme#widgets#1"));
    assert_eq!(w.config("branch.their-feature.merge").as_deref(), Some("refs/heads/their-feature"));
    assert!(!w.state_path().exists() || w.stacks().is_empty(), "review branches aren't stacked");

    let ls = w.gt_ok(&["ls"]);
    assert!(ls.stdout.contains("Reviewing"), "{ls:?}");
    let line = line_of(&ls.stdout, "their-feature");
    assert!(line.starts_with('◉') && line.contains("#1") && line.contains("up to date"), "{ls:?}");
    assert!(line.contains("by Teammate"), "{ls:?}");
}

#[test]
fn review_by_branch_name_works_without_a_pr() {
    let w = World::with_github();
    push_branch(&w, "spike", "main", "s.rs");
    w.gt_ok(&["review", "spike"]);
    assert_eq!(w.current(), "spike");
    assert_eq!(w.config("branch.spike.gtReview").as_deref(), Some("true"));
    let out = w.gt_err(&["review", "77"]);
    assert!(out.stderr.contains("PR 77 not found"), "{out:?}");
}

#[test]
fn sync_resets_review_branches_to_the_pr_head() {
    let w = World::with_github();
    let n = their_pr(&w, "their-feature", "main", "t.rs");
    w.gt_ok(&["rv", &n.to_string()]);
    w.commit("t.rs", "my local poke\n", "Local poke");
    let theirs = w.teammate_push("their-feature", "t.rs", "rewritten\n", true);

    w.git(&["fetch", "--quiet", "origin"]);
    let ls = w.gt_ok(&["ls"]);
    assert!(line_of(&ls.stdout, "their-feature").contains("force-pushed (gt sync)"), "{ls:?}");

    let out = w.gt_ok(&["sync"]);
    assert!(out.stderr.contains("reset their-feature to the force-pushed PR head"), "{out:?}");
    assert_eq!(w.head("their-feature").as_deref(), Some(theirs.as_str()));
    assert_eq!(w.read("t.rs"), "rewritten\n", "the checkout moved with it");
    assert_eq!(w.remote_head("their-feature").as_deref(), Some(theirs.as_str()), "never pushed");
}

#[test]
fn sync_deletes_a_review_branch_once_its_pr_merges() {
    let w = World::with_github();
    let n = their_pr(&w, "their-feature", "main", "t.rs");
    w.gt_ok(&["review", &n.to_string()]);
    w.merge_pr(n);

    let out = w.gt_ok(&["sync"]);
    assert!(out.stderr.contains("deleted review branch their-feature (#1 merged)"), "{out:?}");
    assert_eq!(w.head("their-feature"), None);
    assert_eq!(w.current(), "main");
    assert_eq!(w.config("branch.their-feature.gtReview"), None);
}

#[test]
fn sync_no_prune_keeps_merged_review_branches() {
    let w = World::with_github();
    let n = their_pr(&w, "their-feature", "main", "t.rs");
    w.gt_ok(&["review", &n.to_string()]);
    w.merge_pr(n);
    w.gt_ok(&["sync", "--no-prune"]);
    assert!(w.head("their-feature").is_some());
}

#[test]
fn review_pulls_down_the_rest_of_a_stack_up_to_a_draft() {
    let w = World::with_github();
    their_pr(&w, "r1", "main", "r1.rs");
    let r2 = their_pr(&w, "r2", "r1", "r2.rs");
    their_pr(&w, "r3", "r2", "r3.rs");
    let r4 = their_pr(&w, "r4", "r3", "r4.rs");
    w.gh().update(r4, |p| p.draft = true);

    let out = w.gt_ok(&["review", &r2.to_string()]);
    assert!(out.stderr.contains("also pulling down #1"), "{out:?}");
    assert!(out.stderr.contains("also pulling down #3"), "{out:?}");
    assert!(out.stderr.contains(&format!("stopped at #{r4} r4: it is a draft")), "{out:?}");
    assert_eq!(w.current(), "r2");
    for b in ["r1", "r2", "r3"] {
        assert!(w.head(b).is_some(), "{b} pulled down");
        assert!(w.config(&format!("branch.{b}.gtReview")).is_some());
    }
    assert_eq!(w.head("r4"), None);

    // ls groups them as one stack, top first, above main.
    let ls = w.gt_ok(&["ls"]);
    let lines: Vec<&str> = ls.stdout.lines().skip_while(|l| !l.contains("Reviewing")).collect();
    let pos = |b: &str| lines.iter().position(|l| l.split_whitespace().any(|w| w == b)).unwrap();
    assert!(pos("r3") < pos("r2") && pos("r2") < pos("r1") && pos("r1") < pos("main"), "{ls:?}");
}

#[test]
fn review_stops_at_your_own_branches() {
    let w = World::with_github();
    w.create("mine", "m.rs", "m\n");
    w.gt_ok(&["submit"]);
    let theirs = their_pr(&w, "on-mine", "mine", "o.rs");
    w.git(&["checkout", "--quiet", "main"]);

    let out = w.gt_ok(&["review", &theirs.to_string()]);
    assert!(out.stderr.contains("stopped at #1 mine: it is one of your stacked branches"), "{out:?}");
    assert_eq!(w.config("branch.mine.gtReview"), None);

    let out = w.gt_err(&["review", "mine"]);
    assert!(out.stderr.contains("mine is one of your stacked branches"), "{out:?}");
}

#[test]
fn review_a_fork_pr_by_its_pull_ref() {
    let w = World::with_github();
    let dir = w.teammate();
    w.git_in(&dir, &["checkout", "--quiet", "-B", "fork-work", "origin/main"]);
    write_in(&dir, "f.rs", "from a fork\n");
    w.git_in(&dir, &["add", "-A"]);
    w.git_in(&dir, &["commit", "--quiet", "-m", "Fork work"]);
    let tip = w.git_in(&dir, &["rev-parse", "HEAD"]);
    let mut pr = FakePr::new("patch-1", "main");
    pr.cross_repo = true;
    let n = w.gh().add_pr(pr);
    w.git_in(&dir, &["push", "--quiet", "origin", &format!("HEAD:refs/pull/{n}/head")]);

    w.gt_ok(&["review", &n.to_string()]);
    let name = format!("pr/{n}");
    assert_eq!(w.current(), name);
    assert_eq!(w.head(&name).as_deref(), Some(tip.as_str()));
    assert_eq!(w.git(&["rev-parse", &format!("refs/gt/pull/{n}")]), tip);
    let ls = w.gt_ok(&["ls"]);
    assert!(line_of(&ls.stdout, &name).contains("up to date"), "{ls:?}");
}

#[test]
fn review_without_a_target_lists_requests() {
    let w = World::with_github();
    let out = w.gt_ok(&["review"]);
    assert!(out.stdout.contains("No open PRs are waiting on your review"), "{out:?}");

    let n = their_pr(&w, "asks-me", "main", "a.rs");
    w.gh().update(n, |p| p.review_requested = true);
    // There is one now, and picking needs a terminal.
    let out = w.gt_err(&["review"]);
    assert!(out.stderr.contains("terminal"), "{out:?}");
}

#[test]
fn cloud_lists_your_prs_with_no_local_branch() {
    let w = World::with_github();
    let none = "All your open PRs already have local branches";

    // Someone else's PR isn't listed, even if it asks for your review.
    let theirs = their_pr(&w, "theirs", "main", "t.rs");
    w.gh().update(theirs, |p| p.review_requested = true);
    let out = w.gt_ok(&["cloud"]);
    assert!(out.stdout.contains(none), "{out:?}");

    // Nor is one of yours that you already have locally.
    w.create("mine", "m.rs", "m\n");
    w.gt_ok(&["submit"]);
    let mine = w.gh().pr_for("mine").number;
    w.gh().update(mine, |p| p.author = "me".into());
    let out = w.gt_ok(&["cloud"]);
    assert!(out.stdout.contains(none), "{out:?}");

    // One you don't is, and picking needs a terminal.
    let agents = their_pr(&w, "agents", "main", "a.rs");
    w.gh().update(agents, |p| p.author = "me".into());
    let out = w.gt_err(&["cloud"]);
    assert!(out.stderr.contains("terminal"), "{out:?}");

    // Once pulled down, it has a local branch too.
    w.gt_ok(&["cloud", &agents.to_string()]);
    let out = w.gt_ok(&["cloud"]);
    assert!(out.stdout.contains(none), "{out:?}");
}

#[test]
fn cloud_checks_a_fork_pr_against_its_pr_branch() {
    let w = World::with_github();
    // Its head shares a name with your local main, but it'd land on pr/1.
    let n = w.gh().add_pr(FakePr::new("main", "main"));
    w.gh().update(n, |p| {
        p.author = "me".into();
        p.cross_repo = true;
    });
    let out = w.gt_err(&["cloud"]);
    assert!(out.stderr.contains("terminal"), "{out:?}");
}

#[test]
fn cloud_pulls_down_drafts_on_both_sides_of_a_pr() {
    let w = World::with_github();
    let r1 = their_pr(&w, "r1", "main", "r1.rs");
    let r2 = their_pr(&w, "r2", "r1", "r2.rs");
    let r3 = their_pr(&w, "r3", "r2", "r3.rs");
    let r4 = their_pr(&w, "r4", "r3", "r4.rs");
    let r5 = their_pr(&w, "r5", "r4", "r5.rs");
    for n in [r2, r4] {
        w.gh().update(n, |p| p.draft = true);
    }

    let out = w.gt_ok(&["cloud", &r3.to_string()]);
    for n in [r1, r2, r4, r5] {
        assert!(out.stderr.contains(&format!("also pulling down #{n}")), "{out:?}");
    }
    assert_eq!(w.current(), "r3");
    for b in ["r1", "r2", "r3", "r4", "r5"] {
        assert!(w.head(b).is_some(), "{b} pulled down");
        assert!(w.config(&format!("branch.{b}.gtReview")).is_some());
    }
}

#[test]
fn cloud_leaves_the_same_state_as_review_and_syncs_agent_pushes() {
    let w = World::with_github();
    let n = their_pr(&w, "agent-work", "main", "a.rs");
    w.gh().update(n, |p| p.author = "me".into());

    w.gt_ok(&["cloud", &n.to_string()]);
    assert_eq!(w.current(), "agent-work");
    assert_eq!(w.config("branch.agent-work.gtReview").as_deref(), Some("1"));
    assert_eq!(w.config("branch.agent-work.gtReviewBase").as_deref(), Some("main"));
    assert_eq!(w.config("branch.agent-work.github-pr-owner-number").as_deref(), Some("acme#widgets#1"));
    assert!(!w.state_path().exists() || w.stacks().is_empty(), "cloud branches aren't stacked");
    assert!(w.gt_ok(&["ls"]).stdout.contains("Reviewing"));

    let pushed = w.teammate_push("agent-work", "a.rs", "rewritten\n", true);
    w.gt_ok(&["sync"]);
    assert_eq!(w.head("agent-work").as_deref(), Some(pushed.as_str()));

    w.merge_pr(n);
    w.gt_ok(&["sync"]);
    assert_eq!(w.head("agent-work"), None);
}
