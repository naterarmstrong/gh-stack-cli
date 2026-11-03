//! Test harness: runs the real `gt` binary against throwaway repositories.
//!
//! Each `World` is a bare `origin`, a clone of it to work in, and a home
//! directory with its own git config, all under one temp dir. The clone's
//! remote URL is a GitHub URL (`insteadOf` maps it back to the bare repo), so
//! gt derives owner and repo from it while every fetch and push stays local.
//! With `World::with_github()` a fake GitHub (see `fake_github.rs`) answers
//! gt's API calls; without it the API URL points at a closed port.

#![allow(dead_code)]

pub mod fake_github;

use fake_github::FakeGitHub;
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const OWNER: &str = "acme";
pub const REPO: &str = "widgets";

pub fn remote_url() -> String {
    format!("https://github.com/{OWNER}/{REPO}.git")
}

/// A directory removed on drop (kept when GT_TEST_KEEP is set, for debugging).
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("gt-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        // gt canonicalizes paths (macOS /var → /private/var), so compare like with like.
        TempDir(std::fs::canonicalize(&p).unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::env::var_os("GT_TEST_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// A finished command's exit status and output.
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == 0
    }

    /// stdout and stderr together, for assertions that don't care which.
    pub fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

impl std::fmt::Debug for Out {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit {}\n--- stdout ---\n{}\n--- stderr ---\n{}", self.code, self.stdout, self.stderr)
    }
}

pub struct World {
    pub tmp: TempDir,
    pub origin: PathBuf,
    pub repo: PathBuf,
    pub home: PathBuf,
    pub gh: Option<FakeGitHub>,
}

impl World {
    /// No GitHub: API calls fail fast, as if offline.
    pub fn new() -> World {
        World::build(false)
    }

    pub fn with_github() -> World {
        World::build(true)
    }

    fn build(github: bool) -> World {
        let tmp = TempDir::new();
        let root = tmp.path().to_path_buf();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".gitconfig"),
            "[user]\n\tname = Test User\n\temail = test@example.com\n\
             [init]\n\tdefaultBranch = main\n\
             [advice]\n\tdetachedHead = false\n\
             [commit]\n\tgpgsign = false\n",
        )
        .unwrap();
        let origin = root.join("origin.git");
        let repo = root.join("repo");
        let mut w = World { tmp, origin: origin.clone(), repo: repo.clone(), home, gh: None };

        w.git_in(&root, &["init", "--quiet", "--bare", "-b", "main", origin.to_str().unwrap()]);
        w.git_in(&root, &["clone", "--quiet", origin.to_str().unwrap(), repo.to_str().unwrap()]);
        w.git(&["config", "remote.origin.url", &remote_url()]);
        w.git(&["config", &format!("url.{}.insteadOf", origin.display()), &remote_url()]);
        w.write("README.md", "# widgets\n");
        w.write("src/lib.rs", "pub fn a() {}\n");
        w.git(&["add", "-A"]);
        w.git(&["commit", "--quiet", "-m", "Initial commit"]);
        w.git(&["push", "--quiet", "-u", "origin", "main"]);
        w.git(&["remote", "set-head", "origin", "main"]);
        if github {
            w.gh = Some(FakeGitHub::start(&origin));
        }
        w
    }

    pub fn gh(&self) -> &FakeGitHub {
        self.gh.as_ref().expect("World::with_github() to use the fake GitHub")
    }

    fn env(&self, c: &mut Command) {
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("GIT_CONFIG_GLOBAL", self.home.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_EDITOR", "true")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .env("NO_COLOR", "1")
            .env("LANG", "C")
            .env("GH_TOKEN", "test-token")
            .env(
                "GT_GITHUB_API",
                match &self.gh {
                    Some(gh) => gh.url.clone(),
                    // Nothing listens on the discard port: connections are refused at once.
                    None => "http://127.0.0.1:9".into(),
                },
            );
        // No controlling terminal, so a prompt (picker or confirm) fails instead of hanging.
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    pub fn gt_in(&self, dir: &Path, args: &[&str]) -> Out {
        let mut c = Command::new(env!("CARGO_BIN_EXE_gt"));
        c.current_dir(dir).args(args);
        self.env(&mut c);
        let out = c.output().expect("running gt");
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Runs gt in the clone with a pseudo-terminal as its controlling terminal,
    /// typing each of `answers` (plus Enter) as a `[y/N]` prompt shows up.
    /// Returns what gt wrote to the terminal (prompts, and the echo of what was
    /// typed) beside its output. A prompt with no answer left fails the test.
    pub fn gt_with_tty(&self, args: &[&str], answers: &[&str]) -> (Out, String) {
        use std::io::{Read, Write};
        use std::os::fd::FromRawFd;
        use std::sync::{Arc, Mutex};
        let (mut master, mut slave) = (0, 0);
        let rc = unsafe {
            libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(rc, 0, "openpty failed");
        // Keep both ends out of every other process spawned meanwhile; gt only
        // needs the terminal to be its controlling one, set up before exec.
        unsafe {
            libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        let slave_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(slave) };
        let mut tty = unsafe { std::fs::File::from_raw_fd(master) };

        // Reads the terminal, answering each prompt as it appears.
        let screen = Arc::new(Mutex::new(Vec::<u8>::new()));
        let answers: Vec<String> = answers.iter().map(|a| format!("{a}\n")).collect();
        let reader = {
            let screen = screen.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                let mut answered = 0;
                // Ends once gt exits and the last handle on the terminal closes.
                while let Ok(n @ 1..) = tty.read(&mut buf) {
                    let mut s = screen.lock().unwrap();
                    s.extend_from_slice(&buf[..n]);
                    let prompts = String::from_utf8_lossy(&s).matches("[y/N]").count();
                    while answered < prompts.min(answers.len()) {
                        let _ = tty.write_all(answers[answered].as_bytes());
                        answered += 1;
                    }
                }
            })
        };

        let mut c = Command::new(env!("CARGO_BIN_EXE_gt"));
        c.current_dir(&self.repo).args(args);
        self.env(&mut c);
        // After env()'s setsid, the pty becomes the new session's terminal.
        unsafe {
            c.pre_exec(move || {
                if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        c.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        let child = c.spawn().expect("running gt");
        let pid = child.id() as i32;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(child.wait_with_output()));
        // A prompt nothing answers would wait forever; fail instead.
        let out = match rx.recv_timeout(std::time::Duration::from_secs(60)) {
            Ok(out) => out.expect("running gt"),
            Err(_) => {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                drop(slave_fd);
                let s = String::from_utf8_lossy(&screen.lock().unwrap()).into_owned();
                panic!("gt {} hung; terminal:\n{s}", args.join(" "));
            }
        };
        // The reader stops once nothing holds the terminal open.
        drop(slave_fd);
        let _ = reader.join();
        let out = Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        };
        let screen = String::from_utf8_lossy(&screen.lock().unwrap()).into_owned();
        (out, screen)
    }

    /// Runs gt in the clone, whatever the outcome.
    pub fn gt(&self, args: &[&str]) -> Out {
        self.gt_in(&self.repo, args)
    }

    /// Runs gt in the clone and fails the test unless it succeeds.
    #[track_caller]
    pub fn gt_ok(&self, args: &[&str]) -> Out {
        let out = self.gt(args);
        assert!(out.ok(), "gt {} failed:\n{out:?}", args.join(" "));
        out
    }

    /// Runs gt in the clone and fails the test if it succeeds.
    #[track_caller]
    pub fn gt_err(&self, args: &[&str]) -> Out {
        let out = self.gt(args);
        assert!(!out.ok(), "gt {} should have failed:\n{out:?}", args.join(" "));
        out
    }

    pub fn git_raw(&self, dir: &Path, args: &[&str]) -> Out {
        let mut c = Command::new("git");
        c.current_dir(dir).args(args);
        self.env(&mut c);
        let out = c.output().expect("running git");
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    #[track_caller]
    pub fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.git_raw(dir, args);
        assert!(out.ok(), "git {} failed:\n{out:?}", args.join(" "));
        out.stdout.trim_end().to_string()
    }

    #[track_caller]
    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.repo, args)
    }

    pub fn write(&self, path: &str, content: &str) {
        write_in(&self.repo, path, content)
    }

    pub fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.repo.join(path)).unwrap()
    }

    /// Writes a file and commits it on the current branch.
    pub fn commit(&self, path: &str, content: &str, msg: &str) -> String {
        self.write(path, content);
        self.git(&["add", "-A"]);
        self.git(&["commit", "--quiet", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// Writes a file and runs `gt create <name> -am <msg>`.
    #[track_caller]
    pub fn create(&self, name: &str, path: &str, content: &str) {
        self.write(path, content);
        self.gt_ok(&["create", name, "-am", &format!("Add {path}")]);
    }

    pub fn current(&self) -> String {
        self.git(&["branch", "--show-current"])
    }

    /// A local branch's tip, or None when it doesn't exist.
    pub fn head(&self, branch: &str) -> Option<String> {
        let out = self.git_raw(&self.repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]);
        out.ok().then(|| out.stdout.trim().to_string())
    }

    /// A branch's tip on the bare origin.
    pub fn remote_head(&self, branch: &str) -> Option<String> {
        let out = self.git_raw(&self.origin, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]);
        out.ok().then(|| out.stdout.trim().to_string())
    }

    pub fn parent(&self, rev: &str) -> String {
        self.git(&["rev-parse", &format!("{rev}^")])
    }

    /// A file's content at a revision.
    pub fn show(&self, rev: &str, path: &str) -> Option<String> {
        let out = self.git_raw(&self.repo, &["show", &format!("{rev}:{path}")]);
        out.ok().then_some(out.stdout)
    }

    /// The number of commits in `from..to`.
    pub fn count(&self, range: &str) -> usize {
        self.git(&["rev-list", "--count", range]).parse().unwrap()
    }

    pub fn config(&self, key: &str) -> Option<String> {
        let out = self.git_raw(&self.repo, &["config", "--get", key]);
        out.ok().then(|| out.stdout.trim().to_string())
    }

    pub fn common_dir(&self) -> PathBuf {
        let d = self.git(&["rev-parse", "--git-common-dir"]);
        std::fs::canonicalize(self.repo.join(d)).unwrap()
    }

    /// `.git/gh-stack`: what gh-stack sees.
    pub fn state_path(&self) -> PathBuf {
        self.common_dir().join("gh-stack")
    }

    /// `.git/gt-stack`: gt's local-only branches.
    pub fn local_state_path(&self) -> PathBuf {
        self.common_dir().join("gt-stack")
    }

    /// `.git/gh-stack` as JSON.
    pub fn state(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.state_path()).expect("no .git/gh-stack")).unwrap()
    }

    /// `.git/gt-stack` as JSON.
    pub fn local_state(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.local_state_path()).expect("no .git/gt-stack")).unwrap()
    }

    pub fn write_state(&self, v: &Value) {
        std::fs::write(self.state_path(), serde_json::to_vec_pretty(v).unwrap()).unwrap();
    }

    pub fn write_local_state(&self, v: &Value) {
        std::fs::write(self.local_state_path(), serde_json::to_vec_pretty(v).unwrap()).unwrap();
    }

    fn read_json(path: &Path) -> Value {
        std::fs::read(path).map(|d| serde_json::from_slice(&d).unwrap()).unwrap_or(Value::Null)
    }

    /// Branch names of each stack in one state file, bottom first.
    fn names(v: &Value) -> Vec<Vec<String>> {
        let branches = |st: &Value| -> Vec<String> {
            st["branches"].as_array().unwrap().iter().map(|b| b["branch"].as_str().unwrap().to_string()).collect()
        };
        v["stacks"].as_array().map(|s| s.iter().map(branches).collect()).unwrap_or_default()
    }

    /// Branch names of each gh-stack stack.
    pub fn shared_stacks(&self) -> Vec<Vec<String>> {
        World::names(&World::read_json(&self.state_path()))
    }

    /// Branch names of each gt-stack entry (local branches, on a trunk or on top of a gh-stack stack).
    pub fn local_stacks(&self) -> Vec<Vec<String>> {
        World::names(&World::read_json(&self.local_state_path()))
    }

    /// Branch names of each stack as gt sees it, both files merged: gh-stack's
    /// stacks first, with local branches on top, then local-only stacks.
    pub fn stacks(&self) -> Vec<Vec<String>> {
        let gh = World::read_json(&self.state_path());
        let local = World::read_json(&self.local_state_path());
        let mut stacks = World::names(&gh);
        let empty = vec![];
        for (ls, names) in local["stacks"].as_array().unwrap_or(&empty).iter().zip(World::names(&local)) {
            let on = ls["on"].as_str().unwrap_or_default();
            match stacks.iter_mut().find(|s| !on.is_empty() && s.iter().any(|b| b == on)) {
                Some(s) => s.extend(names),
                None => stacks.push(names),
            }
        }
        stacks
    }

    /// One stacked branch's entry, from whichever state file holds it.
    pub fn branch_state(&self, branch: &str) -> Value {
        for path in [self.state_path(), self.local_state_path()] {
            let s = World::read_json(&path);
            for st in s["stacks"].as_array().into_iter().flatten() {
                for b in st["branches"].as_array().unwrap() {
                    if b["branch"] == branch {
                        return b.clone();
                    }
                }
            }
        }
        panic!("{branch} isn't in .git/gh-stack or .git/gt-stack");
    }

    /// Edits a stacked branch's entry in place, in whichever state file holds it.
    pub fn edit_branch_state(&self, branch: &str, f: impl FnOnce(&mut Value)) {
        for path in [self.state_path(), self.local_state_path()] {
            let mut s = World::read_json(&path);
            let found = s["stacks"]
                .as_array_mut()
                .into_iter()
                .flatten()
                .flat_map(|st| st["branches"].as_array_mut().unwrap().iter_mut())
                .find(|b| b["branch"] == branch);
            if let Some(b) = found {
                f(b);
                std::fs::write(&path, serde_json::to_vec_pretty(&s).unwrap()).unwrap();
                return;
            }
        }
        panic!("{branch} isn't in .git/gh-stack or .git/gt-stack");
    }

    /// A second clone standing in for a teammate or bot pushing to origin.
    pub fn teammate(&self) -> PathBuf {
        let dir = self.tmp.path().join("teammate");
        if !dir.exists() {
            self.git_in(self.tmp.path(), &["clone", "--quiet", self.origin.to_str().unwrap(), dir.to_str().unwrap()]);
            self.git_in(&dir, &["config", "user.name", "Teammate"]);
            self.git_in(&dir, &["config", "user.email", "mate@example.com"]);
        } else {
            self.git_in(&dir, &["fetch", "--quiet", "--prune", "origin"]);
        }
        dir
    }

    /// Commits to `branch` from the teammate clone and pushes it. With `amend`
    /// the branch's commit is amended and force-pushed instead.
    pub fn teammate_push(&self, branch: &str, path: &str, content: &str, amend: bool) -> String {
        let dir = self.teammate();
        self.git_in(&dir, &["checkout", "--quiet", "-B", branch, &format!("origin/{branch}")]);
        write_in(&dir, path, content);
        self.git_in(&dir, &["add", "-A"]);
        if amend {
            self.git_in(&dir, &["commit", "--quiet", "--amend", "--no-edit"]);
        } else {
            self.git_in(&dir, &["commit", "--quiet", "-m", &format!("Teammate edits {path}")]);
        }
        self.git_in(&dir, &["push", "--quiet", "--force", "origin", branch]);
        self.git_in(&dir, &["rev-parse", "HEAD"])
    }

    /// Lands a commit on origin's main from the teammate clone.
    pub fn advance_main(&self, path: &str, content: &str) -> String {
        let dir = self.teammate();
        self.git_in(&dir, &["checkout", "--quiet", "-B", "main", "origin/main"]);
        write_in(&dir, path, content);
        self.git_in(&dir, &["add", "-A"]);
        self.git_in(&dir, &["commit", "--quiet", "-m", &format!("Land {path} on main")]);
        self.git_in(&dir, &["push", "--quiet", "origin", "main"]);
        self.git_in(&dir, &["rev-parse", "HEAD"])
    }

    /// Squash-merges PR `n` into origin's main the way GitHub does, and marks it merged.
    pub fn merge_pr(&self, n: u64) -> String {
        let pr = self.gh().pr(n);
        let dir = self.teammate();
        self.git_in(&dir, &["checkout", "--quiet", "-B", "main", "origin/main"]);
        self.git_in(&dir, &["merge", "--quiet", "--squash", &format!("origin/{}", pr.head)]);
        self.git_in(&dir, &["commit", "--quiet", "-m", &format!("{} (#{n})", pr.title)]);
        self.git_in(&dir, &["push", "--quiet", "origin", "main"]);
        self.gh().merge(n);
        self.git_in(&dir, &["rev-parse", "HEAD"])
    }
}

pub fn write_in(dir: &Path, path: &str, content: &str) {
    let p = dir.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

/// The lines of `gt ls` output that mention a branch, trimmed.
pub fn line_of<'a>(out: &'a str, branch: &str) -> &'a str {
    out.lines()
        .find(|l| l.split_whitespace().any(|w| w == branch))
        .unwrap_or_else(|| panic!("no line for {branch} in:\n{out}"))
        .trim()
}
