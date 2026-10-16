mod commands;
mod ctx;
mod git;
mod github;
mod history;
mod loc;
mod picker;
mod restack;
mod state;
mod ui;

use clap::{Parser, Subcommand};
use commands::Nav;

/// Fast, Graphite-style stacked PRs on top of gh-stack's state and GitHub's stacks API.
/// Assumes one commit per branch.
#[derive(Parser)]
#[command(name = "gt", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show every local stack and review branch with PR, CI and review status
    #[command(visible_alias = "l")]
    Log {
        /// Only the stack containing the current branch
        #[arg(short, long)]
        stack: bool,
    },
    /// Show every local stack and review branch, compactly, from local state only (no network)
    Ls {
        #[arg(short, long)]
        stack: bool,
    },
    /// Check out the branch above
    #[command(visible_alias = "u")]
    Up { steps: Option<usize> },
    /// Check out the branch below
    #[command(visible_alias = "d")]
    Down { steps: Option<usize> },
    /// Check out the top of the stack
    #[command(visible_alias = "t")]
    Top,
    /// Check out the bottom of the stack
    #[command(visible_alias = "b")]
    Bottom,
    /// Check out the stack's trunk
    Trunk,
    /// Check out a branch by name or PR number (interactive picker if omitted)
    #[command(visible_alias = "co")]
    Checkout { target: Option<String> },
    /// Create a branch above the current one and commit staged changes to it
    #[command(visible_alias = "c")]
    Create {
        /// Branch name (derived from the message if omitted)
        name: Option<String>,
        #[arg(short, long)]
        message: Option<String>,
        /// Stage all changes first
        #[arg(short, long)]
        all: bool,
    },
    /// Amend the current branch's commit and restack everything above it
    #[command(visible_alias = "m")]
    Modify {
        /// Stage all changes first
        #[arg(short, long)]
        all: bool,
        /// Replace the commit message
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Rebase each branch onto its parent (in memory, no checkouts)
    #[command(visible_alias = "r")]
    Restack {
        /// Also move the bottom branch onto the latest remote trunk
        #[arg(long)]
        trunk: bool,
    },
    /// Fold each branch in the stack down to one commit (content unchanged)
    Squash,
    /// Continue after resolving a restack conflict; returns you to where you started once done
    Continue,
    /// Abort a conflicted restack: undo the `git rebase`, mark the branch as needing a
    /// rebase, and return to the branch you were on
    Abort,
    /// Fetch, mark merged PRs, pull in remote fixes, restack onto trunk, push.
    /// Branches that conflict are left in place and marked as needing a rebase.
    /// Also moves review branches to their PR heads, overwriting local commits.
    #[command(visible_alias = "s")]
    Sync {
        #[arg(short, long)]
        all: bool,
        #[arg(long)]
        no_push: bool,
        /// Keep local branches of merged PRs
        #[arg(long)]
        no_prune: bool,
        /// When a branch changed both locally and remotely, take the remote
        #[arg(long)]
        prefer_remote: bool,
        /// Skip GitHub API calls (git fetch/push only)
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        no_verify: bool,
    },
    /// Push the stack, create/retarget PRs, and link them as a GitHub stack
    #[command(visible_alias = "ss")]
    Submit {
        /// Create new PRs as drafts
        #[arg(long)]
        draft: bool,
        #[arg(long)]
        no_verify: bool,
        /// Replace the GitHub stack if it doesn't match the local order
        #[arg(long)]
        relink: bool,
    },
    /// Pull down a stack from GitHub by PR number, URL, or branch
    Get { target: String },
    /// Start tracking existing branches (bottom first) as a stack
    Track {
        branches: Vec<String>,
        #[arg(long)]
        trunk: Option<String>,
    },
    /// Delete a branch and everything above it in its stack, locally only (picker if omitted)
    #[command(visible_alias = "dl")]
    Delete {
        /// Lowest branch to delete (name or PR number)
        target: Option<String>,
        /// Don't ask for confirmation
        #[arg(short, long)]
        force: bool,
    },
    /// Delete every local branch whose PR merged (only if the branch is exactly what merged)
    #[command(name = "clear-merged", visible_alias = "cm")]
    ClearMerged {
        /// Don't ask for confirmation
        #[arg(short, long)]
        force: bool,
        /// Also delete branches whose local commit differs from what merged
        #[arg(short = 'D', long)]
        include_diverged: bool,
    },
    /// Check out a PR to review, read-only: `gt sync` moves it to the PR head (even after a
    /// force push, dropping local commits) but never restacks or pushes it.
    /// Without a target, pick from PRs where you're a requested reviewer (not via a team).
    #[command(visible_alias = "rv")]
    Review {
        /// Branch name, PR number, or PR URL
        target: Option<String>,
    },
    /// Lines changed by a branch's commit, per file, split into src/test/doc/gen
    Loc {
        /// Branch, PR number, or any commit (defaults to the current branch)
        target: Option<String>,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// A branch's commit, version by version, from its reflog: list the versions,
    /// diff them, or put the branch back on an earlier one
    #[command(visible_alias = "h", args_conflicts_with_subcommands = true)]
    History {
        #[command(subcommand)]
        action: Option<HistoryCmd>,
        /// Branch or PR number (defaults to the current branch)
        branch: Option<String>,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Open a branch's PR in the browser
    #[command(visible_alias = "pr")]
    Open { target: Option<String> },
}

/// Versions are named `p3` or `3`; any commit works too.
#[derive(Subcommand)]
enum HistoryCmd {
    /// What a version changed over its own base (the PR as of that version)
    Show {
        version: String,
        /// Branch or PR number (defaults to the current branch)
        #[arg(short, long)]
        branch: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// What changed from one version to another, even across restacks.
    /// Defaults to the previous version against the current one; `base` is the other side's parent
    Compare {
        from: Option<String>,
        to: Option<String>,
        #[arg(short, long)]
        branch: Option<String>,
        /// Endpoints, trees and changed files instead of a diff
        #[arg(long)]
        json: bool,
    },
    /// Put the branch, or only the paths after `--`, back on an earlier version,
    /// then restack. The revert is itself a version, so it can be undone the same way
    Revert {
        version: String,
        #[arg(short, long)]
        branch: Option<String>,
        /// Take the version exactly as it was (its old base, or its files verbatim)
        /// instead of re-applying it onto the branch's current base
        #[arg(long)]
        exact: bool,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
        #[arg(last = true)]
        paths: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let res = match cli.cmd {
        Cmd::Log { stack } => commands::log(stack, false),
        Cmd::Ls { stack } => commands::log(stack, true),
        Cmd::Up { steps } => commands::nav(Nav::Up(steps.unwrap_or(1))),
        Cmd::Down { steps } => commands::nav(Nav::Down(steps.unwrap_or(1))),
        Cmd::Top => commands::nav(Nav::Top),
        Cmd::Bottom => commands::nav(Nav::Bottom),
        Cmd::Trunk => commands::nav(Nav::Trunk),
        Cmd::Checkout { target } => commands::checkout_cmd(target),
        Cmd::Create { name, message, all } => commands::create(name, message, all),
        Cmd::Modify { all, message } => commands::modify(all, message),
        Cmd::Restack { trunk } => commands::restack_cmd(trunk),
        Cmd::Squash => commands::squash_cmd(),
        Cmd::Continue => commands::continue_cmd(),
        Cmd::Abort => commands::abort_cmd(),
        Cmd::Sync { all, no_push, no_prune, prefer_remote, offline, no_verify } => {
            commands::sync(commands::SyncOpts { all, no_push, no_prune, prefer_remote, offline, no_verify })
        }
        Cmd::Submit { draft, no_verify, relink } => commands::submit(draft, no_verify, relink),
        Cmd::Get { target } => commands::get(target),
        Cmd::Track { branches, trunk } => commands::track(branches, trunk),
        Cmd::Delete { target, force } => commands::delete_upstack(target, force),
        Cmd::ClearMerged { force, include_diverged } => commands::clear_merged(force, include_diverged),
        Cmd::Review { target } => commands::review(target),
        Cmd::Loc { target, json } => commands::loc_cmd(target, json),
        Cmd::Open { target } => commands::open(target),
        Cmd::History { action, branch, json } => match action {
            None => commands::history_list(branch, json),
            Some(HistoryCmd::Show { version, branch, json }) => {
                commands::history_compare(Some("base".into()), Some(version), branch, json)
            }
            Some(HistoryCmd::Compare { from, to, branch, json }) => commands::history_compare(from, to, branch, json),
            Some(HistoryCmd::Revert { version, branch, exact, yes, paths }) => {
                commands::history_revert(version, branch, exact, yes, paths)
            }
        },
    };
    if let Err(e) = res {
        eprintln!("{} {e:#}", ui::red("error:"));
        std::process::exit(1);
    }
}
