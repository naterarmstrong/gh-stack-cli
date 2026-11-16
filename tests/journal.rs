//! The run log under ~/.local/state/gt/log: one JSON line per event.

mod common;

use common::World;
use serde_json::Value;

/// Every event logged so far, oldest first.
fn events(w: &World) -> Vec<Value> {
    let dir = w.home.join(".local/state/gt/log");
    let mut lines = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let text = std::fs::read_to_string(e.unwrap().path()).unwrap();
        lines.extend(text.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()));
    }
    lines
}

fn of_run<'a>(events: &'a [Value], argv1: &str) -> Vec<&'a Value> {
    let run = events
        .iter()
        .find(|e| e["ev"] == "start" && e["argv"][1] == argv1)
        .unwrap_or_else(|| panic!("no run of {argv1}: {events:#?}"))["run"]
        .clone();
    events.iter().filter(|e| e["run"] == run).collect()
}

#[test]
fn a_run_logs_its_repo_git_calls_output_and_end() {
    let w = World::new();
    w.create("feat-a", "a.txt", "a\n");
    w.gt_ok(&["ls"]);
    let all = events(&w);
    let run = of_run(&all, "ls");

    assert_eq!(run[0]["ev"], "start");
    let repo = run.iter().find(|e| e["ev"] == "repo").expect("repo event");
    assert_eq!(repo["branch"], "feat-a");
    let end = run.last().unwrap();
    assert_eq!(end["ev"], "end");
    assert_eq!(end["code"], 0);
    let head = end["stdout"]["head"].as_array().unwrap();
    assert!(head.iter().any(|l| l.as_str().unwrap().contains("feat-a")), "{end}");
}

#[test]
fn a_failed_run_logs_the_error_and_its_git_calls() {
    let w = World::new();
    w.gt_err(&["checkout", "no-such-branch"]);
    w.gt_err(&["no-such-command"]);
    let all = events(&w);

    let end = *of_run(&all, "checkout").last().unwrap();
    assert_eq!(end["code"], 1);
    assert!(end["err"].as_str().unwrap().contains("no-such-branch"), "{end}");

    let end = *of_run(&all, "no-such-command").last().unwrap();
    assert_eq!(end["code"], 2);
    assert_eq!(end["err"], "unrecognized subcommand");

    // `create` commits through git, and each call is timed.
    w.create("feat-a", "a.txt", "a\n");
    let all = events(&w);
    let git: Vec<_> = of_run(&all, "create").into_iter().filter(|e| e["ev"] == "git").collect();
    assert!(git.iter().any(|e| e["args"][0] == "commit" && e["code"] == 0 && e["ms"].is_u64()), "{git:#?}");
}
