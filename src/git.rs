use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

pub struct Output {
    pub ok: bool,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// The real git binary. On macOS `/usr/bin/git` is an xcrun trampoline that
/// costs ~6ms per spawn, so resolve the active developer dir ourselves.
pub fn git_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        if let Some(p) = std::env::var_os("GT_GIT") {
            return PathBuf::from(p);
        }
        let first = std::env::var_os("PATH")
            .and_then(|p| std::env::split_paths(&p).map(|d| d.join("git")).find(|g| g.is_file()));
        match first {
            Some(g) if g == Path::new("/usr/bin/git") => std::fs::read_link("/var/db/xcode_select_link")
                .map(|dev| dev.join("usr/bin/git"))
                .ok()
                .filter(|g| g.is_file())
                .unwrap_or(g),
            Some(g) => g,
            None => PathBuf::from("git"),
        }
    })
}

fn cmd(dir: Option<&Path>, args: &[&str]) -> Command {
    let mut c = Command::new(git_bin());
    if let Some(d) = dir {
        c.arg("-C").arg(d);
    }
    c.args(args);
    c
}

/// Run git and capture output, without failing on a non-zero exit.
pub fn run_raw(dir: Option<&Path>, args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> Result<Output> {
    let mut c = cmd(dir, args);
    for (k, v) in env {
        c.env(k, v);
    }
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    if args.first() == Some(&"fetch") {
        *CHAINS.lock().unwrap() = None;
    }
    let started = std::time::Instant::now();
    let mut child = c.spawn().context("spawning git")?;
    // Written from another thread while the output is read, or git blocks on a
    // full stdout pipe while we block writing stdin it has stopped reading.
    let writer = stdin.map(|input| {
        let mut pipe = child.stdin.take().unwrap();
        let input = input.to_owned();
        std::thread::spawn(move || pipe.write_all(input.as_bytes()))
    });
    let out = child.wait_with_output()?;
    if let Some(w) = writer {
        w.join().expect("stdin writer")?;
    }
    log_git(args, out.status.code().unwrap_or(-1), started, &out.stderr, Some(out.stdout.len()), stdin.map(str::len));
    Ok(Output {
        ok: out.status.success(),
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Run git, fail with its stderr on a non-zero exit, return trimmed stdout.
pub fn run(args: &[&str]) -> Result<String> {
    run_in(None, args)
}

pub fn run_in(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let out = run_raw(dir, args, None, &[])?;
    if !out.ok {
        bail!("git {} failed: {}", args.join(" "), out.stderr.trim());
    }
    Ok(out.stdout.trim_end().to_string())
}

/// Run git with the terminal attached (editors, hooks, progress output).
pub fn run_interactive(dir: Option<&Path>, args: &[&str]) -> Result<bool> {
    let started = std::time::Instant::now();
    let status = cmd(dir, args).status().context("spawning git")?;
    log_git(args, status.code().unwrap_or(-1), started, b"", None, None);
    Ok(status.success())
}

/// Records a git call in the journal: its args (the first 20), exit code,
/// time, and stderr; stdout and stdin only by size.
pub fn log_git(
    args: &[&str],
    code: i32,
    started: std::time::Instant,
    stderr: &[u8],
    stdout_len: Option<usize>,
    stdin_len: Option<usize>,
) {
    let mut f = serde_json::json!({ "code": code, "ms": crate::journal::ms(started) });
    f["args"] = args.iter().take(20).copied().collect::<Vec<_>>().into();
    if args.len() > 20 {
        f["more_args"] = (args.len() - 20).into();
    }
    let stderr = String::from_utf8_lossy(stderr);
    if !stderr.trim().is_empty() {
        f["stderr"] = crate::journal::truncate(stderr.trim()).into();
    }
    if let Some(n) = stdout_len {
        f["stdout_bytes"] = n.into();
    }
    if let Some(n) = stdin_len {
        f["stdin_bytes"] = n.into();
    }
    crate::journal::event("git", f);
}

pub struct RepoInfo {
    pub common_dir: PathBuf,
    pub git_dir: PathBuf,
    pub toplevel: PathBuf,
    pub current_branch: Option<String>,
    /// Lowercased emails and names that count as "me": every `gt.me` value,
    /// else `user.email` and `user.name`.
    pub me: Vec<String>,
}

/// Opens the repository in-process (no git spawn).
pub fn open() -> Result<gix::Repository> {
    gix::discover(".").map_err(|_| anyhow::anyhow!("not inside a git repository"))
}

pub fn repo_info(repo: &gix::Repository) -> Result<RepoInfo> {
    let abs = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let info = RepoInfo {
        common_dir: abs(repo.common_dir()),
        git_dir: abs(repo.git_dir()),
        toplevel: abs(repo.workdir().ok_or_else(|| anyhow::anyhow!("bare repositories aren't supported"))?),
        current_branch: repo
            .head_name()?
            .and_then(|n| n.as_bstr().to_string().strip_prefix("refs/heads/").map(str::to_string)),
        me: identities(repo),
    };
    crate::journal::repo(&info.toplevel, info.current_branch.as_deref());
    Ok(info)
}

fn identities(repo: &gix::Repository) -> Vec<String> {
    let cfg = repo.config_snapshot();
    let mut me: Vec<String> =
        cfg.plumbing().strings("gt.me").unwrap_or_default().iter().map(|v| v.to_string()).collect();
    if me.is_empty() {
        me = ["user.email", "user.name"].iter().filter_map(|k| cfg.string(*k)).map(|v| v.to_string()).collect();
    }
    me.into_iter().map(|v| v.trim().to_lowercase()).filter(|v| !v.is_empty()).collect()
}

#[derive(Clone, Debug)]
pub struct Commit {
    pub sha: String,
    /// `parents` and `subject` are only decoded for branches gt cares about
    /// (stacked branches and trunks); other branches carry just their sha.
    pub parents: Vec<String>,
    pub subject: String,
    pub author_name: String,
    pub author_email: String,
}

/// Snapshot of every local branch and remote-tracking ref, read in-process.
pub struct Refs {
    pub heads: HashMap<String, Commit>,
    pub remote: HashMap<String, String>,
    /// Fork PR heads fetched by `gt review`, by PR number (`refs/gt/pull/<N>`).
    pub pulls: HashMap<u64, String>,
}

impl Refs {
    pub fn load(repo: &gix::Repository, remote: &str, decode: &HashSet<String>) -> Result<Refs> {
        let platform = repo.references()?;
        let mut heads = HashMap::new();
        for r in platform.local_branches()? {
            let Ok(r) = r else { continue };
            let Some(id) = r.try_id() else { continue };
            let name = r.name().shorten().to_string();
            let mut c = Commit {
                sha: id.to_string(),
                parents: vec![],
                subject: String::new(),
                author_name: String::new(),
                author_email: String::new(),
            };
            if decode.contains(&name) {
                if let Ok(commit) = repo.find_commit(id.detach()) {
                    c.parents = commit.parent_ids().map(|p| p.to_string()).collect();
                    c.subject = commit.message().map(|m| m.summary().to_string()).unwrap_or_default();
                    if let Ok(a) = commit.author() {
                        c.author_name = a.name.to_string();
                        c.author_email = a.email.to_string();
                    }
                }
            }
            heads.insert(name, c);
        }
        let prefix = format!("refs/remotes/{remote}/");
        let mut remote_refs = HashMap::new();
        for r in platform.prefixed(prefix.as_str())? {
            let Ok(r) = r else { continue };
            let Some(id) = r.try_id() else { continue };
            if let Some(b) = r.name().as_bstr().to_string().strip_prefix(&prefix) {
                remote_refs.insert(b.to_string(), id.to_string());
            }
        }
        let mut pulls = HashMap::new();
        for r in platform.prefixed("refs/gt/pull/")? {
            let Ok(r) = r else { continue };
            let Some(id) = r.try_id() else { continue };
            if let Some(n) = r.name().as_bstr().to_string().strip_prefix("refs/gt/pull/").and_then(|n| n.parse().ok()) {
                pulls.insert(n, id.to_string());
            }
        }
        Ok(Refs { heads, remote: remote_refs, pulls })
    }

    pub fn head(&self, branch: &str) -> Option<&str> {
        self.heads.get(branch).map(|c| c.sha.as_str())
    }
}

/// Every checkout's (git dir, worktree path): the main one, then each linked
/// worktree that still exists, read straight from `.git/worktrees`.
fn checkouts(common_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut v = vec![];
    if let Some(main) = common_dir.parent() {
        v.push((common_dir.to_path_buf(), main.to_path_buf()));
    }
    let Ok(entries) = std::fs::read_dir(common_dir.join("worktrees")) else { return v };
    for e in entries.flatten() {
        let dir = e.path();
        let wt = std::fs::read_to_string(dir.join("gitdir"))
            .ok()
            .and_then(|g| Path::new(g.trim()).parent().map(Path::to_path_buf));
        if let Some(wt) = wt.filter(|wt| wt.exists()) {
            v.push((dir, std::fs::canonicalize(&wt).unwrap_or(wt)));
        }
    }
    v
}

/// Map of branch name -> worktree path, read straight from `.git/worktrees`.
pub fn worktrees(common_dir: &Path) -> HashMap<String, PathBuf> {
    checkouts(common_dir)
        .into_iter()
        .filter_map(|(dir, wt)| {
            let head = std::fs::read_to_string(dir.join("HEAD")).ok()?;
            Some((head.trim().strip_prefix("ref: refs/heads/")?.to_string(), wt))
        })
        .collect()
}

/// A worktree `gt park` left on a trunk's tip with no branch checked out, so
/// it holds none. Marked by `gt-parked` in its git dir: the trunk and the commit.
#[derive(Clone, Debug)]
pub struct Parked {
    pub worktree: PathBuf,
    pub trunk: String,
    pub sha: String,
    /// HEAD is still detached but no longer where `gt park` or `gt sync` put it
    /// (a commit made while parked, or a checkout of some other commit).
    pub moved: bool,
}

const PARK_FILE: &str = "gt-parked";

/// The worktree's park mark, if its HEAD is still detached. One whose HEAD is
/// back on a branch is no longer parked, and its stale mark is removed.
fn read_parked(git_dir: &Path, worktree: PathBuf) -> Option<Parked> {
    let mark = std::fs::read_to_string(git_dir.join(PARK_FILE)).ok()?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if head.starts_with("ref:") {
        let _ = std::fs::remove_file(git_dir.join(PARK_FILE));
        return None;
    }
    let (trunk, sha) = mark.trim().split_once(' ')?;
    Some(Parked { worktree, trunk: trunk.to_string(), sha: sha.to_string(), moved: head != sha })
}

/// Every parked worktree, this one included.
pub fn parked(common_dir: &Path) -> Vec<Parked> {
    checkouts(common_dir).into_iter().filter_map(|(dir, wt)| read_parked(&dir, wt)).collect()
}

/// Drops the worktree's park mark, once it's back on a branch.
pub fn clear_parked(git_dir: &Path) {
    let _ = std::fs::remove_file(git_dir.join(PARK_FILE));
}

/// Marks the worktree with git dir `git_dir` as parked on `trunk` at `sha`.
pub fn write_parked(git_dir: &Path, trunk: &str, sha: &str) -> Result<()> {
    std::fs::write(git_dir.join(PARK_FILE), format!("{trunk} {sha}\n"))?;
    Ok(())
}

pub fn rev_parse(rev: &str) -> Option<String> {
    let out = run_raw(None, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")], None, &[]).ok()?;
    out.ok.then(|| out.stdout.trim().to_string())
}

pub fn is_ancestor(a: &str, b: &str) -> bool {
    run_raw(None, &["merge-base", "--is-ancestor", a, b], None, &[]).map(|o| o.ok).unwrap_or(false)
}

type Chains = HashMap<(String, Vec<String>), Vec<(String, Vec<String>)>>;
/// `own_chain` answers by shas. A fetch clears it: an exclude that was missing
/// (and so ignored) may exist now.
static CHAINS: std::sync::Mutex<Option<Chains>> = std::sync::Mutex::new(None);

/// Commits reachable from `tip` but from none of `excludes`, oldest first.
/// Excludes that no longer exist are ignored.
pub fn own_commits(tip: &str, excludes: &[String]) -> Result<Vec<String>> {
    Ok(own_chain(tip, excludes)?.into_iter().map(|(c, _)| c).collect())
}

/// `own_commits`, each with its parents. Asked again with the same shas (as
/// `gt log` does for a branch's row and then its size), it answers from memory:
/// commits never change, so the answer can't either.
pub fn own_chain(tip: &str, excludes: &[String]) -> Result<Vec<(String, Vec<String>)>> {
    let is_sha = |s: &str| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit());
    // A name like HEAD or a branch can move, so only shas are remembered.
    let key = (is_sha(tip) && excludes.iter().all(|e| e.is_empty() || is_sha(e)))
        .then(|| (tip.to_string(), excludes.to_vec()));
    if let Some(hit) = key.as_ref().and_then(|k| CHAINS.lock().unwrap().as_ref()?.get(k).cloned()) {
        return Ok(hit);
    }
    let mut args = vec![
        "rev-list".to_string(),
        "--reverse".into(),
        "--parents".into(),
        "--ignore-missing".into(),
        tip.to_string(),
    ];
    args.extend(excludes.iter().filter(|e| !e.is_empty()).map(|e| format!("^{e}")));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let chain: Vec<(String, Vec<String>)> = run(&args)?
        .lines()
        .filter_map(|l| {
            let mut f = l.split(' ').map(str::to_string);
            Some((f.next()?, f.collect()))
        })
        .collect();
    if let Some(k) = key {
        CHAINS.lock().unwrap().get_or_insert_default().insert(k, chain.clone());
    }
    Ok(chain)
}

/// Commits only on `a` and commits only on `b`.
pub fn ahead_behind(a: &str, b: &str) -> Option<(usize, usize)> {
    let out = run(&["rev-list", "--left-right", "--count", &format!("{a}...{b}")]).ok()?;
    let (l, r) = out.split_once(char::is_whitespace)?;
    Some((l.trim().parse().ok()?, r.trim().parse().ok()?))
}

/// When anything in this repo last ran `git fetch`: the newest FETCH_HEAD
/// across the main checkout and every worktree.
pub fn last_fetch(common_dir: &Path) -> Option<std::time::SystemTime> {
    let worktrees = std::fs::read_dir(common_dir.join("worktrees")).into_iter().flatten().flatten().map(|e| e.path());
    std::iter::once(common_dir.to_path_buf())
        .chain(worktrees)
        .filter_map(|d| std::fs::metadata(d.join("FETCH_HEAD")).and_then(|m| m.modified()).ok())
        .max()
}

pub fn merge_base(a: &str, b: &str) -> Option<String> {
    run(&["merge-base", a, b]).ok().filter(|s| !s.is_empty())
}

/// The best common ancestor of `tip` and a hypothetical merge of all of `others`.
pub fn merge_base_of(tip: &str, others: &[String]) -> Option<String> {
    let mut args = vec!["merge-base", tip];
    args.extend(others.iter().filter(|o| !o.is_empty()).map(String::as_str));
    if args.len() < 3 {
        return None;
    }
    run(&args).ok().filter(|s| !s.is_empty())
}

pub struct CommitMeta {
    pub author_name: String,
    pub author_email: String,
    pub author_date: String,
    pub message: String,
}

pub fn commit_meta(sha: &str) -> Result<CommitMeta> {
    let out = run(&["log", "-1", "--date=raw", "--format=%an%x00%ae%x00%ad%x00%B", sha])?;
    let mut f = out.splitn(4, '\0');
    Ok(CommitMeta {
        author_name: f.next().unwrap_or_default().to_string(),
        author_email: f.next().unwrap_or_default().to_string(),
        author_date: f.next().unwrap_or_default().to_string(),
        message: f.next().unwrap_or_default().to_string(),
    })
}

pub fn tree_of(rev: &str) -> Result<String> {
    run(&["rev-parse", &format!("{rev}^{{tree}}")])
}

pub enum Merge {
    Clean(String),
    Conflict(Vec<String>),
}

/// Three-way merge entirely in the object store: replays `base..theirs` onto `ours`.
pub fn merge_tree(base: &str, ours: &str, theirs: &str) -> Result<Merge> {
    let out = run_raw(
        None,
        &["merge-tree", "--write-tree", "--name-only", "--no-messages", &format!("--merge-base={base}"), ours, theirs],
        None,
        &[],
    )?;
    let mut lines = out.stdout.lines();
    let tree = lines.next().unwrap_or_default().to_string();
    match out.code {
        0 => Ok(Merge::Clean(tree)),
        1 => Ok(Merge::Conflict(lines.filter(|l| !l.is_empty()).map(str::to_string).collect())),
        _ => bail!("git merge-tree failed: {}", out.stderr.trim()),
    }
}

/// Write a commit object with `tree` on top of `parent`, keeping the author of `meta`.
pub fn commit_tree(tree: &str, parent: &str, meta: &CommitMeta) -> Result<String> {
    let out = run_raw(
        None,
        &["commit-tree", tree, "-p", parent, "-F", "-"],
        Some(&meta.message),
        &[
            ("GIT_AUTHOR_NAME", &meta.author_name),
            ("GIT_AUTHOR_EMAIL", &meta.author_email),
            ("GIT_AUTHOR_DATE", &meta.author_date),
        ],
    )?;
    if !out.ok {
        bail!("git commit-tree failed: {}", out.stderr.trim());
    }
    Ok(out.stdout.trim().to_string())
}

/// Write a commit object with `tree` on top of `parent`, authored by the user.
pub fn commit_tree_as_user(tree: &str, parent: &str, message: &str) -> Result<String> {
    let out = run_raw(None, &["commit-tree", tree, "-p", parent, "-F", "-"], Some(message), &[])?;
    if !out.ok {
        bail!("git commit-tree failed: {}", out.stderr.trim());
    }
    Ok(out.stdout.trim().to_string())
}

pub fn is_clean(dir: Option<&Path>) -> Result<bool> {
    Ok(run_in(dir, &["status", "--porcelain", "--untracked-files=no"])?.is_empty())
}

pub fn has_staged_changes() -> Result<bool> {
    Ok(!run_raw(None, &["diff", "--cached", "--quiet"], None, &[])?.ok)
}

/// Whether tracked files differ from HEAD, staged or not.
pub fn has_tracked_changes() -> Result<bool> {
    Ok(!run(&["status", "--porcelain", "--untracked-files=no"])?.trim().is_empty())
}

pub fn rebase_in_progress(git_dir: &Path) -> bool {
    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
}

/// Candidate URLs for a remote: as configured, then with `insteadOf` rewrites
/// applied. Read in-process rather than by spawning git twice.
pub fn remote_urls(remote: &str) -> Vec<String> {
    let Ok(repo) = open() else { return Vec::new() };
    let raw = repo.config_snapshot().string(format!("remote.{remote}.url").as_str()).map(|u| u.to_string());
    let expanded = repo.find_remote(remote).ok().and_then(|r| {
        Some(r.url(gix::remote::Direction::Fetch)?.to_bstring().to_string()).filter(|u| Some(u) != raw.as_ref())
    });
    raw.into_iter().chain(expanded).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_raw_reads_output_bigger_than_a_pipe_while_writing_stdin() {
        // Far more than the ~64 KB a pipe holds, both ways.
        let paths: String = (0..20_000).map(|i| format!("some/dir/file_{i}.rs\0")).collect();
        let out = run_raw(None, &["check-attr", "-z", "--stdin", "linguist-generated"], Some(&paths), &[]).unwrap();
        assert!(out.ok, "{}", out.stderr);
        assert!(out.stdout.len() > 256 * 1024);
    }
}
