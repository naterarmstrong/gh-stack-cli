//! Line counts of one commit's diff against its parent, split into src, test,
//! doc and generated code, per file. With one commit per branch that's the
//! branch's PR, and since a commit's diff never changes the result is cached
//! forever under `<common-dir>/gt/loc-v<N>/<sha>.json`, shared by every worktree.
//!
//! Files are classified by path (and `.gitattributes` linguist attributes and a
//! "generated" header); inside Rust source files, `#[cfg(test)]` items count as
//! test. That last part assumes rustfmt'd code: a `#[cfg(test)]` at column 0
//! runs to the next `}` at column 0 (or to its `;` for a one-line item).

use crate::git;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

/// Bump when classification rules change so old results are recomputed.
const RULES_VERSION: u32 = 1;
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Src,
    Test,
    Doc,
    Gen,
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug)]
pub struct Lines {
    pub add: u32,
    pub del: u32,
}

impl Lines {
    pub fn is_empty(&self) -> bool {
        self.add == 0 && self.del == 0
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug)]
pub struct Split {
    pub src: Lines,
    pub test: Lines,
    pub doc: Lines,
    #[serde(rename = "gen")]
    pub generated: Lines,
}

impl Split {
    fn get(&mut self, k: Kind) -> &mut Lines {
        match k {
            Kind::Src => &mut self.src,
            Kind::Test => &mut self.test,
            Kind::Doc => &mut self.doc,
            Kind::Gen => &mut self.generated,
        }
    }

    fn add(&mut self, o: &Split) {
        for (k, l) in o.iter() {
            let s = self.get(k);
            s.add += l.add;
            s.del += l.del;
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (Kind, Lines)> {
        [(Kind::Src, self.src), (Kind::Test, self.test), (Kind::Doc, self.doc), (Kind::Gen, self.generated)].into_iter()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileStat {
    pub path: String,
    /// Where a renamed or copied file came from.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub old_path: Option<String>,
    /// git's status letter: A, M, D, R, C or T.
    pub status: String,
    /// The file's category by path. A Rust source file is `src` even when some
    /// of its lines are counted as test.
    pub kind: Kind,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub binary: bool,
    pub lines: Split,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Diff {
    pub commit: String,
    /// What the commit was diffed against: its first parent (or the empty
    /// tree), or for a branch with several commits, where it forked from its parent.
    pub base: String,
    pub total: Split,
    pub files: Vec<FileStat>,
}

impl Diff {
    pub fn empty(commit: &str) -> Diff {
        Diff { commit: commit.into(), base: commit.into(), total: Split::default(), files: vec![] }
    }
}

fn cache_dir(common_dir: &Path) -> PathBuf {
    common_dir.join("gt").join(format!("loc-v{RULES_VERSION}"))
}

/// The diff of `commit` against `base` (default: its first parent), from the
/// cache when possible.
pub fn of_commit(common_dir: &Path, commit: &str, base: Option<&str>) -> Result<Diff> {
    let key = match base {
        Some(b) => format!("{commit}-{b}"),
        None => commit.to_string(),
    };
    let path = cache_dir(common_dir).join(format!("{key}.json"));
    if let Some(d) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Diff>(&b).ok()) {
        return Ok(d);
    }
    let d = compute(commit, base)?;
    // Best effort: write to a temp file and rename so readers never see half a file.
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_vec(&d)?).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    Ok(d)
}

struct RawEntry {
    old_blob: String,
    new_blob: String,
    status: String,
    path: String,
    old_path: Option<String>,
}

fn compute(commit: &str, base: Option<&str>) -> Result<Diff> {
    let base = match base {
        Some(b) => b.to_string(),
        None => {
            let parent = git::run_raw(None, &["rev-parse", "--verify", "--quiet", &format!("{commit}^")], None, &[])?;
            if parent.ok { parent.stdout.trim().to_string() } else { EMPTY_TREE.to_string() }
        }
    };
    let out = git::run_raw(
        None,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--raw",
            "-p",
            "-U0",
            "-M",
            "--no-abbrev",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-relative",
            &base,
            commit,
        ],
        None,
        &[],
    )?;
    if !out.ok {
        bail!("git diff {commit} failed: {}", out.stderr.trim());
    }

    // `--raw -p` prints every raw line first, then one patch per file in the same order.
    let mut raw = vec![];
    let mut patches: Vec<Vec<&str>> = vec![];
    for line in out.stdout.lines() {
        if let Some(r) = line.strip_prefix(':') {
            raw.push(parse_raw(r).with_context(|| format!("unexpected diff line: {line}"))?);
        } else if line.starts_with("diff --git ") || line.starts_with("diff --cc ") {
            patches.push(vec![]);
        } else if let Some(p) = patches.last_mut() {
            p.push(line);
        }
    }
    if raw.len() != patches.len() {
        bail!("couldn't match up git diff output for {commit} ({} files, {} patches)", raw.len(), patches.len());
    }

    let repo = git::open()?;
    let attrs = linguist_attrs(commit, raw.iter().map(|r| r.path.as_str()));
    let mut files = vec![];
    let mut total = Split::default();
    for (r, patch) in raw.into_iter().zip(patches) {
        let blob = |oid: &str| -> Option<Vec<u8>> {
            if oid.bytes().all(|b| b == b'0') {
                return None;
            }
            let id = gix::ObjectId::from_hex(oid.as_bytes()).ok()?;
            repo.find_object(id).ok().map(|o| o.data.clone())
        };
        let newest = if r.status == "D" { &r.old_blob } else { &r.new_blob };
        let kind = classify(&r.path, attrs.get(&r.path).copied(), || blob(newest));
        let binary = patch.iter().any(|l| l.starts_with("Binary files ") || *l == "GIT binary patch");

        // Removed lines are classified with the old file's test regions, added with the new one's.
        let (old_tests, new_tests) = if kind == Kind::Src && r.path.ends_with(".rs") && !binary {
            let ranges = |oid: &str| blob(oid).map(|b| rust_test_ranges(&String::from_utf8_lossy(&b))).unwrap_or_default();
            (ranges(&r.old_blob), ranges(&r.new_blob))
        } else {
            (vec![], vec![])
        };
        let which = |line: u32, tests: &[RangeInclusive<u32>]| {
            if tests.iter().any(|t| t.contains(&line)) { Kind::Test } else { kind }
        };

        let mut lines = Split::default();
        for l in &patch {
            let Some((old, new)) = parse_hunk(l) else { continue };
            for n in old {
                lines.get(which(n, &old_tests)).del += 1;
            }
            for n in new {
                lines.get(which(n, &new_tests)).add += 1;
            }
        }
        total.add(&lines);
        files.push(FileStat { path: r.path, old_path: r.old_path, status: r.status, kind, binary, lines });
    }
    Ok(Diff { commit: commit.to_string(), base, total, files })
}

/// `100644 100644 <old> <new> M\tpath` or `... R087\told\tnew`.
fn parse_raw(r: &str) -> Option<RawEntry> {
    let (meta, paths) = r.split_once('\t')?;
    let f: Vec<&str> = meta.split(' ').collect();
    let (old_blob, new_blob, status) = (f.get(2)?, f.get(3)?, f.get(4)?);
    let status: String = status.chars().take(1).collect();
    let (path, old_path) = match paths.split_once('\t') {
        Some((old, new)) if matches!(status.as_str(), "R" | "C") => (unquote(new), Some(unquote(old))),
        _ => (unquote(paths), None),
    };
    Some(RawEntry { old_blob: old_blob.to_string(), new_blob: new_blob.to_string(), status, path, old_path })
}

/// git C-quotes paths with unusual characters even with core.quotePath off.
fn unquote(p: &str) -> String {
    let Some(inner) = p.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else { return p.to_string() };
    let mut out = vec![];
    let mut it = inner.bytes().peekable();
    while let Some(c) = it.next() {
        if c != b'\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b't') => out.push(b'\t'),
            Some(d @ b'0'..=b'7') => {
                let mut v = (d - b'0') as u32;
                for _ in 0..2 {
                    if let Some(&d @ b'0'..=b'7') = it.peek() {
                        v = v * 8 + (d - b'0') as u32;
                        it.next();
                    }
                }
                out.push(v as u8);
            }
            Some(o) => out.push(o),
            None => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `@@ -a[,b] +c[,d] @@` → the removed and added line numbers.
fn parse_hunk(l: &str) -> Option<(RangeInclusive<u32>, RangeInclusive<u32>)> {
    let rest = l.strip_prefix("@@ -")?;
    let (spec, _) = rest.split_once(" @@")?;
    let (old, new) = spec.split_once(" +")?;
    let range = |s: &str| -> Option<RangeInclusive<u32>> {
        let (start, len) = match s.split_once(',') {
            Some((a, b)) => (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?),
            None => (s.parse().ok()?, 1),
        };
        // A zero-length side (pure addition or deletion) is an empty range.
        Some(if len == 0 { 1..=0 } else { start..=start + len - 1 })
    };
    Some((range(old)?, range(new)?))
}

/// Linguist attributes from the commit's own `.gitattributes`.
fn linguist_attrs<'a>(commit: &str, paths: impl Iterator<Item = &'a str>) -> HashMap<String, Kind> {
    let input: String = paths.map(|p| format!("{p}\0")).collect();
    if input.is_empty() {
        return HashMap::new();
    }
    let args =
        ["check-attr", "-z", "--stdin", "--source", commit, "linguist-generated", "linguist-vendored", "linguist-documentation"];
    let Ok(out) = git::run_raw(None, &args, Some(&input), &[]) else { return HashMap::new() };
    if !out.ok {
        return HashMap::new();
    }
    let mut map = HashMap::new();
    let f: Vec<&str> = out.stdout.split('\0').collect();
    for t in f.chunks_exact(3) {
        if !matches!(t[2], "set" | "true") {
            continue;
        }
        let kind = if t[1] == "linguist-documentation" { Kind::Doc } else { Kind::Gen };
        // Generated wins over documentation.
        if map.get(t[0]) != Some(&Kind::Gen) {
            map.insert(t[0].to_string(), kind);
        }
    }
    map
}

/// A file's category from its path, attributes and (only if the path says
/// nothing) a "generated" marker in its first lines.
fn classify(path: &str, attr: Option<Kind>, content: impl FnOnce() -> Option<Vec<u8>>) -> Kind {
    if let Some(k) = attr {
        return k;
    }
    let lower = path.to_lowercase();
    let (dirs, name) = match lower.rsplit_once('/') {
        Some((d, n)) => (d.split('/').collect::<Vec<_>>(), n),
        None => (vec![], lower.as_str()),
    };
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let in_dir = |names: &[&str]| dirs.iter().any(|d| names.contains(d));

    let generated_name = matches!(name, "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" | "go.sum")
        || matches!(ext, "lock" | "svg" | "map" | "snap")
        || [".min.js", ".min.css", "_pb2.py", "_pb2.pyi", ".pb.go", ".generated.ts", ".gen.ts"].iter().any(|s| name.ends_with(s));
    if generated_name || in_dir(&["generated", "__generated__", "vendor", "node_modules"]) {
        return Kind::Gen;
    }
    let test_name = (name.starts_with("test_") && ext == "py")
        || ["_test.py", "_test.go", "_test.rs", "_tests.rs", "conftest.py"].iter().any(|s| name.ends_with(s))
        || [".test.", ".spec."].iter().any(|s| name.contains(s));
    if test_name || in_dir(&["tests", "test", "__tests__", "__mocks__", "testdata", "test_data", "e2e"]) {
        return Kind::Test;
    }
    let doc_name = matches!(ext, "md" | "mdx" | "rst" | "adoc" | "txt")
        || ["readme", "changelog", "license", "contributing"].iter().any(|s| name.starts_with(s));
    if doc_name || in_dir(&["docs", "doc"]) {
        return Kind::Doc;
    }
    if content().is_some_and(|c| has_generated_header(&c)) {
        return Kind::Gen;
    }
    Kind::Src
}

fn has_generated_header(content: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&content[..content.len().min(1024)]).to_lowercase();
    head.lines().take(5).any(|l| {
        let l = l.trim_start();
        let comment = ["//", "#", "/*", "*", "<!--", "--", ";"].iter().any(|c| l.starts_with(c));
        comment
            && ["@generated", "do not edit", "auto-generated", "autogenerated", "generated by", "code generated"]
                .iter()
                .any(|m| l.contains(m))
    })
}

/// 1-based line ranges of `#[cfg(test)]` items in rustfmt'd source.
fn rust_test_ranges(src: &str) -> Vec<RangeInclusive<u32>> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = vec![];
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i].trim_end();
        if l != "#[cfg(test)]" && !l.starts_with("#[cfg(all(test") {
            i += 1;
            continue;
        }
        let start = i;
        // Skip further attributes (e.g. `#[rstest]`, `#[allow(..)]`) to reach the item itself.
        let mut j = i + 1;
        while j < lines.len() && lines[j].starts_with("#[") {
            j += 1;
        }
        let end = if lines.get(j).is_some_and(|l| l.trim_end().ends_with(';') || l.trim_end().ends_with('}')) {
            j
        } else {
            (j..lines.len()).find(|&k| lines[k].starts_with('}')).unwrap_or(lines.len() - 1)
        };
        out.push(start as u32 + 1..=end.min(lines.len() - 1) as u32 + 1);
        i = end + 1;
    }
    out
}

// ───────────────────────────── display ─────────────────────────────

/// `1234` → `1.2k`.
pub fn human(n: u32) -> String {
    if n >= 10_000 {
        format!("{}k", n / 1000)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

pub fn label(k: Kind) -> &'static str {
    match k {
        Kind::Src => "src",
        Kind::Test => "test",
        Kind::Doc => "doc",
        Kind::Gen => "gen",
    }
}

/// `+120 −30` as (plain, colored) so callers can pad on the plain width.
pub fn counts(l: &Lines) -> (String, String) {
    use crate::ui;
    let mut p = vec![];
    let mut c = vec![];
    if l.add > 0 {
        p.push(format!("+{}", human(l.add)));
        c.push(ui::green(&format!("+{}", human(l.add))));
    }
    if l.del > 0 {
        p.push(format!("−{}", human(l.del)));
        c.push(ui::red(&format!("−{}", human(l.del))));
    }
    (p.join(" "), c.join(" "))
}

/// `+120 −30 src  +300 test` as (plain, colored).
pub fn summary(s: &Split) -> (String, String) {
    let (plain, color): (Vec<_>, Vec<_>) = s
        .iter()
        .filter(|(_, l)| !l.is_empty())
        .map(|(k, l)| {
            let (p, c) = counts(&l);
            (format!("{p} {}", label(k)), format!("{c} {}", crate::ui::dim(label(k))))
        })
        .unzip();
    (plain.join("  "), color.join("  "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks() {
        assert_eq!(parse_hunk("@@ -3,2 +3,0 @@ fn x"), Some((3..=4, 1..=0)));
        assert_eq!(parse_hunk("@@ -0,0 +1,5 @@"), Some((1..=0, 1..=5)));
        assert_eq!(parse_hunk("@@ -7 +7 @@"), Some((7..=7, 7..=7)));
    }

    #[test]
    fn rust_ranges() {
        let src = "fn a() {}\n\n#[cfg(test)]\nuse x;\n\n#[cfg(test)]\nmod tests {\n    #[rstest]\n    fn t() {}\n}\nfn after() {}\n";
        assert_eq!(rust_test_ranges(src), vec![3..=4, 6..=10]);
    }

    #[test]
    fn paths() {
        let none = || None;
        assert_eq!(classify("crates/x/src/lib.rs", None, none), Kind::Src);
        assert_eq!(classify("crates/x/tests/it.rs", None, none), Kind::Test);
        assert_eq!(classify("tests/unit/test_foo.py", None, none), Kind::Test);
        assert_eq!(classify("ui/src/a.test.tsx", None, none), Kind::Test);
        assert_eq!(classify("docs/my-website/x.md", None, none), Kind::Doc);
        assert_eq!(classify("README.md", None, none), Kind::Doc);
        assert_eq!(classify("uv.lock", None, none), Kind::Gen);
        assert_eq!(classify("crates/p/generated/x.rs", None, none), Kind::Gen);
        assert_eq!(classify("a.py", Some(Kind::Gen), none), Kind::Gen);
        assert_eq!(classify("a.rs", None, || Some(b"// Generated by x.py. Do not edit.\n".to_vec())), Kind::Gen);
    }

    #[test]
    fn quoting() {
        assert_eq!(unquote("\"a\\tb\\303\\251\""), "a\tbé");
        assert_eq!(unquote("plain"), "plain");
    }
}
