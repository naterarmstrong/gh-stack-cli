use crate::git::{self, Refs, RepoInfo};
use crate::github::{Client, PrQuery};
use crate::state::{Lock, Store};
use anyhow::{Result, anyhow, bail};
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Branches whose commits need decoding: every stacked branch, trunk and review branch.
fn tracked(store: &Store, reviews: &[Review]) -> HashSet<String> {
    store
        .file
        .stacks
        .iter()
        .flat_map(|s| std::iter::once(&s.trunk).chain(&s.branches))
        .map(|b| b.branch.clone())
        .chain(reviews.iter().map(|r| r.branch.clone()))
        .collect()
}

/// A branch pulled down by `gt review`, marked by `branch.<b>.gtReview`.
#[derive(Clone, Debug)]
pub struct Review {
    pub branch: String,
    pub pr: Option<u64>,
}

impl Review {
    /// Fork PRs are checked out as `pr/<N>` and have no branch on our remote.
    pub fn fork_pr(&self) -> Option<u64> {
        self.pr.filter(|n| self.branch == format!("pr/{n}"))
    }
}

fn reviews(repo: &gix::Repository) -> Vec<Review> {
    let cfg = repo.config_snapshot();
    let mut v: Vec<Review> = cfg
        .plumbing()
        .sections_by_name("branch")
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let branch = s.header().subsection_name()?.to_string();
            let marker = s.value("gtReview")?.to_string();
            Some(Review { branch, pr: marker.parse().ok() })
        })
        .collect();
    v.sort_by(|a, b| a.branch.cmp(&b.branch));
    v.dedup_by(|a, b| a.branch == b.branch);
    v
}

/// Each tracked branch's configured upstream: (branch.<b>.remote, branch.<b>.merge).
fn upstreams(repo: &gix::Repository, branches: &HashSet<String>) -> HashMap<String, (String, String)> {
    let cfg = repo.config_snapshot();
    let get =
        |b: &str, k: &str| cfg.string(format!("branch.{b}.{k}").as_str()).map(|v| v.to_string()).unwrap_or_default();
    branches.iter().map(|b| (b.clone(), (get(b, "remote"), get(b, "merge")))).collect()
}

pub struct Ctx {
    pub info: RepoInfo,
    pub store: Store,
    pub remote: String,
    pub refs: Refs,
    pub worktrees: HashMap<String, PathBuf>,
    /// Review branches that still exist locally.
    pub reviews: Vec<Review>,
    upstreams: HashMap<String, (String, String)>,
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
        let mut reviews = reviews(&repo);
        let tracked = tracked(&store, &reviews);
        let refs = Refs::load(&repo, &remote, &tracked)?;
        reviews.retain(|r| refs.heads.contains_key(&r.branch));
        let worktrees = git::worktrees(&info.common_dir);
        let upstreams = upstreams(&repo, &tracked);
        Ok(Ctx { info, store, remote, refs, worktrees, reviews, upstreams, lock, gh: OnceCell::new() })
    }

    pub fn gh(&self) -> Result<&Client> {
        if let Some(c) = self.gh.get() {
            return Ok(c);
        }
        let urls = git::remote_urls(&self.remote);
        let url = urls
            .iter()
            .find(|u| crate::github::parse_remote(u).is_some())
            .or(urls.first())
            .ok_or_else(|| anyhow!("no remote named {}", self.remote))?;
        let client = Client::new(url)?;
        Ok(self.gh.get_or_init(|| client))
    }

    pub fn reload(&mut self) -> Result<()> {
        // A fresh handle so ref and object caches see what git just wrote.
        let repo = git::open()?;
        self.info = git::repo_info(&repo)?;
        let mut reviews = reviews(&repo);
        let tracked = tracked(&self.store, &reviews);
        self.refs = Refs::load(&repo, &self.remote, &tracked)?;
        reviews.retain(|r| self.refs.heads.contains_key(&r.branch));
        self.reviews = reviews;
        self.worktrees = git::worktrees(&self.info.common_dir);
        self.upstreams = upstreams(&repo, &tracked);
        Ok(())
    }

    /// Points each branch's upstream at the same-named branch on the remote,
    /// which is what `gh pr view`/`gh pr status` and editors use to find its PR.
    /// Only touches branches that exist on the remote and aren't already set up.
    pub fn ensure_upstreams<'a>(&self, branches: impl IntoIterator<Item = &'a str>) {
        for b in branches {
            if !self.refs.heads.contains_key(b) || !self.refs.remote.contains_key(b) {
                continue;
            }
            let want = (self.remote.clone(), format!("refs/heads/{b}"));
            if self.upstreams.get(b) == Some(&want) {
                continue;
            }
            let upstream = format!("--set-upstream-to={}", self.remote_ref(b));
            if let Err(e) = git::run(&["branch", "--quiet", &upstream, b]) {
                crate::ui::warn(&format!("couldn't set upstream of {b}: {e:#}"));
            }
        }
    }

    /// Every stacked branch and trunk across all stacks.
    pub fn all_branches(&self) -> Vec<String> {
        let mut v: Vec<String> = tracked(&self.store, &[]).into_iter().collect();
        v.sort();
        v
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
        self.store.file.find(cur).ok_or_else(|| anyhow!("{cur} is not part of a stack (see `gt ls`, `gt track`)"))
    }

    pub fn remote_ref(&self, branch: &str) -> String {
        format!("refs/remotes/{}/{branch}", self.remote)
    }

    /// Where a review branch's PR head was last fetched to: the remote-tracking
    /// branch, or for a fork PR, `refs/gt/pull/<N>`.
    pub fn review_ref(&self, r: &Review) -> String {
        match r.fork_pr() {
            Some(n) => format!("refs/gt/pull/{n}"),
            None => self.remote_ref(&r.branch),
        }
    }

    /// The commit at `review_ref`, if it's been fetched.
    pub fn review_tip(&self, r: &Review) -> Option<&str> {
        match r.fork_pr() {
            Some(n) => self.refs.pulls.get(&n),
            None => self.refs.remote.get(&r.branch),
        }
        .map(String::as_str)
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
    /// `include_merged` adds merged branches that still exist locally (prune candidates).
    pub fn pr_queries(&self, si: usize, include_merged: bool) -> Vec<(String, PrQuery)> {
        self.store.file.stacks[si]
            .branches
            .iter()
            .filter(|b| !b.merged() || (include_merged && self.refs.heads.contains_key(&b.branch)))
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
