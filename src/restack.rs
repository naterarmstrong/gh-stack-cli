//! In-memory restacking. With one commit per branch, a branch's old base is
//! simply its tip's parent, so each branch is replayed with `git merge-tree`
//! + `git commit-tree` without touching any working tree.
//!
//! A branch with several commits is replayed the same way, a commit at a time,
//! from the parent of its oldest commit that isn't below it.

use crate::ctx::Ctx;
use crate::git::{self, Merge};
use crate::ui;
use anyhow::{Result, bail};

pub struct Update {
    pub branch: String,
    pub old: String,
    pub new: String,
}

pub struct Conflict {
    pub branch: String,
    pub onto: String,
    pub onto_name: String,
    pub base: String,
    pub files: Vec<String>,
}

pub struct Plan {
    pub updates: Vec<Update>,
    pub conflicts: Vec<Conflict>,
    /// Branches that were checked and replay cleanly (moved or not).
    pub clean: Vec<String>,
}

/// Computes new commits for every unmerged branch in stack `si`. When
/// `onto_trunk` is set the bottom branch moves onto the remote trunk tip;
/// otherwise it stays where it is and only the branches above it move.
///
/// By default planning stops at the first conflict. With `skip_conflicts` a
/// conflicting branch is left where it is and the branches above it are
/// replayed onto it instead.
pub fn plan(ctx: &Ctx, si: usize, onto_trunk: bool, skip_conflicts: bool) -> Result<Plan> {
    plan_below(ctx, si, onto_trunk, skip_conflicts, ctx.store.file.stacks[si].branches.len())
}

/// `plan`, for only the branches of stack `si` below index `end`.
fn plan_below(ctx: &Ctx, si: usize, onto_trunk: bool, skip_conflicts: bool, end: usize) -> Result<Plan> {
    let stack = &ctx.store.file.stacks[si];
    let mut parent: Option<(String, String)> =
        if onto_trunk { Some((stack.trunk.branch.clone(), ctx.trunk_tip(&stack.trunk.branch)?)) } else { None };
    let mut old_parent_head: Option<String> = None;
    let mut updates = vec![];
    let mut conflicts = vec![];
    let mut clean = vec![];
    let mut below = below_seed(ctx, stack);

    for b in stack.branches[..end].iter() {
        if b.merged() {
            below.push(b.head.clone());
            continue;
        }
        let Some(c) = ctx.refs.heads.get(&b.branch) else {
            bail!("branch {} is in the stack but missing locally (try `gt get` or `gt sync`)", b.branch);
        };
        below.extend(ctx.rebase_base(&b.branch).map(str::to_string));
        let Some((parent_name, parent_tip)) = parent.clone() else {
            below.extend([b.head.clone(), c.sha.clone()]);
            parent = Some((b.branch.clone(), c.sha.clone()));
            old_parent_head = Some(c.sha.clone());
            continue;
        };

        let new = if old_parent_head.as_deref() == Some(&c.sha) || c.sha == parent_tip {
            // Branch has no commit of its own yet; it just follows its parent.
            parent_tip.clone()
        } else if c.parents.len() == 1 && c.parents[0] == parent_tip {
            c.sha.clone()
        } else {
            let own = git::own_chain(&c.sha, &below)?;
            if own.iter().any(|(_, parents)| parents.len() != 1) {
                bail!("{} has a merge or root commit of its own; run `gt squash` to fold it", b.branch);
            }
            match own.first() {
                // Nothing of its own above what's below it: it follows its parent.
                None => parent_tip.clone(),
                Some((_, p)) if p[0] == parent_tip => c.sha.clone(),
                Some((_, p)) => {
                    let base = p[0].clone();
                    match replay(&own, &parent_tip)? {
                        Ok(new) => new,
                        Err(files) => {
                            conflicts.push(Conflict {
                                branch: b.branch.clone(),
                                onto: parent_tip,
                                onto_name: parent_name,
                                base,
                                files,
                            });
                            if !skip_conflicts {
                                return Ok(Plan { updates, conflicts, clean });
                            }
                            // Leave it where it is; the branches above follow it there.
                            below.extend([b.head.clone(), c.sha.clone()]);
                            old_parent_head = Some(c.sha.clone());
                            parent = Some((b.branch.clone(), c.sha.clone()));
                            continue;
                        }
                    }
                }
            }
        };
        clean.push(b.branch.clone());
        if new != c.sha {
            updates.push(Update { branch: b.branch.clone(), old: c.sha.clone(), new: new.clone() });
        }
        below.extend([b.head.clone(), c.sha.clone(), new.clone()]);
        old_parent_head = Some(c.sha.clone());
        parent = Some((b.branch.clone(), new));
    }
    Ok(Plan { updates, conflicts, clean })
}

/// Replays a branch's own commits (oldest first, each with its parents) onto
/// `onto`, one at a time, keeping each commit's message and author. Gives the
/// new tip, or the conflicting files of the first commit that doesn't apply.
pub fn replay(own: &[(String, Vec<String>)], onto: &str) -> Result<std::result::Result<String, Vec<String>>> {
    let mut tip = onto.to_string();
    for (sha, parents) in own {
        match git::merge_tree(&parents[0], &tip, sha)? {
            Merge::Clean(tree) => tip = git::commit_tree(&tree, &tip, &git::commit_meta(sha)?)?,
            Merge::Conflict(files) => return Ok(Err(files)),
        }
    }
    Ok(Ok(tip))
}

/// Commits that can never belong to a stacked branch: the trunk, local and remote.
fn below_seed(ctx: &Ctx, stack: &crate::state::Stack) -> Vec<String> {
    let t = &stack.trunk.branch;
    [ctx.refs.remote.get(t).cloned(), ctx.refs.head(t).map(str::to_string), Some(stack.trunk.head.clone())]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect()
}

/// Folds every multi-commit branch in stack `si` into a single commit. Each
/// branch keeps its exact tree; only history is rewritten, so this never conflicts.
pub fn squash(ctx: &Ctx, si: usize) -> Result<Vec<Update>> {
    let stack = &ctx.store.file.stacks[si];
    let trunk_tip = ctx.trunk_tip(&stack.trunk.branch)?;
    let mut below = below_seed(ctx, stack);
    let mut updates = vec![];
    // (old tip, new tip) of the branch below.
    let mut parent: Option<(String, String)> = None;
    for b in stack.branches.iter() {
        if b.merged() {
            below.push(b.head.clone());
            continue;
        }
        let Some(c) = ctx.refs.heads.get(&b.branch) else {
            bail!("branch {} is in the stack but missing locally", b.branch);
        };
        below.extend(ctx.rebase_base(&b.branch).map(str::to_string));
        let own = git::own_commits(&c.sha, &below)?;
        let new_parent = match &parent {
            None => git::merge_base(&c.sha, &trunk_tip)
                .ok_or_else(|| anyhow::anyhow!("{} shares no history with {}", b.branch, stack.trunk.branch))?,
            Some((old, new)) => {
                if !git::is_ancestor(old, &c.sha) {
                    bail!("{} doesn't contain its parent's current tip; rebase it by hand before squashing", b.branch);
                }
                new.clone()
            }
        };
        let in_place = own.len() == 1 && c.parents.first() == Some(&new_parent);
        let new = if in_place || own.is_empty() {
            c.sha.clone()
        } else {
            git::commit_tree(&git::tree_of(&c.sha)?, &new_parent, &git::commit_meta(&own[0])?)?
        };
        if own.len() > 1 {
            ui::info(&format!("folding {} commits on {}", own.len(), ui::bold(&b.branch)));
        }
        if new != c.sha {
            updates.push(Update { branch: b.branch.clone(), old: c.sha.clone(), new: new.clone() });
        }
        below.extend([b.head.clone(), c.sha.clone()]);
        parent = Some((c.sha.clone(), new));
    }
    Ok(updates)
}

/// Applies updates bottom-up, stopping at the first failure so the stack never
/// points at commits whose parents weren't applied.
pub fn apply(ctx: &mut Ctx, updates: &[Update]) -> Result<()> {
    for u in updates {
        ctx.set_branch(&u.branch, Some(&u.old), &u.new, "gt restack")?;
        ui::ok(&format!("restacked {} {} → {}", ui::bold(&u.branch), ui::dim(ui::short(&u.old)), ui::short(&u.new)));
    }
    ctx.reload()?;
    record_heads(ctx);
    Ok(())
}

/// Mirrors gh-stack's bookkeeping: each branch's head and the parent head it sits on.
pub fn record_heads(ctx: &mut Ctx) {
    let refs = &ctx.refs;
    for stack in ctx.store.file.stacks.iter_mut() {
        if let Some(tip) =
            refs.remote.get(&stack.trunk.branch).or_else(|| refs.heads.get(&stack.trunk.branch).map(|c| &c.sha))
        {
            stack.trunk.head = tip.clone();
        }
        // The tip of the nearest unmerged branch below, which a restacked branch sits on.
        let mut below = stack.trunk.head.clone();
        for b in stack.branches.iter_mut().filter(|b| !b.merged()) {
            if let Some(c) = refs.heads.get(&b.branch) {
                b.head = c.sha.clone();
                let first = c.parents.first();
                if first.is_some_and(|p| *p != below) && c.sha != below && git::is_ancestor(&below, &c.sha) {
                    // Several commits of its own, on the branch below.
                    b.base = below.clone();
                } else if let Some(p) = first {
                    b.base = p.clone();
                }
                below = c.sha.clone();
            }
        }
    }
}

/// Runs a full restack. On conflict, applies everything below it and leaves
/// the conflicting branch mid-rebase so the user can resolve it.
pub fn run(ctx: &mut Ctx, si: usize, onto_trunk: bool) -> Result<bool> {
    let plan = plan(ctx, si, onto_trunk, false)?;
    apply(ctx, &plan.updates)?;
    clear_marks(ctx, &plan.clean);
    match plan.conflicts.first() {
        None => Ok(true),
        Some(c) => {
            start_conflict(ctx, c)?;
            Ok(false)
        }
    }
}

/// Restacks everything that replays cleanly and leaves each conflicting
/// branch where it is, marked as needing a rebase. Returns the skipped branches.
pub fn run_skipping(ctx: &mut Ctx, si: usize, onto_trunk: bool) -> Result<Vec<String>> {
    let plan = plan(ctx, si, onto_trunk, true)?;
    apply(ctx, &plan.updates)?;
    clear_marks(ctx, &plan.clean);
    for c in &plan.conflicts {
        ctx.mark_needs_rebase(&c.branch, Some(&c.base))?;
        ui::warn(&format!(
            "skipped {}: it conflicts with {} in\n{}",
            ui::bold(&c.branch),
            ui::bold(&c.onto_name),
            c.files.iter().map(|f| format!("    {}", ui::red(f))).collect::<Vec<_>>().join("\n")
        ));
    }
    Ok(plan.conflicts.into_iter().map(|c| c.branch).collect())
}

/// Restacks the branches below the nearest one that needs a rebase or would
/// conflict, marking it if it wasn't already. That branch and everything above
/// it stay where they are. Returns its index, or the stack's length if there's none.
pub fn run_below_conflict(ctx: &mut Ctx, si: usize) -> Result<usize> {
    let stack = &ctx.store.file.stacks[si];
    let marked = stack.active().find(|(_, b)| ctx.needs_rebase(&b.branch)).map_or(stack.branches.len(), |(i, _)| i);
    let plan = plan_below(ctx, si, false, false, marked)?;
    let stop = plan.conflicts.first().and_then(|c| stack.index_of(&c.branch)).unwrap_or(marked);
    apply(ctx, &plan.updates)?;
    clear_marks(ctx, &plan.clean);
    if let Some(c) = plan.conflicts.first() {
        ctx.mark_needs_rebase(&c.branch, Some(&c.base))?;
    }
    Ok(stop)
}

fn clear_marks(ctx: &mut Ctx, branches: &[String]) {
    for b in branches {
        ctx.clear_needs_rebase(b);
    }
}

/// Where `start_conflict` notes the branch to return to on `gt abort`, the
/// branch being rebased, and the commit its own commits sat on:
/// `<git-dir>/gt-rebase`, one per line.
pub fn abort_file(git_dir: &std::path::Path) -> std::path::PathBuf {
    git_dir.join("gt-rebase")
}

fn start_conflict(ctx: &mut Ctx, c: &Conflict) -> Result<()> {
    ui::warn(&format!(
        "conflict restacking {} onto {}:\n{}",
        ui::bold(&c.branch),
        ui::bold(&c.onto_name),
        c.files.iter().map(|f| format!("    {}", ui::red(f))).collect::<Vec<_>>().join("\n")
    ));
    let rebase_cmd = format!("git rebase --onto {} {}", ui::short(&c.onto), ui::short(&c.base));
    if let Some(wt) = ctx.other_worktree(&c.branch) {
        eprintln!(
            "\n{} is checked out in {}. Resolve it there:\n    cd {} && {rebase_cmd}\nthen run `gt continue`.",
            c.branch,
            wt.display(),
            wt.display()
        );
        return Ok(());
    }
    if !git::is_clean(None)? {
        eprintln!(
            "\nWorking tree is dirty. Commit or stash, then:\n    git checkout {} && {rebase_cmd}\nand run `gt continue`.",
            c.branch
        );
        return Ok(());
    }
    let orig = ctx.info.current_branch.clone().unwrap_or_default();
    std::fs::write(abort_file(&ctx.info.git_dir), format!("{orig}\n{}\n{}\n", c.branch, c.base))?;
    if orig != c.branch {
        git::run(&["checkout", "--quiet", &c.branch])?;
    }
    git::run_raw(None, &["rebase", "--onto", &c.onto, &c.base], None, &[])?;
    eprintln!("\nResolve the conflicts, `git add` them, then run `gt continue` (or `gt abort` to give up).");
    Ok(())
}
