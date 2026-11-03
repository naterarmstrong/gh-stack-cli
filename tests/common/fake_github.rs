//! A fake GitHub API on a local port, enough of it for gt: the GraphQL
//! queries and mutations gt sends, and the REST stacks endpoints.
//!
//! PR heads are read live from the bare origin repo, so a push shows up the
//! way it would on GitHub. Every head an open PR is seen at is recorded, which
//! stands in for GitHub's force-push timeline. Merging a PR freezes its head.
//!
//! The GraphQL "parser" only understands the shapes gt sends: aliased
//! `pullRequest(...)`, `pullRequests(...)`, `createPullRequest(...)`,
//! `updatePullRequest(...)` fields and one `search(...)`.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use super::{OWNER, REPO};

#[derive(Clone, Debug)]
pub struct FakePr {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    /// OPEN, CLOSED or MERGED.
    pub state: String,
    pub draft: bool,
    pub cross_repo: bool,
    pub author: String,
    /// Shows up in `user-review-requested:@me` searches.
    pub review_requested: bool,
    pub review_decision: Option<String>,
    /// statusCheckRollup state: SUCCESS, FAILURE, PENDING…
    pub checks: Option<String>,
    /// The head once merged or closed; open PRs follow the branch on origin.
    pub frozen_head: Option<String>,
    /// Every head this PR has been seen at, oldest first.
    pub heads: Vec<String>,
}

impl FakePr {
    pub fn new(head: &str, base: &str) -> FakePr {
        FakePr {
            number: 0,
            title: format!("Change {head}"),
            body: String::new(),
            head: head.into(),
            base: base.into(),
            state: "OPEN".into(),
            draft: false,
            cross_repo: false,
            author: "someone".into(),
            review_requested: false,
            review_decision: None,
            checks: None,
            frozen_head: None,
            heads: vec![],
        }
    }
}

#[derive(Clone, Debug)]
pub struct FakeStack {
    pub number: u64,
    pub base: String,
    pub prs: Vec<u64>,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: String,
}

impl Request {
    /// The GraphQL document, for GraphQL requests.
    pub fn query(&self) -> String {
        serde_json::from_str::<Value>(&self.body)
            .ok()
            .and_then(|v| v["query"].as_str().map(str::to_string))
            .unwrap_or_default()
    }
}

struct State {
    origin: PathBuf,
    prs: Vec<FakePr>,
    stacks: Vec<FakeStack>,
    requests: Vec<Request>,
    next_pr: u64,
    next_stack: u64,
}

pub struct FakeGitHub {
    pub url: String,
    state: Arc<Mutex<State>>,
}

impl FakeGitHub {
    pub fn start(origin: &Path) -> FakeGitHub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            origin: origin.to_path_buf(),
            prs: vec![],
            stacks: vec![],
            requests: vec![],
            next_pr: 1,
            next_stack: 1,
        }));
        let shared = state.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let _ = serve(conn, &shared);
            }
        });
        FakeGitHub { url, state }
    }

    /// Opens a PR as someone else would, numbered after the existing ones.
    pub fn add_pr(&self, mut pr: FakePr) -> u64 {
        let mut s = self.state.lock().unwrap();
        pr.number = s.next_pr;
        s.next_pr += 1;
        let n = pr.number;
        s.prs.push(pr);
        s.observe();
        n
    }

    pub fn pr(&self, n: u64) -> FakePr {
        let mut s = self.state.lock().unwrap();
        s.observe();
        s.prs.iter().find(|p| p.number == n).cloned().unwrap_or_else(|| panic!("no PR #{n}"))
    }

    pub fn prs(&self) -> Vec<FakePr> {
        let mut s = self.state.lock().unwrap();
        s.observe();
        s.prs.clone()
    }

    /// The newest PR for a head branch.
    pub fn pr_for(&self, head: &str) -> FakePr {
        self.prs().into_iter().rev().find(|p| p.head == head).unwrap_or_else(|| panic!("no PR for {head}"))
    }

    pub fn update(&self, n: u64, f: impl FnOnce(&mut FakePr)) {
        let mut s = self.state.lock().unwrap();
        s.observe();
        f(s.prs.iter_mut().find(|p| p.number == n).unwrap());
    }

    /// Marks a PR merged at its current head (see `World::merge_pr` to also land it on main).
    pub fn merge(&self, n: u64) {
        let mut s = self.state.lock().unwrap();
        s.observe();
        let origin = s.origin.clone();
        let pr = s.prs.iter_mut().find(|p| p.number == n).unwrap();
        pr.frozen_head = Some(live_head(&origin, pr));
        pr.state = "MERGED".into();
    }

    pub fn add_stack(&self, prs: &[u64]) -> u64 {
        let mut s = self.state.lock().unwrap();
        s.create_stack(prs.to_vec())
    }

    pub fn stacks(&self) -> Vec<FakeStack> {
        self.state.lock().unwrap().stacks.clone()
    }

    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn clear_requests(&self) {
        self.state.lock().unwrap().requests.clear();
    }

    /// GraphQL documents received so far.
    pub fn queries(&self) -> Vec<String> {
        self.requests().iter().filter(|r| r.path.ends_with("/graphql")).map(Request::query).collect()
    }
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").current_dir(dir).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Where an open PR's head is on origin now (fork PRs live under refs/pull/N/head).
fn live_head(origin: &Path, pr: &FakePr) -> String {
    if let Some(h) = &pr.frozen_head {
        return h.clone();
    }
    let r = if pr.cross_repo { format!("refs/pull/{}/head", pr.number) } else { format!("refs/heads/{}", pr.head) };
    git_out(origin, &["rev-parse", "--verify", "--quiet", &r]).unwrap_or_default()
}

impl State {
    /// Records each open PR's current head, like GitHub's push timeline.
    fn observe(&mut self) {
        let origin = self.origin.clone();
        for pr in self.prs.iter_mut().filter(|p| p.state == "OPEN") {
            let h = live_head(&origin, pr);
            if !h.is_empty() && pr.heads.last() != Some(&h) {
                pr.heads.push(h);
            }
        }
    }

    fn head_of(&self, pr: &FakePr) -> String {
        live_head(&self.origin, pr)
    }

    fn create_stack(&mut self, prs: Vec<u64>) -> u64 {
        let base = self.prs.iter().find(|p| Some(&p.number) == prs.first()).map(|p| p.base.clone()).unwrap_or_default();
        let number = self.next_stack;
        self.next_stack += 1;
        self.stacks.push(FakeStack { number, base, prs });
        number
    }

    fn stack_json(&self, st: &FakeStack) -> Value {
        let prs: Vec<Value> = st
            .prs
            .iter()
            .filter_map(|n| self.prs.iter().find(|p| p.number == *n))
            .map(|p| {
                json!({
                    "number": p.number,
                    "merged_at": (p.state == "MERGED").then_some("2026-01-01T00:00:00Z"),
                    "head": { "ref": p.head },
                })
            })
            .collect();
        json!({ "id": st.number * 1000, "number": st.number, "base": { "ref": st.base }, "pull_requests": prs })
    }

    fn pr_json(&self, p: &FakePr) -> Value {
        json!({
            "number": p.number,
            "id": format!("PR_{}", p.number),
            "url": format!("https://github.com/{OWNER}/{REPO}/pull/{}", p.number),
            "state": p.state,
            "isDraft": p.draft,
            "title": p.title,
            "headRefName": p.head,
            "headRefOid": self.head_of(p),
            "baseRefName": p.base,
            "reviewDecision": p.review_decision,
            "mergeQueueEntry": null,
            "autoMergeRequest": null,
            "isCrossRepository": p.cross_repo,
            "additions": 10,
            "deletions": 2,
            "author": { "login": p.author },
            "commits": { "nodes": [ { "commit": { "statusCheckRollup": p.checks.as_ref().map(|c| json!({ "state": c })) } } ] },
        })
    }

    /// Commits and force pushes, as `past_heads` asks for them.
    fn history_json(&self, p: &FakePr) -> Value {
        let head = self.head_of(p);
        let pushes: Vec<Value> = p
            .heads
            .windows(2)
            .map(|w| json!({ "beforeCommit": { "oid": w[0] }, "afterCommit": { "oid": w[1] } }))
            .collect();
        json!({
            "commits": { "nodes": [ { "commit": { "oid": head } } ] },
            "timelineItems": { "nodes": pushes },
        })
    }

    fn graphql(&mut self, query: &str) -> Value {
        if query.trim_start().starts_with("mutation") {
            let mut data = serde_json::Map::new();
            for call in calls(query) {
                match call.field {
                    "createPullRequest" => {
                        let mut pr = FakePr::new(
                            &arg_str(call.args, "headRefName").unwrap(),
                            &arg_str(call.args, "baseRefName").unwrap(),
                        );
                        pr.title = arg_str(call.args, "title").unwrap();
                        pr.body = arg_str(call.args, "body").unwrap_or_default();
                        pr.draft = arg_bool(call.args, "draft").unwrap_or(false);
                        pr.author = "test-user".into();
                        pr.number = self.next_pr;
                        self.next_pr += 1;
                        let n = pr.number;
                        self.prs.push(pr);
                        self.observe();
                        data.insert(
                            call.alias.into(),
                            json!({ "pullRequest": {
                                "number": n,
                                "id": format!("PR_{n}"),
                                "url": format!("https://github.com/{OWNER}/{REPO}/pull/{n}"),
                            }}),
                        );
                    }
                    "updatePullRequest" => {
                        let id = arg_str(call.args, "pullRequestId").unwrap();
                        let n: u64 = id.trim_start_matches("PR_").parse().unwrap();
                        let base = arg_str(call.args, "baseRefName").unwrap();
                        let pr = self.prs.iter_mut().find(|p| p.number == n).unwrap();
                        pr.base = base;
                        data.insert(call.alias.into(), json!({ "pullRequest": { "number": n } }));
                    }
                    other => panic!("fake GitHub doesn't know mutation {other}"),
                }
            }
            return json!({ "data": data });
        }

        if query.contains("search(query:") {
            let nodes: Vec<Value> = self
                .prs
                .iter()
                .filter(|p| p.review_requested && p.state == "OPEN")
                .rev()
                .map(|p| self.pr_json(p))
                .collect();
            return json!({ "data": { "search": { "nodes": nodes } } });
        }

        let history = query.contains("timelineItems");
        let mut repo = serde_json::Map::new();
        repo.insert("id".into(), json!("R_1"));
        for call in calls(query) {
            let v = match call.field {
                "pullRequest" => {
                    let n = arg_u64(call.args, "number").unwrap();
                    match self.prs.iter().find(|p| p.number == n) {
                        Some(p) if history => self.history_json(p),
                        Some(p) => self.pr_json(p),
                        None => Value::Null,
                    }
                }
                "pullRequests" => {
                    let nodes: Vec<Value> = if let Some(head) = arg_str(call.args, "headRefName") {
                        // Newest first, one result.
                        self.prs.iter().rev().filter(|p| p.head == head).take(1).map(|p| self.pr_json(p)).collect()
                    } else if let Some(base) = arg_str(call.args, "baseRefName") {
                        self.prs
                            .iter()
                            .filter(|p| p.base == base && p.state == "OPEN")
                            .map(|p| self.pr_json(p))
                            .collect()
                    } else {
                        panic!("fake GitHub: unexpected pullRequests args {}", call.args)
                    };
                    json!({ "nodes": nodes })
                }
                other => panic!("fake GitHub doesn't know field {other}"),
            };
            repo.insert(call.alias.into(), v);
        }
        json!({ "data": { "repository": repo } })
    }

    /// Handles one request; returns (status, body).
    fn handle(&mut self, method: &str, path: &str, body: &str) -> (u16, Option<Value>) {
        self.observe();
        if path == "/graphql" {
            let q: Value = serde_json::from_str(body).unwrap();
            return (200, Some(self.graphql(q["query"].as_str().unwrap())));
        }
        let prefix = format!("/repos/{OWNER}/{REPO}/stacks");
        let Some(rest) = path.strip_prefix(&prefix) else { return (404, None) };
        let body: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let pr_list =
            || -> Vec<u64> { body["pull_requests"].as_array().unwrap().iter().map(|n| n.as_u64().unwrap()).collect() };
        match (method, rest) {
            ("GET", q) if q.starts_with("?pull_request=") => {
                let n: u64 = q["?pull_request=".len()..].parse().unwrap();
                let found: Vec<Value> =
                    self.stacks.iter().filter(|s| s.prs.contains(&n)).map(|s| self.stack_json(s)).collect();
                (200, Some(json!(found)))
            }
            ("POST", "") => {
                let n = self.create_stack(pr_list());
                let st = self.stacks.iter().find(|s| s.number == n).unwrap().clone();
                (201, Some(self.stack_json(&st)))
            }
            ("GET", r) => {
                let n: u64 = r.trim_start_matches('/').parse().unwrap();
                match self.stacks.iter().find(|s| s.number == n).cloned() {
                    Some(st) => (200, Some(self.stack_json(&st))),
                    None => (404, None),
                }
            }
            ("POST", r) if r.ends_with("/add") => {
                let n: u64 = r.trim_start_matches('/').trim_end_matches("/add").parse().unwrap();
                let add = pr_list();
                let st = self.stacks.iter_mut().find(|s| s.number == n).unwrap();
                st.prs.extend(add);
                let st = st.clone();
                (200, Some(self.stack_json(&st)))
            }
            ("POST", r) if r.ends_with("/unstack") => {
                let n: u64 = r.trim_start_matches('/').trim_end_matches("/unstack").parse().unwrap();
                self.stacks.retain(|s| s.number != n);
                (204, None)
            }
            _ => (404, None),
        }
    }
}

fn serve(conn: TcpStream, state: &Arc<Mutex<State>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut len = 0;
    loop {
        let mut h = String::new();
        reader.read_line(&mut h)?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();

    let (code, resp) = {
        let mut s = state.lock().unwrap();
        s.requests.push(Request { method: method.clone(), path: path.clone(), body: body.clone() });
        s.handle(&method, &path, &body)
    };
    let text = resp.map(|v| v.to_string()).unwrap_or_default();
    let reason = match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        _ => "Not Found",
    };
    let mut conn = conn;
    write!(
        conn,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )?;
    conn.flush()
}

// ───────────────────────────── GraphQL scanning ─────────────────────────────

struct Call<'a> {
    alias: &'a str,
    field: &'static str,
    args: &'a str,
}

/// Every `alias: field(args)` in a document, for the fields gt uses, in order.
fn calls(q: &str) -> Vec<Call<'_>> {
    const FIELDS: [&str; 4] = ["pullRequest", "pullRequests", "createPullRequest", "updatePullRequest"];
    let mut out = vec![];
    let mut i = 0;
    while i < q.len() {
        // Skip string literals so titles and bodies can't look like fields.
        if q.as_bytes()[i] == b'"' {
            i = end_of_string(q, i);
            continue;
        }
        let rest = &q[i..];
        if let Some(field) =
            rest.strip_prefix(": ").and_then(|r| FIELDS.iter().find(|f| r.starts_with(&format!("{f}("))))
        {
            let alias_start = q[..i].rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_').map_or(0, |p| p + 1);
            let open = i + 2 + field.len();
            let close = matching_paren(q, open);
            out.push(Call { alias: &q[alias_start..i], field, args: &q[open + 1..close] });
            i = close;
            continue;
        }
        i += rest.chars().next().map_or(1, char::len_utf8);
    }
    out
}

/// Index just past the string literal starting at `start`.
fn end_of_string(q: &str, start: usize) -> usize {
    let b = q.as_bytes();
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

fn matching_paren(q: &str, open: usize) -> usize {
    let b = q.as_bytes();
    let mut depth = 0;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'"' => {
                i = end_of_string(q, i);
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("unbalanced parens in {q}");
}

/// The text right after `key: ` in an argument list, outside string literals.
fn arg_raw<'a>(args: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("{key}: ");
    let mut i = 0;
    while i < args.len() {
        if args.as_bytes()[i] == b'"' {
            i = end_of_string(args, i);
            continue;
        }
        let boundary = i == 0 || !args.as_bytes()[i - 1].is_ascii_alphanumeric();
        if boundary && args[i..].starts_with(&pat) {
            return Some(&args[i + pat.len()..]);
        }
        i += 1;
    }
    None
}

fn arg_str(args: &str, key: &str) -> Option<String> {
    let rest = arg_raw(args, key)?;
    let end = end_of_string(rest, 0);
    serde_json::from_str(&rest[..end]).ok()
}

fn arg_u64(args: &str, key: &str) -> Option<u64> {
    let rest = arg_raw(args, key)?;
    rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

fn arg_bool(args: &str, key: &str) -> Option<bool> {
    let rest = arg_raw(args, key)?;
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_aliased_calls() {
        let q = r#"query { repository(owner: "acme", name: "widgets") { id q0: pullRequest(number: 12) { number } q1: pullRequests(headRefName: "a (b)", first: 1) { nodes { number } } } }"#;
        let c = calls(q);
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].alias, c[0].field, arg_u64(c[0].args, "number")), ("q0", "pullRequest", Some(12)));
        assert_eq!((c[1].alias, c[1].field), ("q1", "pullRequests"));
        assert_eq!(arg_str(c[1].args, "headRefName").as_deref(), Some("a (b)"));
    }
}
