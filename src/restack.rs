//! In-memory restacking. With one commit per branch, a branch's old base is
//! simply its tip's parent, so each branch is replayed with `git merge-tree`
//! + `git commit-tree` without touching any working tree.

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
    pub conflict: Option<Conflict>,
}

/// Computes new commits for every unmerged branch in stack `si`. When
/// `onto_trunk` is set the bottom branch moves onto the remote trunk tip;
/// otherwise it stays where it is and only the branches above it move.
pub fn plan(ctx: &Ctx, si: usize, onto_trunk: bool) -> Result<Plan> {
    let stack = &ctx.store.file.stacks[si];
    let mut parent: Option<(String, String)> =
        if onto_trunk { Some((stack.trunk.branch.clone(), ctx.trunk_tip(&stack.trunk.branch)?)) } else { None };
    let mut old_parent_head: Option<String> = None;
    let mut updates = vec![];
    let mut below = below_seed(ctx, stack);

    for b in stack.branches.iter() {
        if b.merged() {
            below.push(b.head.clone());
            continue;
        }
        let Some(c) = ctx.refs.heads.get(&b.branch) else {
            bail!("branch {} is in the stack but missing locally (try `gt get` or `gt sync`)", b.branch);
        };
        let Some((parent_name, parent_tip)) = parent.clone() else {
            below.extend([b.head.clone(), c.sha.clone()]);
            parent = Some((b.branch.clone(), c.sha.clone()));
            old_parent_head = Some(c.sha.clone());
            continue;
        };

        let new = if old_parent_head.as_deref() == Some(&c.sha) || c.sha == parent_tip {
            // Branch has no commit of its own yet; it just follows its parent.
            parent_tip.clone()
        } else {
            if c.parents.len() != 1 {
                bail!("{} tip is a merge or root commit; gt expects exactly one commit per branch", b.branch);
            }
            let base = &c.parents[0];
            if *base == parent_tip {
                c.sha.clone()
            } else {
                // Replaying only the tip of a multi-commit branch would drop the rest.
                let own = git::own_commits(&c.sha, &below)?;
                if own.len() > 1 {
                    bail!(
                        "{} has {} commits of its own; gt needs one commit per branch. Run `gt squash` to fold them.",
                        b.branch,
                        own.len()
                    );
                }
                match git::merge_tree(base, &parent_tip, &c.sha)? {
                    Merge::Clean(tree) => git::commit_tree(&tree, &parent_tip, &git::commit_meta(&c.sha)?)?,
                    Merge::Conflict(files) => {
                        return Ok(Plan {
                            updates,
                            conflict: Some(Conflict {
                                branch: b.branch.clone(),
                                onto: parent_tip,
                                onto_name: parent_name,
                                base: base.clone(),
                                files,
                            }),
                        });
                    }
                }
            }
        };
        if new != c.sha {
            updates.push(Update { branch: b.branch.clone(), old: c.sha.clone(), new: new.clone() });
        }
        below.extend([b.head.clone(), c.sha.clone(), new.clone()]);
        old_parent_head = Some(c.sha.clone());
        parent = Some((b.branch.clone(), new));
    }
    Ok(Plan { updates, conflict: None })
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
        ctx.set_branch(&u.branch, Some(&u.old), &u.new)?;
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
        for b in stack.branches.iter_mut().filter(|b| !b.merged()) {
            if let Some(c) = refs.heads.get(&b.branch) {
                b.head = c.sha.clone();
                if let Some(p) = c.parents.first() {
                    b.base = p.clone();
                }
            }
        }
    }
}

/// Runs a full restack. On conflict, applies everything below it and leaves
/// the conflicting branch mid-rebase so the user can resolve it.
pub fn run(ctx: &mut Ctx, si: usize, onto_trunk: bool) -> Result<bool> {
    let plan = plan(ctx, si, onto_trunk)?;
    apply(ctx, &plan.updates)?;
    match plan.conflict {
        None => Ok(true),
        Some(c) => {
            start_conflict(ctx, &c)?;
            Ok(false)
        }
    }
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
    if ctx.info.current_branch.as_deref() != Some(&c.branch) {
        git::run(&["checkout", "--quiet", &c.branch])?;
    }
    git::run_raw(None, &["rebase", "--onto", &c.onto, &c.base], None, &[])?;
    eprintln!("\nResolve the conflicts, `git add` them, then run `gt continue`.");
    Ok(())
}
