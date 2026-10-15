use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Command;
use std::time::Duration;

pub struct Client {
    agent: ureq::Agent,
    token: String,
    api: String,
    pub host: String,
    pub owner: String,
    pub repo: String,
}

/// Parses `git@github.com:owner/repo.git`, `ssh://git@host/owner/repo`, or `https://host/owner/repo`.
pub fn parse_remote(url: &str) -> Option<(String, String, String)> {
    let rest = if let Some(r) = url.strip_prefix("git@") {
        r.replacen(':', "/", 1)
    } else {
        let r = url.split("://").nth(1)?;
        r.rsplit_once('@').map(|(_, h)| h).unwrap_or(r).to_string()
    };
    let mut parts = rest.trim_end_matches('/').trim_end_matches(".git").splitn(3, '/');
    let host = parts.next()?.split(':').next()?.to_string();
    Some((host, parts.next()?.to_string(), parts.next()?.to_string()))
}

fn token(host: &str) -> Result<String> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var) {
            if !t.is_empty() {
                return Ok(t);
            }
        }
    }
    let out = Command::new("gh")
        .args(["auth", "token", "--hostname", host])
        .output()
        .context("running `gh auth token`")?;
    if !out.status.success() {
        bail!("no GitHub token: set GH_TOKEN or run `gh auth login`");
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Pr {
    pub number: u64,
    pub id: String,
    pub url: String,
    pub state: String,
    pub is_draft: bool,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub base_ref_name: String,
    pub review_decision: Option<String>,
    pub merge_queue_entry: Option<Value>,
    #[serde(default)]
    pub auto_merge_request: Option<Value>,
    #[serde(default)]
    pub commits: Option<Value>,
}

impl Pr {
    pub fn checks(&self) -> Option<String> {
        self.commits.as_ref()?["nodes"][0]["commit"]["statusCheckRollup"]["state"].as_str().map(str::to_string)
    }
}

const PR_FIELDS: &str = "number id url state isDraft title headRefName headRefOid baseRefName reviewDecision \
    mergeQueueEntry { position } autoMergeRequest { enabledAt } \
    commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }";

#[derive(Deserialize, Debug, Clone)]
pub struct RemoteStackPr {
    pub number: u64,
    #[serde(default)]
    pub merged_at: Option<String>,
    pub head: RemoteHead,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RemoteHead {
    #[serde(rename = "ref")]
    pub ref_name: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RemoteStack {
    pub id: u64,
    pub number: u64,
    pub base: RemoteHead,
    pub pull_requests: Vec<RemoteStackPr>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Login {
    pub login: String,
}

/// A PR as `gt review` needs it: enough to list it and fetch its head.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ReviewPr {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub is_draft: bool,
    pub head_ref_name: String,
    pub is_cross_repository: bool,
    pub additions: u64,
    pub deletions: u64,
    pub author: Option<Login>,
}

const REVIEW_FIELDS: &str =
    "number title state isDraft headRefName isCrossRepository additions deletions author { login }";

pub enum PrQuery {
    Number(u64),
    Head(String),
}

impl Client {
    pub fn new(remote_url: &str) -> Result<Client> {
        let (host, owner, repo) =
            parse_remote(remote_url).ok_or_else(|| anyhow!("can't parse GitHub remote url {remote_url}"))?;
        let api = match std::env::var("GT_GITHUB_API") {
            Ok(a) if !a.is_empty() => a,
            _ if host == "github.com" => "https://api.github.com".into(),
            _ => format!("https://{host}/api/v3"),
        };
        Ok(Client {
            agent: ureq::AgentBuilder::new().timeout(Duration::from_secs(60)).build(),
            token: token(&host)?,
            api,
            host,
            owner,
            repo,
        })
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        self.agent
            .request(method, &format!("{}/{}", self.api, path))
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28")
            .set("User-Agent", "gt")
    }

    /// Returns Ok(None) on 404 / 204.
    pub fn rest(&self, method: &str, path: &str, body: Option<Value>) -> Result<Option<Value>> {
        let req = self.request(method, path);
        let resp = match body {
            Some(b) => req.send_json(b),
            None => req.call(),
        };
        match resp {
            Ok(r) if r.status() == 204 => Ok(None),
            Ok(r) => Ok(Some(r.into_json()?)),
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                bail!("{method} {path}: HTTP {code}: {body}")
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn graphql(&self, query: &str) -> Result<Value> {
        let gql_path = if self.host == "github.com" { "graphql".to_string() } else { "../graphql".to_string() };
        let resp: Value = match self.request("POST", &gql_path).send_json(json!({ "query": query })) {
            Ok(r) => r.into_json()?,
            Err(ureq::Error::Status(code, r)) => bail!("graphql HTTP {code}: {}", r.into_string().unwrap_or_default()),
            Err(e) => return Err(e.into()),
        };
        if let Some(errs) = resp.get("errors").and_then(Value::as_array) {
            // Partial data is still useful (e.g. one PR number that no longer exists).
            let msgs: Vec<_> = errs.iter().filter_map(|e| e["message"].as_str()).collect();
            if resp.get("data").is_none_or(Value::is_null) {
                bail!("graphql: {}", msgs.join("; "));
            }
            for m in msgs {
                crate::ui::warn(&format!("github: {m}"));
            }
        }
        Ok(resp["data"].clone())
    }

    fn repo_selector(&self) -> String {
        format!("repository(owner: {}, name: {})", json!(self.owner), json!(self.repo))
    }

    /// Fetches many PRs in one round trip. Head lookups return the newest PR for that branch.
    pub fn prs(&self, queries: &[PrQuery]) -> Result<(String, Vec<Option<Pr>>)> {
        let mut q = format!("query {{ {} {{ id ", self.repo_selector());
        for (i, query) in queries.iter().enumerate() {
            match query {
                PrQuery::Number(n) => q += &format!("q{i}: pullRequest(number: {n}) {{ {PR_FIELDS} }} "),
                PrQuery::Head(h) => {
                    q += &format!(
                        "q{i}: pullRequests(headRefName: {}, first: 1, orderBy: {{field: CREATED_AT, direction: DESC}}) {{ nodes {{ {PR_FIELDS} }} }} ",
                        json!(h)
                    )
                }
            }
        }
        q += "} }";
        let data = self.graphql(&q)?;
        let repo = &data["repository"];
        let results = (0..queries.len())
            .map(|i| {
                let v = &repo[format!("q{i}")];
                let v = if v.get("nodes").is_some() { &v["nodes"][0] } else { v };
                if v.is_null() { None } else { serde_json::from_value(v.clone()).ok() }
            })
            .collect();
        Ok((repo["id"].as_str().unwrap_or_default().to_string(), results))
    }

    /// Open PRs in this repo where you're asked to review by name; requests
    /// made to a team you're on (e.g. litellm-engineering) don't count.
    pub fn review_requests(&self) -> Result<Vec<ReviewPr>> {
        let search = format!(
            "repo:{}/{} is:pr is:open archived:false user-review-requested:@me sort:updated-desc",
            self.owner, self.repo
        );
        let q = format!(
            "query {{ search(query: {}, type: ISSUE, first: 100) {{ nodes {{ ... on PullRequest {{ {REVIEW_FIELDS} }} }} }} }}",
            json!(search)
        );
        let data = self.graphql(&q)?;
        let nodes = data["search"]["nodes"].as_array().cloned().unwrap_or_default();
        Ok(nodes.into_iter().filter_map(|n| serde_json::from_value(n).ok()).collect())
    }

    /// One PR by number, or the newest one for a head branch.
    pub fn review_pr(&self, query: &PrQuery) -> Result<Option<ReviewPr>> {
        let field = match query {
            PrQuery::Number(n) => format!("pullRequest(number: {n}) {{ {REVIEW_FIELDS} }}"),
            PrQuery::Head(h) => format!(
                "pullRequests(headRefName: {}, first: 1, orderBy: {{field: CREATED_AT, direction: DESC}}) {{ nodes {{ {REVIEW_FIELDS} }} }}",
                json!(h)
            ),
        };
        let data = self.graphql(&format!("query {{ {} {{ pr: {field} }} }}", self.repo_selector()))?;
        let v = &data["repository"]["pr"];
        let v = if v.get("nodes").is_some() { &v["nodes"][0] } else { v };
        Ok(if v.is_null() { None } else { serde_json::from_value(v.clone()).ok() })
    }

    pub fn stack_for_pr(&self, pr: u64) -> Result<Option<RemoteStack>> {
        let v = self.rest("GET", &format!("repos/{}/{}/stacks?pull_request={pr}", self.owner, self.repo), None)?;
        let stacks: Vec<RemoteStack> = match v {
            Some(v) => serde_json::from_value(v)?,
            None => vec![],
        };
        Ok(stacks.into_iter().next())
    }

    pub fn get_stack(&self, number: u64) -> Result<Option<RemoteStack>> {
        let v = self.rest("GET", &format!("repos/{}/{}/stacks/{number}", self.owner, self.repo), None)?;
        Ok(v.map(serde_json::from_value).transpose()?)
    }

    pub fn create_stack(&self, prs: &[u64]) -> Result<RemoteStack> {
        let v = self.rest("POST", &format!("repos/{}/{}/stacks", self.owner, self.repo), Some(json!({ "pull_requests": prs })))?;
        Ok(serde_json::from_value(v.ok_or_else(|| anyhow!("empty create-stack response"))?)?)
    }

    pub fn add_to_stack(&self, number: u64, prs: &[u64]) -> Result<RemoteStack> {
        let v = self.rest(
            "POST",
            &format!("repos/{}/{}/stacks/{number}/add", self.owner, self.repo),
            Some(json!({ "pull_requests": prs })),
        )?;
        Ok(serde_json::from_value(v.ok_or_else(|| anyhow!("empty add-to-stack response"))?)?)
    }

    pub fn unstack(&self, number: u64) -> Result<()> {
        self.rest("POST", &format!("repos/{}/{}/stacks/{number}/unstack", self.owner, self.repo), None)?;
        Ok(())
    }

    pub fn pr_url(&self, number: u64) -> String {
        format!("https://{}/{}/{}/pull/{number}", self.host, self.owner, self.repo)
    }
}

/// One mutation document holding many aliased mutations; GitHub runs them in order.
#[derive(Default)]
pub struct MutationBatch {
    parts: Vec<String>,
}

impl MutationBatch {
    pub fn create_pr(&mut self, alias: &str, repo_id: &str, base: &str, head: &str, title: &str, body: &str, draft: bool) {
        self.parts.push(format!(
            "{alias}: createPullRequest(input: {{repositoryId: {}, baseRefName: {}, headRefName: {}, title: {}, body: {}, draft: {draft}}}) {{ pullRequest {{ number id url }} }}",
            json!(repo_id), json!(base), json!(head), json!(title), json!(body)
        ));
    }

    pub fn set_base(&mut self, alias: &str, pr_id: &str, base: &str) {
        self.parts.push(format!(
            "{alias}: updatePullRequest(input: {{pullRequestId: {}, baseRefName: {}}}) {{ pullRequest {{ number }} }}",
            json!(pr_id),
            json!(base)
        ));
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    pub fn run(&self, client: &Client) -> Result<Value> {
        client.graphql(&format!("mutation {{ {} }}", self.parts.join(" ")))
    }
}
