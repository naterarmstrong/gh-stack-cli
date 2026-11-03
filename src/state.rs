//! Reads and writes `.git/gh-stack`, the same file `gh stack` uses, so both
//! tools can be used interchangeably. Unknown fields are preserved.
//!
//! Branches that aren't shared with GitHub yet live in gt's own
//! `.git/gt-stack` instead, so `gh stack submit` never opens PRs for them.
//! Each of its stacks is either a whole local stack on a trunk, or local
//! branches on top of a gh-stack stack (`on` names the branch they sit on).
//! Both files load into one list of stacks, with each local branch marked
//! `local`, and are split back apart on save.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct PrRef {
    pub number: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub merged: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct BranchRef {
    pub branch: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub head: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PrRef>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
    /// Kept in `.git/gt-stack` rather than `.git/gh-stack`: not submitted yet.
    #[serde(skip)]
    pub local: bool,
}

impl BranchRef {
    pub fn new(branch: &str) -> Self {
        BranchRef { branch: branch.to_string(), ..Default::default() }
    }

    pub fn merged(&self) -> bool {
        self.pull_request.as_ref().is_some_and(|p| p.merged)
    }

    pub fn pr_number(&self) -> Option<u64> {
        self.pull_request.as_ref().map(|p| p.number)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Stack {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub number: u64,
    pub trunk: BranchRef,
    #[serde(default)]
    pub branches: Vec<BranchRef>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
    /// Started in gt (`.git/gt-stack`) rather than gh-stack. Such a stack moves
    /// into `.git/gh-stack` once any of its branches is shared.
    #[serde(skip)]
    pub local: bool,
}

impl Stack {
    pub fn index_of(&self, branch: &str) -> Option<usize> {
        self.branches.iter().position(|b| b.branch == branch)
    }

    pub fn active(&self) -> impl Iterator<Item = (usize, &BranchRef)> {
        self.branches.iter().enumerate().filter(|(_, b)| !b.merged())
    }

    /// Unmerged branches shared with gh-stack, which get PRs and join the GitHub stack.
    pub fn shared(&self) -> impl Iterator<Item = (usize, &BranchRef)> {
        self.active().filter(|(_, b)| !b.local)
    }

    /// The branch an unmerged branch at `idx` should sit on.
    pub fn parent_of(&self, idx: usize) -> &str {
        self.branches[..idx].iter().rev().find(|b| !b.merged()).map(|b| b.branch.as_str()).unwrap_or(&self.trunk.branch)
    }

    pub fn fully_merged(&self) -> bool {
        !self.branches.is_empty() && self.branches.iter().all(|b| b.merged())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StackFile {
    pub schema_version: u32,
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub stacks: Vec<Stack>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl StackFile {
    pub fn find(&self, branch: &str) -> Option<usize> {
        // A trunk is shared by every stack, so only match stack branches.
        self.stacks.iter().position(|s| s.index_of(branch).is_some())
    }

    pub fn find_pr(&self, pr: u64) -> Option<(usize, usize)> {
        self.stacks
            .iter()
            .enumerate()
            .find_map(|(si, s)| s.branches.iter().position(|b| b.pr_number() == Some(pr)).map(|bi| (si, bi)))
    }
}

/// Holds gh-stack's flock files for the duration of a mutating command.
pub struct Lock {
    _files: Vec<File>,
}

fn flock(path: &Path, what: &str) -> Result<File> {
    let f = OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    // SAFETY: flock on a valid, owned fd.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!("another gh-stack/gt {what} is in progress ({} is locked)", path.display());
    }
    Ok(f)
}

impl Lock {
    pub fn acquire(common_dir: &Path) -> Result<Lock> {
        Ok(Lock {
            _files: vec![
                flock(&common_dir.join("gh-stack-operation.lock"), "operation")?,
                flock(&common_dir.join("gh-stack.lock"), "write")?,
            ],
        })
    }
}

/// `.git/gt-stack`: gt's stacks of branches that aren't shared with gh-stack.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct LocalFile {
    schema_version: u32,
    #[serde(default)]
    stacks: Vec<LocalStack>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct LocalStack {
    trunk: BranchRef,
    /// The gh-stack branch these sit on, or empty for a stack of its own on `trunk`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    on: String,
    branches: Vec<BranchRef>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// One JSON state file and the bytes it held when loaded, to catch concurrent writers.
struct Disk {
    path: PathBuf,
    loaded: Option<Vec<u8>>,
}

impl Disk {
    fn read<T: serde::de::DeserializeOwned>(path: PathBuf) -> Result<(Disk, Option<T>)> {
        let name = format!(".git/{}", path.file_name().unwrap_or_default().to_string_lossy());
        match fs::read(&path) {
            Ok(data) => {
                let v = serde_json::from_slice(&data).with_context(|| format!("parsing {name}"))?;
                Ok((Disk { path, loaded: Some(data) }, Some(v)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Disk { path, loaded: None }, None)),
            Err(e) => Err(e).with_context(|| format!("reading {name}")),
        }
    }

    fn check(&self) -> Result<()> {
        if fs::read(&self.path).ok() != self.loaded {
            let name = self.path.file_name().unwrap_or_default().to_string_lossy();
            bail!(".git/{name} changed on disk while gt was running; re-run the command");
        }
        Ok(())
    }

    /// Writes `v` unless the file would be unchanged, or would be created with nothing in it.
    fn write<T: Serialize>(&mut self, v: &T, empty: bool) -> Result<()> {
        if self.loaded.is_none() && empty {
            return Ok(());
        }
        let data = serde_json::to_vec_pretty(v)?;
        if Some(&data) == self.loaded.as_ref() {
            return Ok(());
        }
        let tmp = self.path.with_extension("gt.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&data)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        self.loaded = Some(data);
        Ok(())
    }
}

pub struct Store {
    /// Every stack, gh-stack's first, with gt's local branches merged in.
    pub file: StackFile,
    gh: Disk,
    local: Disk,
    local_extra: Map<String, Value>,
}

impl Store {
    pub fn load(common_dir: &Path) -> Result<Store> {
        let (gh, file) = Disk::read::<StackFile>(common_dir.join("gh-stack"))?;
        let mut file = file.unwrap_or(StackFile {
            schema_version: 1,
            repository: String::new(),
            stacks: vec![],
            extra: Map::new(),
        });
        if file.schema_version > 1 {
            bail!(".git/gh-stack has schema version {}; gt understands 1", file.schema_version);
        }
        let (local, mine) = Disk::read::<LocalFile>(common_dir.join("gt-stack"))?;
        let mine = mine.unwrap_or(LocalFile { schema_version: 1, stacks: vec![], extra: Map::new() });
        if mine.schema_version > 1 {
            bail!(".git/gt-stack has schema version {}; gt understands 1", mine.schema_version);
        }
        for ls in mine.stacks {
            // A branch gh-stack also tracks now (say, `gh stack add`) belongs to gh-stack.
            let branches: Vec<BranchRef> = ls
                .branches
                .into_iter()
                .filter(|b| file.find(&b.branch).is_none())
                .map(|b| BranchRef { local: true, ..b })
                .collect();
            match file.find(&ls.on).filter(|_| !ls.on.is_empty()) {
                // If gh-stack grew above `on` meanwhile, the local branches move up on top of it.
                Some(si) => file.stacks[si].branches.extend(branches),
                None if branches.is_empty() => {}
                None => file.stacks.push(Stack {
                    trunk: ls.trunk,
                    branches,
                    extra: ls.extra,
                    local: true,
                    ..Default::default()
                }),
            }
        }
        Ok(Store { file, gh, local, local_extra: mine.extra })
    }

    pub fn save(&mut self, _lock: &Lock) -> Result<()> {
        self.gh.check()?;
        self.local.check()?;
        let mut gh = StackFile { stacks: vec![], ..self.file.clone() };
        let mut mine = LocalFile { schema_version: 1, stacks: vec![], extra: self.local_extra.clone() };
        for s in &self.file.stacks {
            // gh-stack can't hold a branch above one it doesn't know, so everything at or
            // below the highest shared branch is shared.
            let split = s.branches.iter().rposition(|b| !b.local).map_or(0, |i| i + 1);
            let (shared, local) = s.branches.split_at(split);
            if !shared.is_empty() || !s.local {
                gh.stacks.push(Stack { branches: shared.to_vec(), ..s.clone() });
            }
            if !local.is_empty() {
                mine.stacks.push(LocalStack {
                    trunk: s.trunk.clone(),
                    on: shared.last().map(|b| b.branch.clone()).unwrap_or_default(),
                    branches: local.to_vec(),
                    extra: if shared.is_empty() { s.extra.clone() } else { Map::new() },
                });
            }
        }
        let empty = gh.stacks.is_empty();
        self.gh.write(&gh, empty)?;
        let empty = mine.stacks.is_empty();
        self.local.write(&mine, empty)
    }
}
