//! Flows that talk to GitHub, against the fake in `common/fake_github.rs`:
//! submit, sync, get, clear-merged and log.

mod common;

use common::{World, line_of, write_in};
use std::path::PathBuf;

/// feat-a ← feat-b, submitted: two PRs (#1, #2) linked as GitHub stack #1.
fn submitted() -> World {
    let w = World::with_github();
    w.create("feat-a", "a.rs", "a\n");
    w.create("feat-b", "b.rs", "b\n");
    w.gt_ok(&["submit"]);
    w
}

/// Another clone of origin set up like the first, for `gt get`.
fn second_clone(w: &World) -> PathBuf {
    let dir = w.tmp.path().join("other");
    w.git_in(w.tmp.path(), &["clone", "--quiet", w.origin.to_str().unwrap(), dir.to_str().unwrap()]);
    w.git_in(&dir, &["config", "remote.origin.url", &common::remote_url()]);
    w.git_in(&dir, &["config", &format!("url.{}.insteadOf", w.origin.display()), &common::remote_url()]);
    dir
}

#[test]
fn submit_pushes_opens_prs_and_links_the_stack() {
    let w = World::with_github();
    w.write("a.rs", "a\n");
    w.gt_ok(&["create", "feat-a", "-am", "Add a\n\nWhy a matters."]);
    w.create("feat-b", "b.rs", "b\n");
    let out = w.gt_ok(&["submit", "--draft"]);

    assert_eq!(w.remote_head("feat-a"), w.head("feat-a"));
    assert_eq!(w.remote_head("feat-b"), w.head("feat-b"));
    let prs = w.gh().prs();
    assert_eq!(prs.len(), 2);
    let a = w.gh().pr_for("feat-a");
    let b = w.gh().pr_for("feat-b");
    assert_eq!(
        (a.base.as_str(), a.title.as_str(), a.body.as_str(), a.draft),
        ("main", "Add a", "Why a matters.", true)
    );
    assert_eq!((b.base.as_str(), b.title.as_str()), ("feat-a", "Add b.rs"));

    let stacks = w.gh().stacks();
    assert_eq!(stacks.len(), 1);
    assert_eq!(stacks[0].prs, vec![a.number, b.number]);

    let s = w.state();
    assert_eq!(s["stacks"][0]["number"], 1);
    assert_eq!(w.branch_state("feat-a")["pullRequest"]["number"], a.number);
    assert_eq!(w.branch_state("feat-b")["pullRequest"]["url"], "https://github.com/acme/widgets/pull/2");
    assert_eq!(w.config("branch.feat-a.gtSynced"), w.head("feat-a"));
    assert_eq!(w.config("branch.feat-b.merge").as_deref(), Some("refs/heads/feat-b"));
    // Top of the stack first.
    assert!(out.stdout.find("pull/2").unwrap() < out.stdout.find("pull/1").unwrap(), "{out:?}");
    assert!(out.stderr.contains("linked stack #1"), "{out:?}");

    let ls = w.gt_ok(&["ls"]);
    let line = line_of(&ls.stdout, "feat-a");
    assert!(line.contains("#1") && !line.contains("pushed"), "{ls:?}");
}

#[test]
fn resubmitting_force_pushes_without_new_prs() {
    let w = submitted();
    w.gt_ok(&["bottom"]);
    w.write("a.rs", "a2\n");
    w.gt_ok(&["modify", "-a"]);
    let ls = w.gt_ok(&["ls"]);
    assert!(line_of(&ls.stdout, "feat-a").contains("needs push"), "{ls:?}");
    w.gh().clear_requests();

    w.gt_ok(&["submit"]);
    assert_eq!(w.remote_head("feat-a"), w.head("feat-a"));
    assert_eq!(w.remote_head("feat-b"), w.head("feat-b"));
    assert_eq!(w.gh().prs().len(), 2);
    assert_eq!(w.gh().stacks().len(), 1);
    assert!(w.gh().queries().iter().all(|q| !q.starts_with("mutation")), "nothing to create or retarget");
}

#[test]
fn submit_retargets_and_relinks_after_an_insert() {
    let w = submitted();
    w.gt_ok(&["bottom"]);
    w.create("feat-mid", "m.rs", "m\n");
    let out = w.gt_ok(&["submit"]);
    assert!(out.stderr.contains("retargeting #2 onto feat-mid"), "{out:?}");
    let mid = w.gh().pr_for("feat-mid");
    assert_eq!(mid.base, "feat-a");
    assert_eq!(w.gh().pr(2).base, "feat-mid");
    // GitHub's stack is [1, 2]; local is [1, 3, 2], which only --relink fixes.
    assert!(out.stderr.contains("--relink"), "{out:?}");
    assert_eq!(w.gh().stacks()[0].prs, vec![1, 2]);

    w.gt_ok(&["submit", "--relink"]);
    let stacks = w.gh().stacks();
    assert_eq!(stacks.len(), 1);
    assert_eq!(stacks[0].prs, vec![1, mid.number, 2]);
    assert_eq!(w.state()["stacks"][0]["number"], stacks[0].number);
}

#[test]
fn submit_adds_new_top_branches_to_the_existing_stack() {
    let w = submitted();
    w.create("feat-c", "c.rs", "c\n");
    w.gt_ok(&["submit"]);
    let c = w.gh().pr_for("feat-c");
    assert_eq!(c.base, "feat-b");
    assert_eq!(w.gh().stacks()[0].prs, vec![1, 2, c.number]);
}

#[test]
fn submit_wont_overwrite_a_push_it_never_saw() {
    let w = submitted();
    w.teammate_push("feat-a", "a.rs", "theirs\n", true);
    w.git(&["fetch", "--quiet", "origin"]);
    w.gt_ok(&["bottom"]);
    w.write("a.rs", "mine\n");
    w.gt_ok(&["modify", "-a"]);

    let out = w.gt_err(&["submit"]);
    assert!(out.stderr.contains("feat-a changed on origin since gt last synced"), "{out:?}");
    assert_eq!(w.show("origin/feat-a", "a.rs").as_deref(), Some("theirs\n"));
    assert_eq!(w.show(&w.remote_head("feat-a").unwrap(), "a.rs").as_deref(), Some("theirs\n"));
}

#[test]
fn sync_prunes_a_merged_bottom_and_restacks_onto_trunk() {
    let w = submitted();
    let landed = w.merge_pr(1);
    let out = w.gt_ok(&["sync"]);

    assert!(out.stderr.contains("feat-a merged (#1)"), "{out:?}");
    assert!(out.stderr.contains("deleted merged branch feat-a"), "{out:?}");
    assert_eq!(w.head("feat-a"), None);
    assert_eq!(w.head("main").as_deref(), Some(landed.as_str()), "local trunk fast-forwarded");
    assert_eq!(w.parent("feat-b"), landed);
    assert_eq!(w.count("main..feat-b"), 1);
    assert!(out.stderr.contains("feat-b changed locally and aren't pushed"), "{out:?}");
    assert_ne!(w.remote_head("feat-b"), w.head("feat-b"), "sync never pushes");
    assert_eq!(w.branch_state("feat-a")["pullRequest"]["merged"], true);

    // Submit now pushes the restack and points #2 at main.
    let out = w.gt_ok(&["submit"]);
    assert!(out.stderr.contains("retargeting #2 onto main"), "{out:?}");
    assert_eq!(w.gh().pr(2).base, "main");
    assert_eq!(w.remote_head("feat-b"), w.head("feat-b"));
}

#[test]
fn sync_moves_off_a_merged_branch_before_deleting_it() {
    let w = submitted();
    w.merge_pr(1);
    w.gt_ok(&["bottom"]);
    let out = w.gt_ok(&["sync"]);
    assert!(out.stderr.contains("moved to feat-b"), "{out:?}");
    assert_eq!(w.current(), "feat-b");
    assert_eq!(w.head("feat-a"), None);
}

#[test]
fn sync_reports_a_fully_merged_stack() {
    let w = submitted();
    w.merge_pr(1);
    w.gt_ok(&["sync"]);
    w.gt_ok(&["submit"]);
    w.merge_pr(2);
    let out = w.gt_ok(&["sync"]);
    assert!(out.stderr.contains("stack on main is fully merged"), "{out:?}");
    assert_eq!(w.current(), "main");
    assert_eq!(w.head("feat-b"), None);
    assert_eq!(w.show("main", "b.rs").as_deref(), Some("b\n"));
}

#[test]
fn sync_keeps_a_merged_branch_that_differs_from_what_merged() {
    let w = submitted();
    w.merge_pr(1);
    w.gt_ok(&["bottom"]);
    w.write("a.rs", "unpushed\n");
    w.gt_ok(&["modify", "-a"]);
    let out = w.gt_ok(&["sync"]);
    assert!(out.stderr.contains("feat-a merged at"), "{out:?}");
    assert!(out.stderr.contains("keeping it"), "{out:?}");
    assert!(w.head("feat-a").is_some());
}

#[test]
fn sync_takes_a_teammates_push_when_local_is_unedited() {
    let w = submitted();
    let theirs = w.teammate_push("feat-a", "a.rs", "fixed by bot\n", false);
    let out = w.gt_ok(&["sync"]);
    assert!(out.stderr.contains("pulled remote changes into feat-a"), "{out:?}");

    // Folded back into one commit, on the same base, with the bot's change.
    assert_eq!(w.count("main..feat-a"), 1);
    assert_eq!(w.show("feat-a", "a.rs").as_deref(), Some("fixed by bot\n"));
    assert_eq!(w.config("branch.feat-a.gtSynced").as_deref(), Some(theirs.as_str()));
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());
    assert_eq!(w.read("b.rs"), "b\n");
}

#[test]
fn sync_keeps_local_when_both_sides_changed() {
    let w = submitted();
    w.teammate_push("feat-a", "a.rs", "theirs\n", true);
    w.gt_ok(&["bottom"]);
    w.write("a.rs", "mine\n");
    w.gt_ok(&["modify", "-a"]);

    let out = w.gt_ok(&["sync"]);
    assert!(out.stderr.contains("feat-a changed both locally and on origin"), "{out:?}");
    assert_eq!(w.show("feat-a", "a.rs").as_deref(), Some("mine\n"));

    let out = w.gt_ok(&["sync", "--prefer-remote"]);
    assert!(out.stderr.contains("pulled remote changes into feat-a"), "{out:?}");
    assert_eq!(w.show("feat-a", "a.rs").as_deref(), Some("theirs\n"));
}

#[test]
fn sync_uses_githubs_push_history_without_a_sync_record() {
    let w = submitted();
    w.git(&["config", "--unset", "branch.feat-a.gtSynced"]);
    let theirs = w.teammate_push("feat-a", "a.rs", "force-pushed\n", true);
    // Some other fetch already brought the new commit in, so the tracking ref says nothing.
    w.git(&["fetch", "--quiet", "origin"]);

    let out = w.gt_ok(&["sync"]);
    assert!(out.stderr.contains("pulled remote changes into feat-a"), "{out:?}");
    assert_eq!(w.head("feat-a").as_deref(), Some(theirs.as_str()));
    assert!(w.gh().queries().iter().any(|q| q.contains("HEAD_REF_FORCE_PUSHED_EVENT")), "asked for push history");
}

#[test]
fn sync_skips_a_conflicting_branch_and_marks_it() {
    let w = submitted();
    w.advance_main("a.rs", "conflicting\n");
    let a = w.head("feat-a").unwrap();
    let out = w.gt_ok(&["sync"]);

    assert!(out.stderr.contains("skipped feat-a: it conflicts with main"), "{out:?}");
    assert!(out.stderr.contains("need a rebase"), "{out:?}");
    assert_eq!(w.head("feat-a").as_deref(), Some(a.as_str()), "left where it was");
    assert_eq!(w.config("branch.feat-a.gtNeedsRebase").as_deref(), Some(a.as_str()));
    assert_eq!(w.parent("feat-b"), a);
    assert!(!w.common_dir().join("rebase-merge").exists(), "sync never stops for a conflict");
    let ls = w.gt_ok(&["ls"]);
    assert!(line_of(&ls.stdout, "feat-a").contains("needs rebase"), "{ls:?}");

    // Fixing it by hand: restack --trunk starts a rebase, abort leaves the mark.
    w.gt_ok(&["bottom"]);
    assert_eq!(w.gt(&["restack", "--trunk"]).code, 1);
    w.write("a.rs", "resolved\n");
    w.git(&["add", "a.rs"]);
    w.gt_ok(&["continue"]);
    // The mark only counts while the tip is unchanged. (Its config key outlives it for the
    // bottom branch, which the follow-up restack never lists as clean.)
    let ls = w.gt_ok(&["ls"]);
    assert!(!ls.stdout.contains("needs rebase"), "{ls:?}");
    assert_eq!(w.parent("feat-a"), w.git(&["rev-parse", "origin/main"]));
    assert_eq!(w.parent("feat-b"), w.head("feat-a").unwrap());
}

#[test]
fn sync_offline_never_calls_github() {
    let w = submitted();
    let landed = w.advance_main("m.rs", "m\n");
    w.gh().clear_requests();
    w.gt_ok(&["sync", "--offline"]);
    assert!(w.gh().requests().is_empty(), "{:?}", w.gh().requests());
    assert_eq!(w.parent("feat-a"), landed);
}

#[test]
fn sync_only_touches_the_current_stack_unless_all() {
    let w = submitted();
    w.git(&["checkout", "--quiet", "main"]);
    w.create("other", "o.rs", "o\n");
    let landed = w.advance_main("m.rs", "m\n");
    let a = w.head("feat-a").unwrap();

    w.gt_ok(&["sync"]);
    assert_eq!(w.parent("other"), landed);
    assert_eq!(w.head("feat-a").unwrap(), a);

    w.gt_ok(&["sync", "--all"]);
    assert_eq!(w.parent("feat-a"), landed);
}

#[test]
fn get_pulls_down_a_whole_stack() {
    let w = submitted();
    let other = second_clone(&w);
    let out = w.gt_in(&other, &["get", "2"]);
    assert!(out.ok(), "{out:?}");
    assert!(out.stderr.contains("tracked 2 branch(es) on main"), "{out:?}");

    let branch = w.git_in(&other, &["branch", "--show-current"]);
    assert_eq!(branch, "feat-b");
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(other.join(".git/gh-stack")).unwrap()).unwrap();
    assert_eq!(state["stacks"][0]["number"], 1);
    let names: Vec<&str> =
        state["stacks"][0]["branches"].as_array().unwrap().iter().map(|b| b["branch"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["feat-a", "feat-b"]);
    assert_eq!(w.git_in(&other, &["rev-parse", "feat-a"]), w.head("feat-a").unwrap());
    assert_eq!(w.git_in(&other, &["config", "branch.feat-a.gtSynced"]), w.head("feat-a").unwrap());

    // Already tracked: just checks it out.
    let out = w.gt_in(&other, &["get", "feat-a"]);
    assert!(out.ok() && out.stderr.contains("already tracked"), "{out:?}");
    assert_eq!(w.git_in(&other, &["branch", "--show-current"]), "feat-a");
}

#[test]
fn get_a_lone_pr_and_edit_it() {
    let w = World::with_github();
    let dir = w.teammate();
    w.git_in(&dir, &["checkout", "--quiet", "-b", "their-fix"]);
    write_in(&dir, "fix.rs", "fix\n");
    w.git_in(&dir, &["add", "-A"]);
    w.git_in(&dir, &["commit", "--quiet", "-m", "Their fix"]);
    w.git_in(&dir, &["push", "--quiet", "origin", "their-fix"]);
    let n = w.gh().add_pr(common::fake_github::FakePr::new("their-fix", "main"));

    w.gt_ok(&["get", &format!("https://github.com/acme/widgets/pull/{n}")]);
    assert_eq!(w.current(), "their-fix");
    assert_eq!(w.stacks(), vec![vec!["their-fix"]]);
    w.write("fix.rs", "better fix\n");
    w.gt_ok(&["modify", "-a"]);
    w.gt_ok(&["submit"]);
    assert_eq!(w.show("origin/their-fix", "fix.rs").as_deref(), Some("better fix\n"));
    assert_eq!(w.gh().prs().len(), 1, "reused the existing PR");

    let out = w.gt_err(&["get", "99"]);
    assert!(out.stderr.contains("PR #99 not found"), "{out:?}");
}

#[test]
fn clear_merged_deletes_exactly_merged_branches() {
    let w = World::with_github();
    let push_branch = |name: &str, file: &str| {
        w.git(&["checkout", "--quiet", "-b", name, "main"]);
        w.commit(file, "x\n", name);
        w.git(&["push", "--quiet", "origin", name]);
        w.gh().add_pr(common::fake_github::FakePr::new(name, "main"))
    };
    let done = push_branch("done", "d.rs");
    let diverged = push_branch("diverged", "v.rs");
    push_branch("still-open", "s.rs");
    w.gh().merge(done);
    w.gh().merge(diverged);
    w.git(&["checkout", "--quiet", "diverged"]);
    w.commit("v.rs", "local only\n", "After merge");
    w.git(&["checkout", "--quiet", "done"]);

    let out = w.gt_ok(&["clear-merged", "-f"]);
    assert!(out.stderr.contains("diverged: #2 merged, but local"), "{out:?}");
    assert!(out.stderr.contains("1 local commit(s) not in the PR"), "{out:?}");
    assert_eq!(w.head("done"), None);
    assert_eq!(w.current(), "main", "moved off the deleted branch");
    assert!(w.head("diverged").is_some());
    assert!(w.head("still-open").is_some());
    assert!(w.remote_head("done").is_some(), "remote branches stay");

    w.gt_ok(&["cm", "-f", "-D"]);
    assert_eq!(w.head("diverged"), None);
    assert!(w.head("still-open").is_some());

    let out = w.gt_ok(&["cm", "-f"]);
    assert!(out.stderr.contains("no merged branches to delete"), "{out:?}");
}

#[test]
fn log_shows_pr_status() {
    let w = World::with_github();
    w.create("feat-a", "a.rs", "a\n");
    w.create("feat-b", "b.rs", "b\n");
    w.gt_ok(&["submit", "--draft"]);
    w.gh().update(1, |p| {
        p.draft = false;
        p.checks = Some("SUCCESS".into());
        p.review_decision = Some("APPROVED".into());
    });
    w.gh().update(2, |p| p.checks = Some("FAILURE".into()));

    let out = w.gt_ok(&["log"]);
    let a = line_of(&out.stdout, "feat-a");
    assert!(a.contains("#1") && a.contains("✓ checks") && a.contains("approved") && !a.contains("draft"), "{a}");
    let b = line_of(&out.stdout, "feat-b");
    assert!(b.contains("#2") && b.contains("draft") && b.contains("✗ checks"), "{b}");
    assert!(b.contains("Add b.rs"), "{b}");
}
