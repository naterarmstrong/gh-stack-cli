//! A log of every run, one JSON object per line, in
//! `~/.local/state/gt/log/<date>.jsonl` (or `$GT_LOG_DIR`): what was run and
//! where, every git and GitHub call with its timing, what was printed, what
//! was picked, and how it ended. A run with a `start` but no `end` was killed.
//! `GT_LOG=0` turns it off. Writing it never fails a command.

use serde_json::{Map, Value, json};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// Days of logs kept; older files are deleted when a new day's file starts.
const KEEP_DAYS: u64 = 14;
/// Stdout is summarized rather than kept: `gt log` prints much the same every run.
const STDOUT_HEAD: usize = 20;
const TRUNCATE: usize = 2000;

struct Journal {
    file: Mutex<File>,
    run: String,
    at: String,
    start: Instant,
    stdout: Mutex<Stdout>,
    repo_seen: Mutex<bool>,
}

struct Stdout {
    lines: usize,
    head: Vec<String>,
    /// FNV-1a over every line, to tell repeated output from changed output.
    hash: u64,
}

impl Default for Stdout {
    fn default() -> Stdout {
        Stdout { lines: 0, head: Vec::new(), hash: 0xcbf29ce484222325 }
    }
}

static J: OnceLock<Option<Journal>> = OnceLock::new();

fn dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("GT_LOG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    let state = match std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        Some(s) => PathBuf::from(s),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(state.join("gt/log"))
}

/// Local calendar time, `(YYYY-MM-DD, HH:MM:SS)`.
fn local_now() -> (String, String) {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    unsafe { libc::localtime_r(&t, &mut tm) };
    (
        format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday),
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec),
    )
}

fn open(date: &str) -> Option<File> {
    let dir = dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{date}.jsonl"));
    if !path.exists() {
        prune(&dir);
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

fn prune(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let cutoff = SystemTime::now() - Duration::from_secs(KEEP_DAYS * 86400);
    for e in entries.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|m| m < cutoff);
        if old && e.path().extension().is_some_and(|x| x == "jsonl") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn get() -> Option<&'static Journal> {
    J.get().and_then(Option::as_ref)
}

/// Opens the log and records the run's start. Call once, first thing.
pub fn start() {
    let j = J.get_or_init(|| {
        if std::env::var_os("GT_LOG").is_some_and(|v| v == "0") {
            return None;
        }
        let (date, time) = local_now();
        let file = open(&date)?;
        let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().subsec_nanos();
        Some(Journal {
            file: Mutex::new(file),
            run: format!("{:x}", (std::process::id() as u64) << 32 ^ nanos as u64),
            at: format!("{date}T{time}"),
            start: Instant::now(),
            stdout: Mutex::default(),
            repo_seen: Mutex::new(false),
        })
    });
    let Some(j) = j else { return };
    event(
        "start",
        json!({
            "at": j.at,
            "argv": std::env::args().collect::<Vec<_>>(),
            "cwd": std::env::current_dir().ok(),
            "ver": env!("CARGO_PKG_VERSION"),
        }),
    );
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |p| {
        event("panic", json!({ "msg": p.to_string() }));
        prev(p);
    }));
}

/// Appends one event; `fields` must be a JSON object.
pub fn event(ev: &str, fields: Value) {
    let Some(j) = get() else { return };
    let mut m = Map::new();
    m.insert("run".into(), j.run.clone().into());
    m.insert("t".into(), (j.start.elapsed().as_millis() as u64).into());
    m.insert("ev".into(), ev.into());
    if let Value::Object(f) = fields {
        m.extend(f);
    }
    let mut line = Value::Object(m).to_string();
    line.push('\n');
    // One write per line, so concurrent runs in other worktrees can't interleave.
    let _ = j.file.lock().unwrap().write_all(line.as_bytes());
}

/// The repository and branch, recorded the first time they're known.
pub fn repo(toplevel: &std::path::Path, branch: Option<&str>) {
    let Some(j) = get() else { return };
    let mut seen = j.repo_seen.lock().unwrap();
    if !*seen {
        *seen = true;
        drop(seen);
        event("repo", json!({ "repo": toplevel, "branch": branch }));
    }
}

/// A line printed on stdout (see `outln!`).
pub fn stdout(s: &str) {
    let Some(j) = get() else { return };
    let mut o = j.stdout.lock().unwrap();
    for line in plain(s).split('\n') {
        o.lines += 1;
        for b in line.bytes().chain(*b"\n") {
            o.hash = (o.hash ^ b as u64).wrapping_mul(0x100000001b3);
        }
        if o.head.len() < STDOUT_HEAD {
            o.head.push(line.to_string());
        }
    }
}

/// A line printed on stderr: `kind` is info, ok, warn or err.
pub fn stderr(kind: &str, s: &str) {
    event(kind, json!({ "msg": truncate(&plain(s)) }));
}

/// Records how the run ended. `err` is the message shown after `error:`.
pub fn end(code: i32, err: Option<&str>) {
    let Some(j) = get() else { return };
    let o = std::mem::take(&mut *j.stdout.lock().unwrap());
    let mut f = json!({ "code": code, "ms": j.start.elapsed().as_millis() as u64 });
    if o.lines > 0 {
        f["stdout"] = json!({ "lines": o.lines, "hash": format!("{:016x}", o.hash), "head": o.head });
    }
    if let Some(e) = err {
        f["err"] = truncate(&plain(e)).into();
    }
    event("end", f);
}

/// Ends the run and exits; use instead of `std::process::exit`.
pub fn exit(code: i32) -> ! {
    end(code, None);
    std::process::exit(code)
}

pub fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

pub fn truncate(s: &str) -> String {
    if s.len() <= TRUNCATE {
        return s.to_string();
    }
    let mut cut = TRUNCATE;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… ({} bytes)", &s[..cut], s.len())
}

/// Drops ANSI color and OSC 8 hyperlink sequences.
fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then a final letter.
            Some('[') => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            // OSC: runs to ESC \ (or BEL).
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// `println!` that also goes in the log.
#[macro_export]
macro_rules! outln {
    () => { $crate::outln!("") };
    ($($arg:tt)*) => {{
        let s = format!($($arg)*);
        println!("{s}");
        $crate::journal::stdout(&s);
    }};
}

/// `eprintln!` that also goes in the log.
#[macro_export]
macro_rules! errln {
    () => { $crate::errln!("") };
    ($($arg:tt)*) => {{
        let s = format!($($arg)*);
        eprintln!("{s}");
        $crate::journal::stderr("err", &s);
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_strips_colors_and_links() {
        let s = "\x1b[1mbold\x1b[0m \x1b]8;;https://x/1\x1b\\#1\x1b]8;;\x1b\\ \x1b[2;32mok\x1b[0m";
        assert_eq!(plain(s), "bold #1 ok");
    }

    #[test]
    fn truncate_keeps_char_boundaries() {
        let s = "é".repeat(TRUNCATE);
        assert!(truncate(&s).starts_with(&"é".repeat(TRUNCATE / 2)));
    }
}
