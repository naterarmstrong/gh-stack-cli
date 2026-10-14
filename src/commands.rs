use crate::ctx::Ctx;
use crate::git;
use crate::github::{MutationBatch, Pr, PrQuery};
use crate::restack;
use crate::state::{BranchRef, PrRef, Stack};
use crate::ui;
use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;
use std::io::{BufRead, Write};

// ───────────────────────────── log ─────────────────────────────

fn checks_badge(pr: &Pr) -> String {
    match pr.checks().as_deref() {
        Some("SUCCESS") => ui::green("✓ checks"),
        Some("FAILURE" | "ERROR") => ui::red("✗ checks"),
        Some("PENDING" | "EXPECTED") => ui::yellow("● checks"),
        _ => String::new(),
    }
}

fn pr_badges(pr: &Pr) -> Vec<String> {
    let mut out = vec![];
    match pr.state.as_str() {
        "MERGED" => out.push(ui::magenta("merged")),
        "CLOSED" => out.push(ui::red("closed")),
        _ => {
            if pr.is_draft {
                out.push(ui::dim("draft"));
            }
            let c = checks_badge(pr);
            if !c.is_empty() {
                out.push(c);
            }
            match pr.review_decision.as_deref() {
                Some("APPROVED") => out.push(ui::green("approved")),
                Some("CHANGES_REQUESTED") => out.push(ui::red("changes requested")),
                Some("REVIEW_REQUIRED") => out.push(ui::dim("needs review")),
                _ => {}
            }
            if pr.merge_queue_entry.is_some() {
                out.push(ui::magenta("queued"));
            } else if pr.auto_merge_request.is_some() {
                out.push(ui::cyan("auto-merge"));
            }
        }
    }
    out
}

/// Local-only status flags: is the branch restacked and pushed?
fn local_badges(ctx: &Ctx, stack: &Stack, idx: usize) -> Vec<String> {
    let b = &stack.branches[idx];
    let Some(c) = ctx.refs.heads.get(&b.branch) else { return vec![ui::red("missing locally")] };
    let mut out = vec![];
    let parent = stack.parent_of(idx);
    let parent_tip = if parent == stack.trunk.branch {
        ctx.refs.remote.get(parent).or(ctx.refs.heads.get(parent).map(|c| &c.sha))
    } else {
        ctx.refs.heads.get(parent).map(|c| &c.sha)
    };
    if let Some(p) = parent_tip {
        if c.parents.first() != Some(p) && c.sha != *p {
            // Only off the happy path: count the branch's own commits (one git call).
            let mut below: Vec<String> = [ctx.refs.remote.get(&stack.trunk.branch), ctx.refs.heads.get(&stack.trunk.branch).map(|c| &c.sha)]
                .into_iter()
                .flatten()
                .cloned()
                .collect();
            for lower in &stack.branches[..idx] {
                below.push(lower.head.clone());
                below.extend(ctx.refs.head(&lower.branch).map(str::to_string));
            }
            let own = git::own_commits(&c.sha, &below).map(|v| v.len()).unwrap_or(1);
            out.push(if own > 1 {
                ui::red(&format!("{own} commits (gt squash)"))
            } else if parent == stack.trunk.branch {
                ui::dim(&format!("behind {parent}"))
            } else {
                ui::yellow("needs restack")
            });
        }
    }
    match ctx.refs.remote.get(&b.branch) {
        None => out.push(ui::yellow("not pushed")),
        Some(r) if *r != c.sha => out.push(ui::yellow("needs push")),
        _ => {}
    }
    out
}

fn render_stack(ctx: &Ctx, stack: &Stack, prs: &HashMap<String, Pr>) {
    let cur = ctx.info.current_branch.as_deref();
    let width = stack.branches.iter().map(|b| b.branch.len()).max().unwrap_or(0).max(stack.trunk.branch.len());
    if stack.number > 0 {
        println!("{}", ui::dim(&format!("stack #{}", stack.number)));
    }
    for (idx, b) in stack.branches.iter().enumerate().rev() {
        let here = cur == Some(&b.branch);
        let marker = if b.merged() {
            ui::dim("◌")
        } else if here {
            ui::cyan("◉")
        } else {
            "◯".to_string()
        };
        let name = format!("{:width$}", b.branch);
        let name = if here {
            ui::bold(&ui::cyan(&name))
        } else if b.merged() {
            ui::dim(&name)
        } else {
            name
        };
        let pr = prs.get(&b.branch);
        let num = pr.map(|p| p.number).or(b.pr_number());
        let num = match num {
            Some(n) => {
                let url = pr.map(|p| p.url.clone()).filter(|u| !u.is_empty()).unwrap_or_else(|| {
                    b.pull_request.as_ref().map(|p| p.url.clone()).unwrap_or_default()
                });
                ui::link(&url, &format!("#{n:<6}"))
            }
            None => ui::dim(&format!("{:7}", "no PR")),
        };
        let mut badges = vec![];
        if b.merged() {
            badges.push(ui::magenta("merged"));
        } else {
            badges.extend(local_badges(ctx, stack, idx));
            if let Some(p) = pr {
                badges.extend(pr_badges(p));
            }
        }
        let subject = if b.merged() {
            String::new()
        } else {
            ctx.refs.heads.get(&b.branch).map(|c| ui::dim(&c.subject)).unwrap_or_default()
        };
        println!("{marker} {name}  {num} {}  {subject}", badges.join(&ui::dim(" · ")));
        println!("{}", ui::dim("│"));
    }
    let trunk_here = cur == Some(stack.trunk.branch.as_str());
    let trunk_marker = if trunk_here { ui::cyan("◉") } else { "◯".to_string() };
    println!("{trunk_marker} {}", ui::dim(&stack.trunk.branch));
}

fn stacks_to_show(ctx: &Ctx, all: bool) -> Vec<usize> {
    match ctx.info.current_branch.as_deref().and_then(|c| ctx.store.file.find(c)) {
        Some(si) if !all => vec![si],
        _ => (0..ctx.store.file.stacks.len()).filter(|&i| !ctx.store.file.stacks[i].fully_merged()).collect(),
    }
}

fn fetch_prs(ctx: &Ctx, sis: &[usize], include_merged: bool) -> Result<(String, HashMap<String, Pr>)> {
    let queries: Vec<_> = sis.iter().flat_map(|&si| ctx.pr_queries(si, include_merged)).collect();
    if queries.is_empty() {
        return Ok((String::new(), HashMap::new()));
    }
    let (names, qs): (Vec<_>, Vec<_>) = queries.into_iter().unzip();
    let (repo_id, results) = ctx.gh()?.prs(&qs)?;
    let mut map = HashMap::new();
    for ((name, q), pr) in names.into_iter().zip(qs.iter()).zip(results) {
        let Some(pr) = pr else { continue };
        // A head lookup can find an old PR from a reused branch name; only
        // trust a non-open one if it points at a commit we actually have.
        if matches!(q, PrQuery::Head(_)) && pr.state != "OPEN" {
            let ours = ctx.refs.head(&name) == Some(&pr.head_ref_oid)
                || ctx.refs.remote.get(&name) == Some(&pr.head_ref_oid);
            if !ours {
                continue;
            }
        }
        map.insert(name, pr);
    }
    Ok((repo_id, map))
}

pub fn log(all: bool, offline: bool) -> Result<()> {
    let ctx = Ctx::new(false)?;
    let sis = stacks_to_show(&ctx, all);
    if sis.is_empty() {
        println!("No stacks. Start one with `gt create <name> -m <message>`.");
        return Ok(());
    }
    let prs = if offline {
        HashMap::new()
    } else {
        match fetch_prs(&ctx, &sis, false) {
            Ok((_, m)) => m,
            Err(e) => {
                ui::warn(&format!("couldn't load PR status: {e:#}"));
                HashMap::new()
            }
        }
    };
    for (n, &si) in sis.iter().enumerate() {
        if n > 0 {
            println!();
        }
        render_stack(&ctx, &ctx.store.file.stacks[si], &prs);
    }
    Ok(())
}

// ───────────────────────────── navigation ─────────────────────────────

fn checkout(ctx: &Ctx, branch: &str) -> Result<()> {
    if ctx.info.current_branch.as_deref() == Some(branch) {
        ui::info(&format!("already on {}", ui::bold(branch)));
        return Ok(());
    }
    if let Some(wt) = ctx.other_worktree(branch) {
        bail!("{branch} is checked out in {}", wt.display());
    }
    let out = git::run_raw(None, &["checkout", "--quiet", branch], None, &[])?;
    if !out.ok {
        bail!("{}", out.stderr.trim());
    }
    ui::ok(&format!("checked out {}", ui::bold(branch)));
    Ok(())
}

pub enum Nav {
    Up(usize),
    Down(usize),
    Top,
    Bottom,
    Trunk,
}

pub fn nav(dir: Nav) -> Result<()> {
    let ctx = Ctx::new(false)?;
    let cur = ctx.current()?.to_string();
    let si = ctx.current_stack()?;
    let stack = &ctx.store.file.stacks[si];
    let active: Vec<usize> = stack.active().map(|(i, _)| i).collect();
    if active.is_empty() {
        bail!("every branch in this stack is merged");
    }
    // Position among active branches; a merged current branch sits just below the next active one.
    let idx = stack.index_of(&cur).unwrap();
    let pos = active.iter().position(|&i| i >= idx).unwrap_or(active.len());
    let on_active = active.get(pos) == Some(&idx);
    let target = match dir {
        Nav::Top => Some(*active.last().unwrap()),
        Nav::Bottom => Some(active[0]),
        Nav::Trunk => None,
        Nav::Up(n) => {
            let t = if on_active { pos + n } else { pos + n - 1 };
            Some(active[t.min(active.len() - 1)])
        }
        Nav::Down(n) => pos.checked_sub(n).map(|p| active[p]),
    };
    match target {
        Some(i) => checkout(&ctx, &stack.branches[i].branch),
        None => checkout(&ctx, &stack.trunk.branch.clone()),
    }
}

fn parse_pr(target: &str) -> Option<u64> {
    let t = target.trim_start_matches('#');
    t.parse().ok().or_else(|| t.split("/pull/").nth(1)?.split(['/', '#', '?']).next()?.parse().ok())
}

pub fn checkout_cmd(target: Option<String>) -> Result<()> {
    let ctx = Ctx::new(false)?;
    let branch = match target {
        Some(t) => {
            if ctx.refs.heads.contains_key(&t) {
                t
            } else if let Some((si, bi)) = parse_pr(&t).and_then(|n| ctx.store.file.find_pr(n)) {
                let b = &ctx.store.file.stacks[si].branches[bi];
                if b.merged() {
                    bail!("#{} ({}) is already merged", b.pr_number().unwrap(), b.branch);
                }
                b.branch.clone()
            } else {
                bail!("no local branch or tracked PR matches {t} (use `gt get {t}` to pull it down)");
            }
        }
        None => pick_branch(&ctx)?,
    };
    checkout(&ctx, &branch)
}

fn pick_branch(ctx: &Ctx) -> Result<String> {
    let mut choices = vec![];
    for si in stacks_to_show(ctx, true) {
        let s = &ctx.store.file.stacks[si];
        for (_, b) in s.active().collect::<Vec<_>>().into_iter().rev() {
            choices.push(b.branch.clone());
        }
    }
    if choices.is_empty() {
        bail!("no stacked branches");
    }
    for (i, c) in choices.iter().enumerate() {
        let here = ctx.info.current_branch.as_deref() == Some(c);
        let wt = ctx.other_worktree(c).map(|p| ui::dim(&format!("  ({})", p.display()))).unwrap_or_default();
        eprintln!("{:>3}  {}{wt}", i + 1, if here { ui::cyan(c) } else { c.clone() });
    }
    eprint!("checkout #: ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let n: usize = line.trim().parse().map_err(|_| anyhow!("not a number"))?;
    choices.get(n.wrapping_sub(1)).cloned().ok_or_else(|| anyhow!("out of range"))
}

// ───────────────────────────── create / modify ─────────────────────────────

fn slug(msg: &str) -> String {
    let mut s = String::new();
    for ch in msg.lines().next().unwrap_or("").chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch.to_ascii_lowercase());
        } else if !s.ends_with('_') && !s.is_empty() {
            s.push('_');
        }
        if s.len() >= 50 {
            break;
        }
    }
    s.trim_end_matches('_').to_string()
}

pub fn create(name: Option<String>, message: Option<String>, all: bool) -> Result<()> {
    let mut ctx = Ctx::new(true)?;
    let cur = ctx.current()?.to_string();
    let name = match (name, &message) {
        (Some(n), _) => n,
        (None, Some(m)) if !slug(m).is_empty() => slug(m),
        _ => bail!("give a branch name or a message (-m)"),
    };
    if ctx.refs.heads.contains_key(&name) {
        bail!("branch {name} already exists");
    }

    // Where the new branch goes: right above the current branch.
    let (si, insert_at) = match ctx.store.file.find(&cur) {
        Some(si) => (si, ctx.store.file.stacks[si].index_of(&cur).unwrap() + 1),
        None => {
            let is_trunk = cur == ctx.default_trunk() || ctx.store.file.stacks.iter().any(|s| s.trunk.branch == cur);
            let mut stack = Stack { trunk: BranchRef::new(&if is_trunk { cur.clone() } else { ctx.default_trunk() }), ..Default::default() };
            if !is_trunk {
                ui::info(&format!("tracking {} as the bottom of a new stack", ui::bold(&cur)));
                stack.branches.push(BranchRef::new(&cur));
            }
            let at = stack.branches.len();
            ctx.store.file.stacks.push(stack);
            (ctx.store.file.stacks.len() - 1, at)
        }
    };

    if all {
        git::run(&["add", "-A"])?;
    }
    git::run(&["checkout", "--quiet", "-b", &name])?;
    if git::has_staged_changes()? {
        let ok = match &message {
            Some(m) => git::run_interactive(None, &["commit", "--quiet", "-m", m])?,
            None => git::run_interactive(None, &["commit", "--quiet"])?,
        };
        if !ok {
            ui::warn("commit failed; the branch was created empty");
        }
    } else {
        ui::warn("nothing staged; created an empty branch (commit, then `gt modify`)");
    }

    ctx.store.file.stacks[si].branches.insert(insert_at, BranchRef::new(&name));
    ctx.reload()?;
    ui::ok(&format!("created {}", ui::bold(&name)));
    let clean = if insert_at + 1 < ctx.store.file.stacks[si].branches.len() {
        restack::run(&mut ctx, si, false)?
    } else {
        restack::record_heads(&mut ctx);
        true
    };
    ctx.save()?;
    exit_on_conflict(clean)
}

fn exit_on_conflict(clean: bool) -> Result<()> {
    if !clean {
        std::process::exit(1);
    }
    Ok(())
}

pub fn modify(all: bool, message: Option<String>) -> Result<()> {
    let mut ctx = Ctx::new(true)?;
    let si = ctx.current_stack()?;
    let cur = ctx.current()?.to_string();
    let stack = &ctx.store.file.stacks[si];
    let idx = stack.index_of(&cur).unwrap();
    let parent = stack.parent_of(idx).to_string();
    let head = ctx.refs.head(&cur).unwrap_or_default();
    // A branch sitting exactly on its parent has no commit yet: make one instead of amending.
    let empty = ctx.refs.head(&parent) == Some(head) || ctx.refs.remote.get(&parent).map(String::as_str) == Some(head);

    if all {
        git::run(&["add", "-A"])?;
    }
    let mut args = vec!["commit", "--quiet"];
    if !empty {
        args.push("--amend");
    }
    match &message {
        Some(m) => args.extend(["-m", m.as_str()]),
        None if !empty => args.push("--no-edit"),
        None => {}
    }
    if !empty && message.is_none() && !git::has_staged_changes()? {
        ui::info("nothing staged; just restacking");
    } else if !git::run_interactive(None, &args)? {
        bail!("git commit failed");
    }
    ctx.reload()?;
    let clean = restack::run(&mut ctx, si, false)?;
    ctx.save()?;
    exit_on_conflict(clean)
}

// ───────────────────────────── restack / continue ─────────────────────────────

pub fn restack_cmd(onto_trunk: bool) -> Result<()> {
    let mut ctx = Ctx::new(true)?;
    let si = ctx.current_stack()?;
    let plan = restack::plan(&ctx, si, onto_trunk)?;
    if plan.updates.is_empty() && plan.conflict.is_none() {
        ui::ok("stack is already restacked");
        return Ok(());
    }
    let clean = restack::run(&mut ctx, si, onto_trunk)?;
    ctx.save()?;
    exit_on_conflict(clean)
}

pub fn squash_cmd() -> Result<()> {
    let mut ctx = Ctx::new(true)?;
    let si = ctx.current_stack()?;
    let updates = restack::squash(&ctx, si)?;
    if updates.is_empty() {
        ui::ok("every branch already has one commit");
        return Ok(());
    }
    restack::apply(&mut ctx, &updates)?;
    ctx.save()
}

pub fn continue_cmd() -> Result<()> {
    let git_dir = git::repo_info(&git::open()?)?.git_dir;
    if git::rebase_in_progress(&git_dir) {
        let ok = std::process::Command::new(git::git_bin())
            .args(["rebase", "--continue"])
            .env("GIT_EDITOR", "true")
            .status()?
            .success();
        if !ok {
            bail!("rebase still has conflicts; resolve, `git add`, and run `gt continue` again");
        }
    }
    let mut ctx = Ctx::new(true)?;
    let si = ctx.current_stack()?;
    let clean = restack::run(&mut ctx, si, false)?;
    ctx.save()?;
    if clean {
        ui::ok("stack restacked");
    }
    exit_on_conflict(clean)
}

// ───────────────────────────── track ─────────────────────────────

pub fn track(branches: Vec<String>, trunk: Option<String>) -> Result<()> {
    let mut ctx = Ctx::new(true)?;
    let trunk = trunk.unwrap_or_else(|| ctx.default_trunk());
    let branches = if branches.is_empty() { vec![ctx.current()?.to_string()] } else { branches };
    for b in &branches {
        if !ctx.refs.heads.contains_key(b) {
            bail!("no local branch {b}");
        }
        if *b == trunk {
            bail!("{b} is the trunk");
        }
        if ctx.store.file.find(b).is_some() {
            bail!("{b} is already in a stack");
        }
    }
    ctx.store.file.stacks.push(Stack {
        trunk: BranchRef::new(&trunk),
        branches: branches.iter().map(|b| BranchRef::new(b)).collect(),
        ..Default::default()
    });
    ctx.reload()?;
    restack::record_heads(&mut ctx);
    ctx.save()?;
    ui::ok(&format!("tracking {} on {trunk}", branches.join(" ← ")));
    Ok(())
}

// ───────────────────────────── fetch / push helpers ─────────────────────────────

/// Fetches the given branches into remote-tracking refs in a single `git fetch`.
fn fetch(ctx: &Ctx, branches: &[String]) -> Result<()> {
    let specs: Vec<String> = branches.iter().map(|b| format!("+refs/heads/{b}:{}", ctx.remote_ref(b))).collect();
    let mut args = vec!["fetch", "--quiet", "--no-tags", ctx.remote.as_str()];
    args.extend(specs.iter().map(String::as_str));
    let out = git::run_raw(None, &args, None, &[])?;
    if !out.ok {
        bail!("git fetch failed: {}", out.stderr.trim());
    }
    Ok(())
}

/// Force-pushes branches atomically, each leased against its last known remote sha.
fn push(ctx: &Ctx, branches: &[String], no_verify: bool) -> Result<()> {
    if branches.is_empty() {
        return Ok(());
    }
    let mut args: Vec<String> = vec!["push".into(), "--quiet".into(), "--atomic".into()];
    if no_verify {
        args.push("--no-verify".into());
    }
    for b in branches {
        let expect = ctx.refs.remote.get(b).cloned().unwrap_or_default();
        args.push(format!("--force-with-lease=refs/heads/{b}:{expect}"));
    }
    args.push(ctx.remote.clone());
    args.extend(branches.iter().map(|b| format!("refs/heads/{b}:refs/heads/{b}")));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if !git::run_interactive(None, &args)? {
        bail!("git push failed");
    }
    ui::ok(&format!("pushed {}", branches.join(", ")));
    Ok(())
}

fn needs_push(ctx: &Ctx, si: usize) -> Vec<String> {
    ctx.store.file.stacks[si]
        .active()
        .filter(|(_, b)| ctx.refs.head(&b.branch).is_some() && ctx.refs.head(&b.branch) != ctx.refs.remote.get(&b.branch).map(String::as_str))
        .map(|(_, b)| b.branch.clone())
        .collect()
}

/// Takes a remote branch that someone else pushed to (a bot fix, a teammate)
/// and folds it back into a single commit on the branch.
fn adopt_remote(ctx: &Ctx, branch: &str, parent_ref: &str) -> Result<()> {
    let remote = ctx.refs.remote[branch].clone();
    let local = ctx.refs.heads.get(branch).cloned();
    let base = match &local {
        Some(l) if l.parents.len() == 1 && git::is_ancestor(&l.sha, &remote) => l.parents[0].clone(),
        _ => git::merge_base(&remote, parent_ref).ok_or_else(|| anyhow!("{branch}: no merge base with {parent_ref}"))?,
    };
    let remote_parent = git::rev_parse(&format!("{remote}^"));
    let new = if remote_parent.as_deref() == Some(base.as_str()) {
        remote.clone()
    } else {
        let meta_src = local.as_ref().map(|l| l.sha.clone()).unwrap_or_else(|| remote.clone());
        git::commit_tree(&git::tree_of(&remote)?, &base, &git::commit_meta(&meta_src)?)?
    };
    ctx.set_branch(branch, local.as_ref().map(|l| l.sha.as_str()), &new)?;
    ui::ok(&format!("pulled remote changes into {}", ui::bold(branch)));
    Ok(())
}

// ───────────────────────────── sync ─────────────────────────────

pub struct SyncOpts {
    pub all: bool,
    pub no_push: bool,
    pub no_prune: bool,
    pub prefer_remote: bool,
    pub offline: bool,
    pub no_verify: bool,
}

pub fn sync(opts: SyncOpts) -> Result<()> {
    let mut ctx = Ctx::new(true)?;
    let sis = stacks_to_show(&ctx, opts.all);
    if sis.is_empty() {
        bail!("nothing to sync");
    }

    // 1. One GraphQL round trip for every PR: learn numbers and what merged.
    let prs = if opts.offline { HashMap::new() } else { fetch_prs(&ctx, &sis, true)?.1 };
    for &si in &sis {
        for b in ctx.store.file.stacks[si].branches.iter_mut() {
            let Some(pr) = prs.get(&b.branch) else { continue };
            let r = b.pull_request.get_or_insert_with(PrRef::default);
            r.number = pr.number;
            r.id = pr.id.clone();
            r.url = pr.url.clone();
            if pr.state == "MERGED" && !r.merged {
                r.merged = true;
                ui::ok(&format!("{} merged (#{})", ui::bold(&b.branch), pr.number));
            } else if pr.state == "CLOSED" {
                ui::warn(&format!("{}'s PR #{} was closed without merging", b.branch, pr.number));
            }
        }
    }

    // 2. One fetch for the trunks and every branch whose head still exists on the remote.
    let old_remote = ctx.refs.remote.clone();
    let mut to_fetch: Vec<String> = vec![];
    for &si in &sis {
        let s = &ctx.store.file.stacks[si];
        to_fetch.push(s.trunk.branch.clone());
        for (_, b) in s.active() {
            let on_remote = match prs.get(&b.branch) {
                Some(pr) => pr.state == "OPEN",
                None => old_remote.contains_key(&b.branch),
            };
            if on_remote {
                to_fetch.push(b.branch.clone());
            }
        }
    }
    to_fetch.sort();
    to_fetch.dedup();
    if let Err(e) = fetch(&ctx, &to_fetch) {
        ui::warn(&format!("{e:#}; retrying with trunks only"));
        let trunks: Vec<String> = sis.iter().map(|&si| ctx.store.file.stacks[si].trunk.branch.clone()).collect();
        fetch(&ctx, &trunks)?;
    }
    ctx.reload()?;

    // 3. Pull in commits others pushed to our branches.
    for &si in &sis {
        let s = ctx.store.file.stacks[si].clone();
        for (idx, b) in s.active() {
            let Some(new_r) = ctx.refs.remote.get(&b.branch).cloned() else { continue };
            let local = ctx.refs.head(&b.branch).map(str::to_string);
            let old_r = old_remote.get(&b.branch);
            if local.as_deref() == Some(&new_r) || old_r == Some(&new_r) {
                continue;
            }
            if let Some(l) = &local {
                if git::is_ancestor(&new_r, l) {
                    continue;
                }
            }
            let parent = s.parent_of(idx);
            let parent_ref =
                if parent == s.trunk.branch { ctx.remote_ref(parent) } else { format!("refs/heads/{parent}") };
            let local_unchanged = local.is_none() || local.as_ref() == old_r || git::is_ancestor(local.as_deref().unwrap(), &new_r);
            if local_unchanged || opts.prefer_remote {
                adopt_remote(&ctx, &b.branch, &parent_ref)?;
            } else {
                ui::warn(&format!(
                    "{} changed both locally and on {} — keeping local (use --prefer-remote to take theirs)",
                    b.branch, ctx.remote
                ));
            }
        }
    }
    ctx.reload()?;

    // 4. Fast-forward local trunks nobody has checked out.
    for &si in &sis {
        let trunk = ctx.store.file.stacks[si].trunk.branch.clone();
        if let (Some(l), Some(r)) = (ctx.refs.head(&trunk), ctx.refs.remote.get(&trunk)) {
            if l != r && !ctx.worktrees.contains_key(&trunk) && git::is_ancestor(l, r) {
                ctx.set_branch(&trunk, Some(l), r)?;
            }
        }
    }

    // 5. Restack every stack onto its updated trunk.
    for &si in &sis {
        if !restack::run(&mut ctx, si, true)? {
            ctx.save()?;
            std::process::exit(1);
        }
    }

    // 6. Push branches that already have a remote copy.
    if !opts.no_push {
        for &si in &sis {
            let branches: Vec<String> =
                needs_push(&ctx, si).into_iter().filter(|b| ctx.refs.remote.contains_key(b)).collect();
            push(&ctx, &branches, opts.no_verify)?;
        }
        ctx.reload()?;
    }

    // 7. Delete local branches whose PRs merged, unless they hold unpushed work.
    if !opts.no_prune {
        prune(&mut ctx, &sis, &prs)?;
    }

    restack::record_heads(&mut ctx);
    ctx.save()?;
    for (n, &si) in sis.iter().enumerate() {
        if n > 0 {
            println!();
        }
        let s = &ctx.store.file.stacks[si];
        if s.fully_merged() {
            ui::ok(&format!("stack on {} is fully merged", s.trunk.branch));
        } else {
            render_stack(&ctx, s, &prs);
        }
    }
    Ok(())
}

fn prune(ctx: &mut Ctx, sis: &[usize], prs: &HashMap<String, Pr>) -> Result<()> {
    for &si in sis {
        let s = ctx.store.file.stacks[si].clone();
        for (idx, b) in s.branches.iter().enumerate().filter(|(_, b)| b.merged()) {
            let Some(local) = ctx.refs.head(&b.branch).map(str::to_string) else { continue };
            let safe = match prs.get(&b.branch) {
                Some(pr) => pr.head_ref_oid == local,
                None => ctx.refs.remote.get(&b.branch) == Some(&local),
            };
            if !safe {
                ui::warn(&format!("{} merged but has local commits that weren't in the PR; keeping it", b.branch));
                continue;
            }
            if let Some(wt) = ctx.other_worktree(&b.branch) {
                ui::info(&format!("{} merged; not deleting it, it's checked out in {}", b.branch, wt.display()));
                continue;
            }
            if ctx.info.current_branch.as_deref() == Some(&b.branch) {
                let next = s.branches[idx + 1..]
                    .iter()
                    .find(|x| !x.merged() && !ctx.worktrees.contains_key(&x.branch))
                    .map(|x| x.branch.clone());
                let Some(next) = next else {
                    ui::info(&format!("{} merged; staying on it (nowhere to move)", b.branch));
                    continue;
                };
                if !git::run_raw(None, &["checkout", "--quiet", &next], None, &[])?.ok {
                    ui::info(&format!("{} merged; couldn't switch away to delete it", b.branch));
                    continue;
                }
                ui::info(&format!("moved to {}", ui::bold(&next)));
                ctx.reload()?;
            }
            git::run(&["branch", "--quiet", "-D", &b.branch])?;
            ui::ok(&format!("deleted merged branch {}", b.branch));
        }
    }
    ctx.reload()
}

// ───────────────────────────── submit ─────────────────────────────

pub fn submit(draft: bool, no_verify: bool, relink: bool) -> Result<()> {
    let mut ctx = Ctx::new(true)?;
    let si = ctx.current_stack()?;
    if !restack::run(&mut ctx, si, false)? {
        ctx.save()?;
        std::process::exit(1);
    }

    let (repo_id, prs) = fetch_prs(&ctx, &[si], false)?;
    push(&ctx, &needs_push(&ctx, si), no_verify)?;
    ctx.reload()?;

    let stack = ctx.store.file.stacks[si].clone();
    let mut batch = MutationBatch::default();
    let mut created = vec![];
    for (idx, b) in stack.active() {
        let parent = stack.parent_of(idx);
        match prs.get(&b.branch).filter(|p| p.state == "OPEN") {
            Some(pr) if pr.base_ref_name != parent => {
                batch.set_base(&format!("b{idx}"), &pr.id, parent);
                ui::info(&format!("retargeting #{} onto {parent}", pr.number));
            }
            Some(_) => {}
            None => {
                let meta = git::commit_meta(ctx.refs.head(&b.branch).unwrap())?;
                let (title, body) = meta.message.split_once('\n').unwrap_or((&meta.message, ""));
                batch.create_pr(&format!("c{idx}"), &repo_id, parent, &b.branch, title.trim(), body.trim(), draft);
                created.push(idx);
            }
        }
    }
    let mut result = serde_json::Value::Null;
    if !batch.is_empty() {
        result = batch.run(ctx.gh()?)?;
    }

    // Record PR refs: existing ones from the query, new ones from the mutation.
    let stack = &mut ctx.store.file.stacks[si];
    for (idx, b) in stack.branches.iter_mut().enumerate() {
        if let Some(pr) = prs.get(&b.branch).filter(|p| p.state == "OPEN") {
            b.pull_request = Some(PrRef { number: pr.number, id: pr.id.clone(), url: pr.url.clone(), ..Default::default() });
        } else if created.contains(&idx) {
            let p = &result[format!("c{idx}")]["pullRequest"];
            if let Some(n) = p["number"].as_u64() {
                b.pull_request = Some(PrRef {
                    number: n,
                    id: p["id"].as_str().unwrap_or_default().into(),
                    url: p["url"].as_str().unwrap_or_default().into(),
                    ..Default::default()
                });
                ui::ok(&format!("created #{n} for {}", ui::bold(&b.branch)));
            }
        }
    }

    if let Err(e) = link_stack(&mut ctx, si, relink) {
        ui::warn(&format!("couldn't link the stack on GitHub: {e:#}"));
    }
    restack::record_heads(&mut ctx);
    ctx.save()?;

    let gh = ctx.gh()?;
    for (_, b) in ctx.store.file.stacks[si].active().collect::<Vec<_>>().into_iter().rev() {
        if let Some(n) = b.pr_number() {
            println!("{:<40} {}", b.branch, gh.pr_url(n));
        }
    }
    Ok(())
}

/// Makes GitHub's stack object match the local order of open PRs.
fn link_stack(ctx: &mut Ctx, si: usize, relink: bool) -> Result<()> {
    let stack = &ctx.store.file.stacks[si];
    let open: Vec<u64> = stack.active().filter_map(|(_, b)| b.pr_number()).collect();
    if open.len() < 2 {
        return Ok(());
    }
    let gh = ctx.gh()?;
    let remote = match stack.number {
        0 => None,
        n => gh.get_stack(n)?,
    };
    let remote = match remote {
        Some(r) => Some(r),
        None => gh.stack_for_pr(open[0])?,
    };
    let linked = match remote {
        None => gh.create_stack(&open)?,
        Some(r) => {
            let r_open: Vec<u64> =
                r.pull_requests.iter().filter(|p| p.merged_at.is_none()).map(|p| p.number).collect();
            if r_open == open {
                r
            } else if open.starts_with(&r_open) {
                gh.add_to_stack(r.number, &open[r_open.len()..])?
            } else if relink {
                gh.unstack(r.number)?;
                gh.create_stack(&open)?
            } else {
                bail!(
                    "GitHub stack #{} is {:?} but local is {:?}; re-run with --relink to replace it",
                    r.number,
                    r_open,
                    open
                );
            }
        }
    };
    let stack = &mut ctx.store.file.stacks[si];
    if stack.number != linked.number {
        ui::ok(&format!("linked stack #{}", linked.number));
    }
    stack.number = linked.number;
    stack.id = linked.id.to_string();
    Ok(())
}

// ───────────────────────────── get ─────────────────────────────

pub fn get(target: String) -> Result<()> {
    let mut ctx = Ctx::new(true)?;
    let number = match parse_pr(&target) {
        Some(n) => n,
        None => {
            let (_, r) = ctx.gh()?.prs(&[PrQuery::Head(target.clone())])?;
            r.into_iter().flatten().next().ok_or_else(|| anyhow!("no PR for branch {target}"))?.number
        }
    };
    if let Some((si, bi)) = ctx.store.file.find_pr(number) {
        let b = ctx.store.file.stacks[si].branches[bi].branch.clone();
        ui::info(&format!("#{number} is already tracked; checking out {b} (run `gt sync` to update)"));
        return checkout(&ctx, &b);
    }

    let gh = ctx.gh()?;
    // (number, head, merged)
    let (trunk, entries, stack_num, stack_id): (String, Vec<(u64, String, bool)>, u64, String) =
        match gh.stack_for_pr(number)? {
            Some(r) => (
                r.base.ref_name.clone(),
                r.pull_requests.iter().map(|p| (p.number, p.head.ref_name.clone(), p.merged_at.is_some())).collect(),
                r.number,
                r.id.to_string(),
            ),
            None => {
                let (_, r) = gh.prs(&[PrQuery::Number(number)])?;
                let pr = r.into_iter().flatten().next().ok_or_else(|| anyhow!("PR #{number} not found"))?;
                (pr.base_ref_name, vec![(number, pr.head_ref_name, pr.state == "MERGED")], 0, String::new())
            }
        };
    for (_, head, _) in &entries {
        if ctx.store.file.find(head).is_some() {
            bail!("{head} is already in another local stack");
        }
    }

    let mut to_fetch: Vec<String> = entries.iter().filter(|e| !e.2).map(|e| e.1.clone()).collect();
    to_fetch.push(trunk.clone());
    fetch(&ctx, &to_fetch)?;
    ctx.reload()?;

    let mut branches = vec![];
    for (n, head, merged) in &entries {
        if !merged {
            let remote = ctx.refs.remote.get(head).cloned().ok_or_else(|| anyhow!("{head} missing after fetch"))?;
            match ctx.refs.head(head).map(str::to_string) {
                None => ctx.set_branch(head, None, &remote)?,
                Some(l) if l == remote => {}
                Some(l) if git::is_ancestor(&l, &remote) => ctx.set_branch(head, Some(&l), &remote)?,
                Some(_) => ui::warn(&format!("{head} differs from {}; keeping local (sync to reconcile)", ctx.remote)),
            }
        }
        branches.push(BranchRef {
            branch: head.clone(),
            pull_request: Some(PrRef { number: *n, url: ctx.gh()?.pr_url(*n), merged: *merged, ..Default::default() }),
            ..Default::default()
        });
    }
    ctx.store.file.stacks.push(Stack {
        id: stack_id,
        number: stack_num,
        trunk: BranchRef::new(&trunk),
        branches,
        ..Default::default()
    });
    ctx.reload()?;
    restack::record_heads(&mut ctx);
    ctx.save()?;
    let target_branch = entries.iter().find(|e| e.0 == number).map(|e| e.1.clone()).unwrap();
    ui::ok(&format!("tracked {} branch(es) on {trunk}", entries.len()));
    checkout(&ctx, &target_branch)
}

// ───────────────────────────── open ─────────────────────────────

pub fn open(target: Option<String>) -> Result<()> {
    let ctx = Ctx::new(false)?;
    let branch = match target {
        Some(t) if parse_pr(&t).is_some() => {
            let url = ctx.gh()?.pr_url(parse_pr(&t).unwrap());
            return open_url(&url);
        }
        Some(t) => t,
        None => ctx.current()?.to_string(),
    };
    let tracked = ctx.store.file.find(&branch).and_then(|si| {
        let s = &ctx.store.file.stacks[si];
        s.branches[s.index_of(&branch)?].pull_request.clone()
    });
    let url = match tracked {
        Some(p) if !p.url.is_empty() => p.url,
        Some(p) => ctx.gh()?.pr_url(p.number),
        None => {
            let (_, r) = ctx.gh()?.prs(&[PrQuery::Head(branch.clone())])?;
            r.into_iter().flatten().next().ok_or_else(|| anyhow!("no PR for {branch}"))?.url
        }
    };
    open_url(&url)
}

fn open_url(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener).arg(url).status()?;
    println!("{url}");
    Ok(())
}
