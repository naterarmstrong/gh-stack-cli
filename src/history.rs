//! A branch's one commit, version by version. Every `gt modify` amends that
//! commit, so the branch's reflog is its version history: each ref move is an
//! entry, and each new patch is a version (p1, p2, …). A move that keeps the
//! patch (a restack, a reset back to it) folds into the version it carries,
//! keyed by `git patch-id`, so only real edits count as new versions.
//!
//! What a version changed is an interdiff, not a tree diff: across a restack
//! the base moved, and a tree diff would fold in everything that landed on the
//! parent meanwhile. So the previous patch is re-applied onto this version's
//! base (`git merge-tree`) and diffed against it. An amend keeps its base, so
//! the common case needs no merge at all.
//!
//! Everything derived from immutable inputs is cached under `<common-dir>/gt/`:
//! patch-ids and re-applied trees in `history-v<N>.json` (each entry dropped
//! once it goes a month without being read),
//! diff sizes in loc's cache. A warm run reads a few reflogs in-process and
//! spawns a single `git rev-list`.

use crate::git::{self, Merge};
use crate::loc;
use crate::ui;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Bump when what the cache stores changes meaning, so old entries are dropped.
const CACHE_VERSION: u32 = 2;
/// Stands in for a version argument to mean "the commit the other side sits on".
pub const BASE_ARG: &str = "base";
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

// ───────────────────────────── reflogs ─────────────────────────────

pub struct Entry {
    pub sha: String,
    pub time: gix::date::Time,
    pub message: String,
}

/// A ref's reflog, oldest first, read in-process. Deletions are skipped; a ref
/// with no reflog (never written here, or expired) gives an empty list.
pub fn reflog(repo: &gix::Repository, refname: &str) -> Vec<Entry> {
    let Ok(Some(r)) = repo.try_find_reference(refname) else { return vec![] };
    let mut platform = r.log_iter();
    let Ok(Some(lines)) = platform.all() else { return vec![] };
    lines
        .filter_map(Result::ok)
        .filter_map(|l| {
            let sha = l.new_oid.to_string();
            (!sha.bytes().all(|b| b == b'0')).then(|| Entry {
                sha,
                time: l.signature.time().unwrap_or_default(),
                message: l.message.to_string(),
            })
        })
        .collect()
}

/// What a reflog message says happened, as a short label.
pub fn action(message: &str) -> &'static str {
    // Matched in order, so longer prefixes come before the ones they extend.
    const PREFIXES: &[(&str, &str)] = &[
        ("commit (amend)", "amend"),
        ("commit (initial)", "create"),
        ("commit (merge)", "merge"),
        ("commit", "commit"),
        ("branch: Created", "create"),
        ("branch: Reset", "reset"),
        ("gt history", "revert"),
        ("gt restack", "restack"),
        ("gt sync", "sync"),
        ("gt get", "get"),
        ("gt review", "review"),
        // Moves gt made before it labelled them were all restacks or syncs.
        ("gt", "restack"),
        ("rebase", "rebase"),
        ("reset", "reset"),
        ("pull", "pull"),
        ("cherry-pick", "pick"),
        ("merge", "merge"),
    ];
    PREFIXES.iter().find(|(p, _)| message.starts_with(p)).map(|(_, a)| *a).unwrap_or("other")
}

// ───────────────────────────── cache ─────────────────────────────

/// Entries nobody has read for this long are dropped when the cache loads.
const PRUNE_AFTER_SECS: i64 = 30 * 86400;
/// A read only rewrites an entry's last-used time once it is this old, so a
/// warm run doesn't rewrite the cache file.
const TOUCH_AFTER_SECS: i64 = 86400;

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

#[derive(Serialize, Deserialize)]
struct Slot {
    value: String,
    /// When the entry was last written or read, in unix seconds.
    used: i64,
}

/// A string map whose entries remember when they were last used.
#[derive(Serialize, Deserialize, Default)]
#[serde(transparent)]
struct Table {
    map: HashMap<String, Slot>,
    #[serde(skip)]
    dirty: bool,
}

impl Table {
    fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    fn get(&mut self, key: &str) -> Option<&str> {
        let now = now();
        let slot = self.map.get_mut(key)?;
        if now - slot.used > TOUCH_AFTER_SECS {
            slot.used = now;
            self.dirty = true;
        }
        Some(&slot.value)
    }

    fn insert(&mut self, key: String, value: String) {
        self.map.insert(key, Slot { value, used: now() });
        self.dirty = true;
    }

    fn prune(&mut self, cutoff: i64) {
        let before = self.map.len();
        self.map.retain(|_, s| s.used >= cutoff);
        self.dirty |= self.map.len() != before;
    }
}

/// Results that only depend on commit and tree ids, so they never go stale;
/// entries are dropped only once they go unused for `PRUNE_AFTER_SECS`.
#[derive(Serialize, Deserialize, Default)]
struct Cache {
    /// Commit → its patch-id, or "" when it has no diff (empty, or a merge).
    #[serde(default)]
    patch_ids: Table,
    /// `<commit> <onto>` → the tree of that commit's patch re-applied onto
    /// `onto`, or "" when the re-apply conflicts.
    #[serde(default)]
    rebased: Table,
    #[serde(skip)]
    path: PathBuf,
}

impl Cache {
    fn load(common_dir: &Path) -> Cache {
        let dir = common_dir.join("gt");
        let path = dir.join(format!("history-v{CACHE_VERSION}.json"));
        let mut c: Cache = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        c.path = path;
        let cutoff = now() - PRUNE_AFTER_SECS;
        c.patch_ids.prune(cutoff);
        c.rebased.prune(cutoff);
        // Files from older cache versions are never read again.
        for v in 1..CACHE_VERSION {
            let _ = std::fs::remove_file(dir.join(format!("history-v{v}.json")));
        }
        c
    }

    /// Best effort, and last writer wins: anything lost is just recomputed.
    fn save(&self) {
        if !self.patch_ids.dirty && !self.rebased.dirty {
            return;
        }
        let Ok(data) = serde_json::to_vec(self) else { return };
        let _ = std::fs::create_dir_all(self.path.parent().unwrap());
        let tmp = self.path.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, data).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

// ───────────────────────────── commits ─────────────────────────────

struct Info {
    parent: Option<String>,
    tree: String,
    subject: String,
}

/// Commits decoded in-process, each at most once.
struct Commits<'r> {
    repo: &'r gix::Repository,
    seen: HashMap<String, Option<Info>>,
}

impl<'r> Commits<'r> {
    fn new(repo: &'r gix::Repository) -> Self {
        Commits { repo, seen: HashMap::new() }
    }

    fn get(&mut self, sha: &str) -> Option<&Info> {
        if !self.seen.contains_key(sha) {
            let info = (|| {
                let c = self.repo.find_commit(gix::ObjectId::from_hex(sha.as_bytes()).ok()?).ok()?;
                Some(Info {
                    parent: c.parent_ids().next().map(|p| p.to_string()),
                    tree: c.tree_id().ok()?.to_string(),
                    subject: c.message().map(|m| m.summary().to_string()).unwrap_or_default(),
                })
            })();
            self.seen.insert(sha.to_string(), info);
        }
        self.seen[sha].as_ref()
    }

    fn parent(&mut self, sha: &str) -> Option<String> {
        self.get(sha).and_then(|i| i.parent.clone())
    }

    fn exists(&self, sha: &str) -> bool {
        gix::ObjectId::from_hex(sha.as_bytes()).is_ok_and(|id| self.repo.has_object(id))
    }
}

// ───────────────────────────── patch-ids ─────────────────────────────

/// Patch-ids for `shas`, from the cache or else from one
/// `git diff-tree --stdin | git patch-id` pipeline for all the missing ones.
fn patch_ids(cache: &mut Cache, shas: &[&str]) -> Result<()> {
    let missing: Vec<&str> =
        shas.iter().copied().filter(|s| !cache.patch_ids.contains(s)).collect::<HashSet<_>>().into_iter().collect();
    if missing.is_empty() {
        return Ok(());
    }
    // -U0: context lines come from the base, so with them a restack onto a
    // changed base would read as a new patch.
    let mut diff = Command::new(git::git_bin())
        .args(["diff-tree", "--stdin", "-p", "--unified=0", "--no-color", "--no-ext-diff", "--no-textconv"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning git diff-tree")?;
    let ids = Command::new(git::git_bin())
        .args(["patch-id", "--stable"])
        .stdin(diff.stdout.take().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning git patch-id")?;
    // Feed diff-tree from another thread: with enough shas every pipe in the
    // chain can fill up at once.
    let input: String = missing.iter().map(|s| format!("{s}\n")).collect();
    let mut stdin = diff.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let out = ids.wait_with_output()?;
    let _ = writer.join();
    let _ = diff.wait();

    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((pid, sha)) = line.split_once(' ') {
            cache.patch_ids.insert(sha.trim().to_string(), pid.to_string());
        }
    }
    for s in missing {
        if !cache.patch_ids.contains(s) {
            cache.patch_ids.insert(s.to_string(), String::new());
        }
    }
    Ok(())
}

// ───────────────────────────── re-applying patches ─────────────────────────────

/// The tree of `sha`'s patch re-applied onto `onto`, or None if that conflicts.
/// A 3-way merge whose base is `sha`'s own parent: just that patch, somewhere else.
fn rebased_tree(sha: &str, parent: &str, onto: &str) -> Result<Option<String>> {
    Ok(match git::merge_tree(parent, onto, sha)? {
        Merge::Clean(tree) => Some(tree),
        Merge::Conflict(_) => None,
    })
}

/// `rebased_tree` for every (sha, onto) pair, from the cache or else in
/// parallel. A cached tree that `git gc` has since pruned is recomputed.
fn rebase_all(cache: &mut Cache, commits: &mut Commits, pairs: &[(String, String)]) -> Result<()> {
    let mut todo = vec![];
    for (sha, onto) in pairs {
        let key = format!("{sha} {onto}");
        let fresh = cache.rebased.get(&key).is_some_and(|t| t.is_empty() || commits.exists(t));
        if fresh || todo.iter().any(|(k, ..): &(String, String, String, String)| *k == key) {
            continue;
        }
        let parent = commits.parent(sha).ok_or_else(|| anyhow!("{} is a root commit", ui::short(sha)))?;
        todo.push((key, sha.clone(), parent, onto.clone()));
    }
    let results = par_map(&todo, |(_, sha, parent, onto)| rebased_tree(sha, parent, onto));
    for ((key, ..), tree) in todo.into_iter().zip(results) {
        cache.rebased.insert(key, tree?.unwrap_or_default());
    }
    Ok(())
}

fn cached_rebase(cache: &mut Cache, sha: &str, onto: &str) -> Option<String> {
    cache.rebased.get(&format!("{sha} {onto}")).filter(|t| !t.is_empty()).map(str::to_string)
}

/// `f` over `items` on a few threads, results in order. Each item here costs a
/// git process, so the cold path scales with cores instead of version count.
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    if items.len() < 2 {
        return items.iter().map(f).collect();
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8).min(items.len());
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    *slots[i].lock().unwrap() = Some(f(item));
                }
            });
        }
    });
    slots.into_iter().map(|m| m.into_inner().unwrap().unwrap()).collect()
}

// ───────────────────────────── versions ─────────────────────────────

/// Where a branch sits, as the caller's stack knows it.
pub struct Placement {
    pub parent: String,
    /// Commits that belong below the branch (its parent, the trunk, lower branches).
    pub below: Vec<String>,
    /// Branches below it whose reflogs hold commits that were never its own,
    /// such as an empty branch following its parent through restacks.
    pub lower: Vec<String>,
}

#[derive(Serialize, Clone)]
pub struct Move {
    pub sha: String,
    pub time: i64,
    pub action: &'static str,
}

#[derive(Serialize, Default)]
pub struct Remote {
    /// Pushed from this clone.
    pub pushed: bool,
    /// Seen on the remote by a fetch from this clone (someone else's push, or our own fetched back).
    pub fetched: bool,
    /// What the remote branch points at now, as of the last fetch or push.
    pub tip: bool,
}

#[derive(Serialize)]
pub struct Delta {
    /// The version this one is measured against, or None for the first, which
    /// is measured against its own base.
    pub vs: Option<usize>,
    /// Re-applying the earlier patch conflicts, so there is no honest interdiff.
    pub conflict: bool,
    pub total: loc::Split,
    pub files: Vec<loc::FileStat>,
}

#[derive(Serialize)]
pub struct Version {
    pub version: usize,
    /// The latest commit to carry this patch.
    pub sha: String,
    pub subject: String,
    /// When the patch first appeared, and how.
    pub time: i64,
    pub date: String,
    pub action: &'static str,
    pub current: bool,
    pub remote: Remote,
    /// Every move of the branch onto this patch, oldest first, the first included.
    pub moves: Vec<Move>,
    pub delta: Option<Delta>,
    /// The version the branch was on just before this one appeared, and its commit then.
    #[serde(skip)]
    from: Option<(usize, String)>,
}

impl Version {
    pub fn label(&self) -> String {
        format!("p{}", self.version)
    }

    /// Every commit that carried this patch, oldest first.
    pub fn shas(&self) -> Vec<&str> {
        let mut v: Vec<&str> = vec![];
        for m in &self.moves {
            if !v.contains(&m.sha.as_str()) {
                v.push(&m.sha);
            }
        }
        v
    }

    fn first_sha(&self) -> &str {
        &self.moves[0].sha
    }
}

#[derive(Serialize)]
pub struct History {
    pub branch: String,
    pub parent: String,
    pub tip: String,
    /// Newest first in JSON and on screen; oldest first here.
    pub versions: Vec<Version>,
    #[serde(skip)]
    cache: Cache,
}

/// Builds `branch`'s version history. `with_deltas` also sizes what each version changed.
pub fn load(common_dir: &Path, remote: &str, branch: &str, at: &Placement, with_deltas: bool) -> Result<History> {
    let repo = git::open()?;
    let mut commits = Commits::new(&repo);
    let mut cache = Cache::load(common_dir);
    let tip = repo
        .try_find_reference(format!("refs/heads/{branch}").as_str())?
        .and_then(|r| r.try_id().map(|id| id.to_string()))
        .ok_or_else(|| anyhow!("no local branch {branch}"))?;
    let entries = reflog(&repo, &format!("refs/heads/{branch}"));
    if entries.is_empty() {
        bail!("{branch} has no reflog (reflogs are per-clone, and git expires old entries)");
    }

    // The branch's own entries: not a commit that ever sat on a lower branch,
    // and not reachable from what's below it now. That drops the creation point
    // and any stretch where the branch had no commit and followed its parent.
    let mut theirs: HashSet<String> = at.below.iter().cloned().collect();
    for b in &at.lower {
        theirs.extend(reflog(&repo, &format!("refs/heads/{b}")).into_iter().map(|e| e.sha));
    }
    let candidates: Vec<&str> = {
        let mut seen = HashSet::new();
        entries
            .iter()
            .map(|e| e.sha.as_str())
            .filter(|s| !theirs.contains(*s) && commits.exists(s) && seen.insert(*s))
            .collect()
    };
    let own = not_below(&candidates, &at.below.iter().filter(|s| commits.exists(s)).collect::<Vec<_>>())?;
    let entries: Vec<&Entry> = entries.iter().filter(|e| own.contains(&e.sha)).collect();
    if entries.is_empty() {
        bail!("{branch} has no commit of its own in its reflog");
    }
    patch_ids(&mut cache, &entries.iter().map(|e| e.sha.as_str()).collect::<Vec<_>>())?;

    let mut versions: Vec<Version> = vec![];
    let mut by_patch: HashMap<String, usize> = HashMap::new();
    let mut last: Option<usize> = None;
    for e in entries {
        let key = match cache.patch_ids.get(&e.sha).filter(|p| !p.is_empty()) {
            Some(pid) => pid.to_string(),
            None => format!("tree:{}", commits.get(&e.sha).map(|i| i.tree.as_str()).unwrap_or(&e.sha)),
        };
        let mv = Move { sha: e.sha.clone(), time: e.time.seconds, action: action(&e.message) };
        let i = match by_patch.get(&key) {
            Some(&i) => {
                let v = &mut versions[i];
                if v.sha != e.sha || last != Some(i) {
                    v.sha = e.sha.clone();
                    // A message-only amend keeps the patch; show the latest message.
                    v.subject = commits.get(&e.sha).map(|c| c.subject.clone()).unwrap_or_default();
                    v.moves.push(mv);
                }
                i
            }
            None => {
                let i = versions.len();
                versions.push(Version {
                    version: i + 1,
                    sha: e.sha.clone(),
                    subject: commits.get(&e.sha).map(|c| c.subject.clone()).unwrap_or_default(),
                    time: e.time.seconds,
                    date: e.time.format_or_unix(gix::date::time::format::ISO8601_STRICT),
                    action: mv.action,
                    current: false,
                    remote: Remote::default(),
                    moves: vec![mv],
                    delta: None,
                    from: last.map(|l| (l, versions[l].sha.clone())),
                });
                by_patch.insert(key, i);
                i
            }
        };
        last = Some(i);
    }

    let remote_ref = format!("refs/remotes/{remote}/{branch}");
    let remote_log = reflog(&repo, &remote_ref);
    let remote_tip = repo.try_find_reference(remote_ref.as_str())?.and_then(|r| r.try_id().map(|id| id.to_string()));
    for v in versions.iter_mut() {
        v.current = v.sha == tip;
        let shas: Vec<String> = v.shas().into_iter().map(str::to_string).collect();
        let shas: Vec<&str> = shas.iter().map(String::as_str).collect();
        for e in remote_log.iter().filter(|e| shas.contains(&e.sha.as_str())) {
            if e.message.starts_with("update by push") {
                v.remote.pushed = true;
            } else {
                v.remote.fetched = true;
            }
        }
        v.remote.tip = remote_tip.as_deref().is_some_and(|t| shas.contains(&t));
    }

    let mut h = History { branch: branch.to_string(), parent: at.parent.clone(), tip, versions, cache };
    if with_deltas {
        attach_deltas(&mut h, common_dir, &mut commits)?;
    }
    h.cache.save();
    Ok(h)
}

/// Which of `shas` are not reachable from any of `below`, in one `git rev-list`.
fn not_below(shas: &[&str], below: &[&String]) -> Result<HashSet<String>> {
    if shas.is_empty() {
        return Ok(HashSet::new());
    }
    let input: String = shas.iter().map(|s| format!("{s}\n")).chain(below.iter().map(|b| format!("^{b}\n"))).collect();
    let out = git::run_raw(None, &["rev-list", "--stdin"], Some(&input), &[])?;
    if !out.ok {
        bail!("git rev-list failed: {}", out.stderr.trim());
    }
    let reachable: HashSet<&str> = out.stdout.lines().collect();
    Ok(shas.iter().filter(|s| reachable.contains(*s)).map(|s| s.to_string()).collect())
}

/// Sizes each version's change against the version the branch was on just
/// before it (for the first version, against its own base). Measured on the
/// version's first commit, which is what the edit produced: an amend keeps its
/// base, so that is usually a plain diff of two commits, and the results stay
/// cacheable as the reflog grows.
fn attach_deltas(h: &mut History, common_dir: &Path, commits: &mut Commits) -> Result<()> {
    enum Base {
        Own,
        Commit(String),
        Rebased(String, String),
    }
    let mut bases = vec![];
    let mut pairs = vec![];
    for v in &h.versions {
        let to = v.first_sha().to_string();
        let base = match &v.from {
            None => Base::Own,
            Some((_, from)) => {
                let onto = commits.parent(&to);
                if onto.is_some() && commits.parent(from) == onto {
                    Base::Commit(from.clone())
                } else if let Some(onto) = onto {
                    pairs.push((from.clone(), onto.clone()));
                    Base::Rebased(from.clone(), onto)
                } else {
                    Base::Own
                }
            }
        };
        bases.push((to, base));
    }
    rebase_all(&mut h.cache, commits, &pairs)?;

    // None: conflict. Some(None): against its own parent.
    let jobs: Vec<(String, Option<Option<String>>)> = bases
        .into_iter()
        .map(|(to, base)| {
            let b = match base {
                Base::Own => Some(None),
                Base::Commit(c) => Some(Some(c)),
                Base::Rebased(from, onto) => cached_rebase(&mut h.cache, &from, &onto).map(Some),
            };
            (to, b)
        })
        .collect();
    let diffs = par_map(&jobs, |(to, base)| match base {
        None => Ok(None),
        Some(b) => loc::of_commit(common_dir, to, b.as_deref()).map(Some),
    });
    for (v, d) in h.versions.iter_mut().zip(diffs) {
        let vs = v.from.as_ref().map(|(i, _)| i + 1);
        v.delta = Some(match d? {
            Some(d) => Delta { vs, conflict: false, total: d.total, files: d.files },
            None => Delta { vs, conflict: true, total: loc::Split::default(), files: vec![] },
        });
    }
    Ok(())
}

// ───────────────────────────── resolving and comparing ─────────────────────────────

pub struct Point {
    pub label: String,
    pub sha: String,
}

impl History {
    pub fn current(&self) -> Option<&Version> {
        self.versions.iter().find(|v| v.current)
    }

    /// A version argument (`p3`, `3`, or any commit) as a commit.
    pub fn resolve(&self, arg: &str) -> Result<Point> {
        let n = arg.strip_prefix(['p', 'P']).unwrap_or(arg);
        if !n.is_empty() && n.len() <= 4 && n.bytes().all(|b| b.is_ascii_digit()) {
            let n: usize = n.parse()?;
            let v = self.versions.iter().find(|v| v.version == n).ok_or_else(|| {
                anyhow!("{} has no p{n} (it has p1–p{}; see `gt history`)", self.branch, self.versions.len())
            })?;
            return Ok(Point { label: v.label(), sha: v.sha.clone() });
        }
        let sha = git::rev_parse(arg).ok_or_else(|| anyhow!("{arg} is neither a version nor a commit"))?;
        let label = self
            .versions
            .iter()
            .find(|v| v.shas().contains(&sha.as_str()))
            .map(Version::label)
            .unwrap_or_else(|| ui::short(&sha).to_string());
        Ok(Point { label, sha })
    }

    /// The two sides of a compare as tree-ishes: `from` re-applied onto `to`'s
    /// base when their bases differ, so the diff is only what changed in the
    /// patch. `base` on either side is the other side's parent.
    pub fn compare(&mut self, from: &str, to: &str) -> Result<(Point, Point, String, String)> {
        let repo = git::open()?;
        let mut commits = Commits::new(&repo);
        let parent_of = |commits: &mut Commits, p: &Point| -> Point {
            let sha = commits.parent(&p.sha).unwrap_or_else(|| EMPTY_TREE.to_string());
            Point { label: BASE_ARG.into(), sha }
        };
        let (a, b) = match (from, to) {
            (BASE_ARG, BASE_ARG) => bail!("`base` needs a version on the other side to resolve against"),
            (BASE_ARG, t) => {
                let b = self.resolve(t)?;
                (parent_of(&mut commits, &b), b)
            }
            (f, BASE_ARG) => {
                let a = self.resolve(f)?;
                let b = parent_of(&mut commits, &a);
                (a, b)
            }
            (f, t) => (self.resolve(f)?, self.resolve(t)?),
        };
        if a.sha == b.sha {
            bail!("{} and {} are the same commit; nothing to compare", a.label, b.label);
        }
        let right = format!("{}^{{tree}}", b.sha);
        if from == BASE_ARG || to == BASE_ARG {
            // One side is the other's parent: no gap between bases to correct for.
            let left = format!("{}^{{tree}}", a.sha);
            return Ok((a, b, left, right));
        }
        let (pa, pb) = (commits.parent(&a.sha), commits.parent(&b.sha));
        if pa.is_some() && pa == pb {
            let left = format!("{}^{{tree}}", a.sha);
            return Ok((a, b, left, right));
        }
        let onto = pb.ok_or_else(|| anyhow!("{} is a root commit", b.label))?;
        rebase_all(&mut self.cache, &mut commits, &[(a.sha.clone(), onto.clone())])?;
        self.cache.save();
        let Some(left) = cached_rebase(&mut self.cache, &a.sha, &onto) else {
            bail!(
                "re-applying {} onto {}'s base conflicts, so there's no clean interdiff; \
                 `git diff {} {}` diffs them directly, including whatever landed on the base in between",
                a.label,
                b.label,
                ui::short(&a.sha),
                ui::short(&b.sha)
            );
        };
        Ok((a, b, left, right))
    }

    /// The commit a revert lands on, and a line saying how it got there.
    ///
    /// Everything happens on the tip's own base, so the revert changes content
    /// only; the restack that follows moves the branch if its parent moved.
    /// `--exact` takes the target as it was instead, base and all (or, for
    /// paths, its blobs verbatim), which is the way through a conflict.
    pub fn plan_revert(&mut self, target: &Point, paths: &[String], exact: bool) -> Result<(String, String)> {
        let repo = git::open()?;
        let mut commits = Commits::new(&repo);
        let base = commits.parent(&self.tip).ok_or_else(|| anyhow!("{} is a root commit", self.branch))?;
        let target_base = commits.parent(&target.sha).ok_or_else(|| anyhow!("{} is a root commit", target.label))?;
        let short = ui::short(&target.sha).to_string();

        let source_tree = if exact || target_base == base {
            git::tree_of(&target.sha)?
        } else {
            rebase_all(&mut self.cache, &mut commits, &[(target.sha.clone(), base.clone())])?;
            self.cache.save();
            cached_rebase(&mut self.cache, &target.sha, &base).ok_or_else(|| {
                anyhow!(
                    "re-applying {} onto the branch's current base conflicts; \
                     rerun with --exact to take it as it was ({short}) instead",
                    target.label
                )
            })?
        };

        if paths.is_empty() {
            if exact || target_base == base {
                let mode = if exact {
                    format!("exact: back to {short}, on its old base")
                } else {
                    format!("{short} already sits on the branch's base")
                };
                return Ok((target.sha.clone(), mode));
            }
            let new = git::commit_tree(&source_tree, &base, &git::commit_meta(&target.sha)?)?;
            return Ok((new, format!("{}'s patch re-applied onto the branch's current base", target.label)));
        }

        let tip_tree = git::tree_of(&self.tip)?;
        let named = paths.join(" ");
        let present = [&tip_tree, &source_tree].iter().any(|t| {
            let mut args = vec!["ls-tree", "-r", "--name-only", "--full-tree", t.as_str(), "--"];
            args.extend(paths.iter().map(String::as_str));
            git::run(&args).is_ok_and(|o| !o.is_empty())
        });
        if !present {
            bail!("nothing matching {named} exists in {} or on the current tip", target.label);
        }
        let new_tree = tree_with_paths(&tip_tree, &source_tree, paths)?;
        if new_tree == tip_tree {
            bail!("{named} already matches {}; nothing to revert", target.label);
        }
        // The message describes the branch, not the reverted files.
        let new = git::commit_tree(&new_tree, &base, &git::commit_meta(&self.tip)?)?;
        let mode = if exact {
            format!("those paths copied verbatim from {short}")
        } else {
            format!("those paths as {} has them, re-applied onto the branch's base", target.label)
        };
        Ok((new, mode))
    }
}

/// `base_tree` with everything under `paths` taken from `source_tree`, and
/// dropped where the source doesn't have it. Built in a throwaway index, so
/// neither the working tree nor the real index is touched.
fn tree_with_paths(base_tree: &str, source_tree: &str, paths: &[String]) -> Result<String> {
    let mut args = vec!["ls-tree", "-r", "-z", "--full-tree", source_tree, "--"];
    args.extend(paths.iter().map(String::as_str));
    let entries = git::run(&args)?;
    // --no-renames: a rename inside the paths would otherwise hide the old name's deletion.
    let top: Vec<String> = paths.iter().map(|p| format!(":(top){p}")).collect();
    let mut args = vec!["diff", "--no-renames", "--name-only", "--diff-filter=D", "-z", base_tree, source_tree, "--"];
    args.extend(top.iter().map(String::as_str));
    let dropped = git::run(&args)?;

    let mut records = String::new();
    for entry in entries.split('\0').filter(|e| !e.is_empty()) {
        let (meta, path) = entry.split_once('\t').ok_or_else(|| anyhow!("unexpected ls-tree line: {entry}"))?;
        let f: Vec<&str> = meta.split(' ').collect();
        records.push_str(&format!("{} {}\t{path}\0", f[0], f[2]));
    }
    for path in dropped.split('\0').filter(|p| !p.is_empty()) {
        // Mode 0 removes the path.
        records.push_str(&format!("0 {}\t{path}\0", "0".repeat(40)));
    }

    let git_dir = PathBuf::from(git::run(&["rev-parse", "--absolute-git-dir"])?);
    let index = git_dir.join(format!("gt-history-index-{}", std::process::id()));
    let index_s = index.to_string_lossy().to_string();
    let env = [("GIT_INDEX_FILE", index_s.as_str())];
    let result = (|| {
        let out = git::run_raw(None, &["read-tree", base_tree], None, &env)?;
        if !out.ok {
            bail!("git read-tree failed: {}", out.stderr.trim());
        }
        if !records.is_empty() {
            let out = git::run_raw(None, &["update-index", "-z", "--index-info"], Some(&records), &env)?;
            if !out.ok {
                bail!("git update-index failed: {}", out.stderr.trim());
            }
        }
        let out = git::run_raw(None, &["write-tree"], None, &env)?;
        if !out.ok {
            bail!("git write-tree failed: {}", out.stderr.trim());
        }
        Ok(out.stdout.trim().to_string())
    })();
    let _ = std::fs::remove_file(&index);
    result
}

/// User paths, relative to where they're standing, as repo-root paths.
pub fn repo_relative(paths: &[String], toplevel: &Path) -> Result<Vec<String>> {
    let cwd = std::env::current_dir()?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    paths
        .iter()
        .map(|p| {
            let mut parts: Vec<String> = vec![];
            let joined = cwd.join(p);
            let rel = joined.strip_prefix(toplevel).map_err(|_| anyhow!("{p} is outside the repository"))?;
            for c in rel.components() {
                match c {
                    std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
                    std::path::Component::ParentDir => {
                        parts.pop().ok_or_else(|| anyhow!("{p} is outside the repository"))?;
                    }
                    _ => {}
                }
            }
            Ok(parts.join("/"))
        })
        .collect()
}

/// Files that differ between two tree-ishes, for `--json`: (status, path, old path).
pub fn changed_files(left: &str, right: &str) -> Result<Vec<serde_json::Value>> {
    let out = git::run(&["diff", "--name-status", "-z", "--find-renames", left, right])?;
    let mut it = out.split('\0').filter(|s| !s.is_empty());
    let mut files = vec![];
    while let Some(status) = it.next() {
        let letter = &status[..1];
        let (old, path) = if matches!(letter, "R" | "C") {
            (it.next().map(str::to_string), it.next().unwrap_or_default().to_string())
        } else {
            (None, it.next().unwrap_or_default().to_string())
        };
        files.push(serde_json::json!({ "status": letter, "path": path, "old_path": old }));
    }
    Ok(files)
}

// ───────────────────────────── display ─────────────────────────────

fn pad(plain: &str, colored: String, width: usize) -> String {
    format!("{colored}{}", " ".repeat(width.saturating_sub(plain.chars().count())))
}

fn when(secs: i64) -> String {
    ui::ago(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs.max(0) as u64))
}

pub fn render(h: &History) {
    let subjects: HashSet<&str> = h.versions.iter().map(|v| v.subject.as_str()).collect();
    let uniform = subjects.len() == 1;
    let mut title = format!("{}  {}", ui::bold(&h.branch), ui::dim(&format!("on {}", h.parent)));
    if uniform {
        title.push_str(&format!("  {}", h.versions[0].subject));
    }
    println!("{title}");
    println!("{}", ui::dim("  PATCH  SHA       WHEN      ACTION   REMOTE     Δ VS PREVIOUS"));

    for v in h.versions.iter().rev() {
        let label = format!("{}{}", v.label(), if v.current { "*" } else { "" });
        let colored_label = if v.current { ui::bold(&ui::cyan(&label)) } else { ui::cyan(&label) };
        let (remote, remote_c) = if v.remote.tip {
            ("on remote", ui::green("on remote"))
        } else if v.remote.pushed {
            ("pushed", ui::faint_green("pushed"))
        } else if v.remote.fetched {
            ("fetched", ui::dim("fetched"))
        } else {
            ("-", ui::dim("-"))
        };
        let delta = match &v.delta {
            None => String::new(),
            Some(d) if d.conflict => ui::red("n/a (re-applying the previous patch conflicts)"),
            Some(d) => {
                let (_, s) = loc::summary(&d.total);
                let s = if s.is_empty() { ui::dim("no change in content") } else { s };
                match d.vs {
                    Some(vs) if vs + 1 != v.version => format!("{s}  {}", ui::dim(&format!("(vs p{vs})"))),
                    _ => s,
                }
            }
        };
        let when = when(v.time);
        println!(
            "  {} {} {} {} {} {delta}",
            pad(&label, colored_label, 6),
            pad(ui::short(&v.sha), ui::yellow(ui::short(&v.sha)), 9),
            pad(&when, when.clone(), 9),
            pad(v.action, v.action.to_string(), 8),
            pad(remote, remote_c, 10),
        );
        print_details(v, uniform);
    }
    println!();
    println!("{}", ui::dim("* = current tip. Remote marks are this clone's view: what it pushed, or saw in a fetch."));
}

fn print_details(v: &Version, uniform: bool) {
    let indent = "         ";
    if !uniform {
        println!("{indent}{}", ui::dim(&v.subject));
    }
    if let Some(d) = v.delta.as_ref().filter(|d| !d.conflict) {
        let (gen_files, mut files): (Vec<&loc::FileStat>, Vec<&loc::FileStat>) =
            d.files.iter().partition(|f| f.kind == loc::Kind::Gen);
        files.sort_by_key(|f| std::cmp::Reverse(f.lines.sum().add + f.lines.sum().del));
        for f in files.iter().take(3) {
            let (_, size) = loc::counts(&f.lines.sum());
            println!("{indent}{} {} {size}", ui::dim("±"), ui::dim(&f.path));
        }
        if files.len() > 3 {
            println!("{indent}{}", ui::dim(&format!("  … {} more", files.len() - 3)));
        }
        if !gen_files.is_empty() {
            let n = gen_files.len();
            println!("{indent}{}", ui::dim(&format!("⊘ {n} generated file{}", if n == 1 { "" } else { "s" })));
        }
    }
    let shas = v.shas();
    if shas.len() > 1 {
        let prior: Vec<&str> = shas[..shas.len() - 1].iter().map(|s| ui::short(s)).collect();
        println!("{indent}{}", ui::dim(&format!("↺ {}", prior.join(" "))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions() {
        assert_eq!(action("commit (amend): Add the parser"), "amend");
        assert_eq!(action("commit: Add the parser"), "commit");
        assert_eq!(action("branch: Created from HEAD"), "create");
        assert_eq!(action("gt"), "restack");
        assert_eq!(action("gt restack: moving to abc"), "restack");
        assert_eq!(action("gt history: revert to p2"), "revert");
        assert_eq!(action("rebase (finish): refs/heads/x onto abc"), "rebase");
        assert_eq!(action("reset: moving to HEAD~1"), "reset");
        assert_eq!(action("something else"), "other");
    }

    #[test]
    fn cache_prunes_unused_entries() {
        let mut t = Table::default();
        t.map.insert("old".into(), Slot { value: "a".into(), used: now() - PRUNE_AFTER_SECS - 1 });
        t.map.insert("stale".into(), Slot { value: "b".into(), used: now() - TOUCH_AFTER_SECS - 1 });
        t.insert("new".into(), "c".into());
        t.dirty = false;
        // A read refreshes an entry's age, so only entries nobody reads expire.
        assert_eq!(t.get("stale"), Some("b"));
        assert!(t.dirty && t.map["stale"].used > now() - 5);
        t.prune(now() - PRUNE_AFTER_SECS);
        assert!(!t.contains("old") && t.contains("stale") && t.contains("new"));
    }

    #[test]
    fn parallel_map_keeps_order() {
        let items: Vec<u32> = (0..50).collect();
        assert_eq!(par_map(&items, |n| n * 2), items.iter().map(|n| n * 2).collect::<Vec<_>>());
    }
}
