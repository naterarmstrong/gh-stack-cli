//! Reads and writes `.git/gh-stack`, the same file `gh stack` uses, so both
//! tools can be used interchangeably. Unknown fields are preserved.

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
}

impl Stack {
    pub fn index_of(&self, branch: &str) -> Option<usize> {
        self.branches.iter().position(|b| b.branch == branch)
    }

    pub fn active(&self) -> impl Iterator<Item = (usize, &BranchRef)> {
        self.branches.iter().enumerate().filter(|(_, b)| !b.merged())
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

pub struct Store {
    pub path: PathBuf,
    pub file: StackFile,
    loaded: Option<Vec<u8>>,
}

impl Store {
    pub fn load(common_dir: &Path) -> Result<Store> {
        let path = common_dir.join("gh-stack");
        let (file, loaded) = match fs::read(&path) {
            Ok(data) => {
                let file: StackFile = serde_json::from_slice(&data).context("parsing .git/gh-stack")?;
                if file.schema_version > 1 {
                    bail!(".git/gh-stack has schema version {}; gt understands 1", file.schema_version);
                }
                (file, Some(data))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (StackFile { schema_version: 1, repository: String::new(), stacks: vec![], extra: Map::new() }, None)
            }
            Err(e) => return Err(e).context("reading .git/gh-stack"),
        };
        Ok(Store { path, file, loaded })
    }

    pub fn save(&mut self, _lock: &Lock) -> Result<()> {
        let current = fs::read(&self.path).ok();
        if current != self.loaded {
            bail!(".git/gh-stack changed on disk while gt was running; re-run the command");
        }
        let data = serde_json::to_vec_pretty(&self.file)?;
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
