use crate::git::{self, Refs, RepoInfo};
use crate::github::{Client, PrQuery};
use crate::state::{Lock, Store};
use anyhow::{Result, anyhow, bail};
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Branches whose commits need decoding: every stacked branch and trunk.
fn tracked(store: &Store) -> HashSet<String> {
    store
        .file
        .stacks
        .iter()
        .flat_map(|s| std::iter::once(&s.trunk).chain(&s.branches))
        .map(|b| b.branch.clone())
        .collect()
}

pub struct Ctx {
    pub info: RepoInfo,
    pub store: Store,
    pub remote: String,
    pub refs: Refs,
    pub worktrees: HashMap<String, PathBuf>,
    lock: Option<Lock>,
    gh: OnceCell<Client>,
}

impl Ctx {
    /// Mutating commands take gh-stack's locks before reading state so the
    /// snapshot they modify can't be changed underneath them.
    pub fn new(mutating: bool) -> Result<Ctx> {
        let repo = git::open()?;
        let info = git::repo_info(&repo)?;
        let lock = if mutating { Some(Lock::acquire(&info.common_dir)?) } else { None };
        let store = Store::load(&info.common_dir)?;
        let remote = std::env::var("GT_REMOTE").unwrap_or_else(|_| "origin".into());
        let refs = Refs::load(&repo, &remote, &tracked(&store))?;
        let worktrees = git::worktrees(&info.common_dir);
        Ok(Ctx { info, store, remote, refs, worktrees, lock, gh: OnceCell::new() })
    }

    pub fn gh(&self) -> Result<&Client> {
        if let Some(c) = self.gh.get() {
            return Ok(c);
        }
        let client = Client::new(&git::remote_url(&self.remote)?)?;
        Ok(self.gh.get_or_init(|| client))
    }

    pub fn reload(&mut self) -> Result<()> {
        // A fresh handle so ref and object caches see what git just wrote.
        let repo = git::open()?;
        self.info = git::repo_info(&repo)?;
        self.refs = Refs::load(&repo, &self.remote, &tracked(&self.store))?;
        self.worktrees = git::worktrees(&self.info.common_dir);
        Ok(())
    }

    pub fn save(&mut self) -> Result<()> {
        let lock = self.lock.as_ref().ok_or_else(|| anyhow!("internal: save without lock"))?;
        self.store.save(lock)
    }

    pub fn current(&self) -> Result<&str> {
        self.info.current_branch.as_deref().ok_or_else(|| anyhow!("HEAD is detached"))
    }

    /// Index of the stack containing the current branch.
    pub fn current_stack(&self) -> Result<usize> {
        let cur = self.current()?;
        self.store
            .file
            .find(cur)
            .ok_or_else(|| anyhow!("{cur} is not part of a stack (see `gt ls --all`, `gt track`)"))
    }

    pub fn remote_ref(&self, branch: &str) -> String {
        format!("refs/remotes/{}/{branch}", self.remote)
    }

    /// The commit a stack's bottom branch should be rebased onto: the remote trunk when known.
    pub fn trunk_tip(&self, trunk: &str) -> Result<String> {
        self.refs
            .remote
            .get(trunk)
            .or_else(|| self.refs.heads.get(trunk).map(|c| &c.sha))
            .cloned()
            .ok_or_else(|| anyhow!("trunk branch {trunk} not found locally or on {}", self.remote))
    }

    /// Worktree (other than this one) where `branch` is checked out.
    pub fn other_worktree(&self, branch: &str) -> Option<&PathBuf> {
        self.worktrees.get(branch).filter(|p| **p != self.info.toplevel)
    }

    /// Move a branch to `new`, keeping any worktree that has it checked out in step.
    pub fn set_branch(&self, branch: &str, old: Option<&str>, new: &str) -> Result<()> {
        if let Some(wt) = self.worktrees.get(branch) {
            let out = git::run_raw(Some(wt), &["reset", "--keep", "--quiet", new], None, &[])?;
            if !out.ok {
                bail!(
                    "couldn't move {branch} in worktree {} (local changes in the way?): {}",
                    wt.display(),
                    out.stderr.trim()
                );
            }
            if *wt != self.info.toplevel {
                crate::ui::info(&format!("updated checked-out {branch} in {}", wt.display()));
            }
            return Ok(());
        }
        let refname = format!("refs/heads/{branch}");
        let mut args = vec!["update-ref", "-m", "gt", refname.as_str(), new];
        if let Some(o) = old {
            args.push(o);
        }
        git::run(&args)?;
        Ok(())
    }

    /// Which PR query to use for each branch in a stack (by number when known, else by head).
    pub fn pr_queries(&self, si: usize, include_merged: bool) -> Vec<(String, PrQuery)> {
        self.store.file.stacks[si]
            .branches
            .iter()
            .filter(|b| include_merged || !b.merged())
            .map(|b| {
                let q = match b.pr_number() {
                    Some(n) => PrQuery::Number(n),
                    None => PrQuery::Head(b.branch.clone()),
                };
                (b.branch.clone(), q)
            })
            .collect()
    }

    pub fn default_trunk(&self) -> String {
        git::run(&["symbolic-ref", "--short", &format!("refs/remotes/{}/HEAD", self.remote)])
            .ok()
            .and_then(|s| s.split_once('/').map(|(_, b)| b.to_string()))
            .unwrap_or_else(|| "main".into())
    }
}
