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
    let mut child = c.spawn().context("spawning git")?;
    if let Some(input) = stdin {
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
    }
    let out = child.wait_with_output()?;
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
    Ok(cmd(dir, args).status().context("spawning git")?.success())
}

pub struct RepoInfo {
    pub common_dir: PathBuf,
    pub git_dir: PathBuf,
    pub toplevel: PathBuf,
    pub current_branch: Option<String>,
}

/// Opens the repository in-process (no git spawn).
pub fn open() -> Result<gix::Repository> {
    gix::discover(".").map_err(|_| anyhow::anyhow!("not inside a git repository"))
}

pub fn repo_info(repo: &gix::Repository) -> Result<RepoInfo> {
    let abs = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    Ok(RepoInfo {
        common_dir: abs(repo.common_dir()),
        git_dir: abs(repo.git_dir()),
        toplevel: abs(repo.workdir().ok_or_else(|| anyhow::anyhow!("bare repositories aren't supported"))?),
        current_branch: repo
            .head_name()?
            .and_then(|n| n.as_bstr().to_string().strip_prefix("refs/heads/").map(str::to_string)),
    })
}

#[derive(Clone, Debug)]
pub struct Commit {
    pub sha: String,
    /// `parents` and `subject` are only decoded for branches gt cares about
    /// (stacked branches and trunks); other branches carry just their sha.
    pub parents: Vec<String>,
    pub subject: String,
}

/// Snapshot of every local branch and remote-tracking ref, read in-process.
pub struct Refs {
    pub heads: HashMap<String, Commit>,
    pub remote: HashMap<String, String>,
}

impl Refs {
    pub fn load(repo: &gix::Repository, remote: &str, decode: &HashSet<String>) -> Result<Refs> {
        let platform = repo.references()?;
        let mut heads = HashMap::new();
        for r in platform.local_branches()? {
            let Ok(r) = r else { continue };
            let Some(id) = r.try_id() else { continue };
            let name = r.name().shorten().to_string();
            let mut c = Commit { sha: id.to_string(), parents: vec![], subject: String::new() };
            if decode.contains(&name) {
                if let Ok(commit) = repo.find_commit(id.detach()) {
                    c.parents = commit.parent_ids().map(|p| p.to_string()).collect();
                    c.subject = commit.message().map(|m| m.summary().to_string()).unwrap_or_default();
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
        Ok(Refs { heads, remote: remote_refs })
    }

    pub fn head(&self, branch: &str) -> Option<&str> {
        self.heads.get(branch).map(|c| c.sha.as_str())
    }
}

/// Map of branch name -> worktree path, read straight from `.git/worktrees`.
pub fn worktrees(common_dir: &Path) -> HashMap<String, PathBuf> {
    let branch_of = |head_file: PathBuf| {
        std::fs::read_to_string(head_file)
            .ok()
            .and_then(|h| h.trim().strip_prefix("ref: refs/heads/").map(str::to_string))
    };
    let mut map = HashMap::new();
    if let (Some(b), Some(main)) = (branch_of(common_dir.join("HEAD")), common_dir.parent()) {
        map.insert(b, main.to_path_buf());
    }
    let Ok(entries) = std::fs::read_dir(common_dir.join("worktrees")) else { return map };
    for e in entries.flatten() {
        let dir = e.path();
        let wt = std::fs::read_to_string(dir.join("gitdir"))
            .ok()
            .and_then(|g| Path::new(g.trim()).parent().map(Path::to_path_buf));
        if let (Some(b), Some(wt)) = (branch_of(dir.join("HEAD")), wt) {
            if wt.exists() {
                map.insert(b, std::fs::canonicalize(&wt).unwrap_or(wt));
            }
        }
    }
    map
}

pub fn rev_parse(rev: &str) -> Option<String> {
    let out = run_raw(None, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")], None, &[]).ok()?;
    out.ok.then(|| out.stdout.trim().to_string())
}

pub fn is_ancestor(a: &str, b: &str) -> bool {
    run_raw(None, &["merge-base", "--is-ancestor", a, b], None, &[]).map(|o| o.ok).unwrap_or(false)
}

/// Commits reachable from `tip` but from none of `excludes`, oldest first.
/// Excludes that no longer exist are ignored.
pub fn own_commits(tip: &str, excludes: &[String]) -> Result<Vec<String>> {
    let mut args = vec!["rev-list".to_string(), "--reverse".into(), "--ignore-missing".into(), tip.to_string()];
    args.extend(excludes.iter().filter(|e| !e.is_empty()).map(|e| format!("^{e}")));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(run(&args)?.lines().map(str::to_string).collect())
}

pub fn merge_base(a: &str, b: &str) -> Option<String> {
    run(&["merge-base", a, b]).ok().filter(|s| !s.is_empty())
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

pub fn is_clean(dir: Option<&Path>) -> Result<bool> {
    Ok(run_in(dir, &["status", "--porcelain", "--untracked-files=no"])?.is_empty())
}

pub fn has_staged_changes() -> Result<bool> {
    Ok(!run_raw(None, &["diff", "--cached", "--quiet"], None, &[])?.ok)
}

pub fn rebase_in_progress(git_dir: &Path) -> bool {
    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
}

pub fn remote_url(remote: &str) -> Result<String> {
    run(&["remote", "get-url", remote])
}
