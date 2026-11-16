# gt

A fast, Graphite-style CLI for stacked PRs, built on `gh stack`'s state and GitHub's stacks API.

`gt` reads and writes the same `.git/gh-stack` file that `gh stack` uses (unknown fields are kept, and it takes the same lock files), so you can switch between the two tools freely. Branches you haven't submitted yet live in gt's own `.git/gt-stack` instead (see [Local branches](#local-branches)), so `gh stack submit` never opens PRs for them. It restacks in memory with `git merge-tree` + `git commit-tree`, so restacking never checks out a branch or touches a working tree, and it batches GitHub calls into single GraphQL round trips.

**It is built around one commit per branch.** With one commit, a branch's old base is always its tip's parent, and `gt modify` amends that commit, so each amend is a new version of the branch. A branch can also have several commits, such as one a cloud agent keeps pushing to. Then each commit counts as a version, the way each amend does on a single-commit branch, and gt restacks, syncs and reverts the branch with all of its commits. See [Branches with several commits](#branches-with-several-commits). `gt squash` folds a branch with several commits back into one.

## Install

```sh
cargo install --path .
```

This puts a `gt` binary in `~/.cargo/bin`. The release profile uses LTO and strips symbols.

### Auth

`gt` takes a GitHub token from `GH_TOKEN` or `GITHUB_TOKEN`, and otherwise from `gh auth token --hostname <host>`. The host, owner and repo come from the remote URL (SSH and HTTPS forms both work).

## Quick start

```sh
git checkout main
# ...make changes...
gt create -am "Add the parser"          # new branch above main, commit staged changes
# ...more changes...
gt create -am "Use the parser in the CLI"
gt submit --draft                       # push, open PRs, link them as a GitHub stack
gt log                                  # stacks with PR, CI and review status

# after review feedback on the bottom branch
gt bottom
# ...edit...
gt modify -a                            # amend, restack everything above it
gt submit

# once PRs merge
gt sync                                 # fetch, prune merged, restack onto trunk
gt submit                               # push what sync restacked
```

## Commands

Aliases are in parentheses.

### Viewing

| Command | What it does |
| --- | --- |
| `gt log` (`l`) | Every local stack and review branch with PR, CI, review and merge-queue status. `-s` shows only the current stack. |
| `gt ls` | The same graph from local state only (no network), with each branch's src lines and total diff size. |
| `gt loc [target]` | Lines changed by a branch's commit, per file, split into **src / test / doc / gen**. `--json` gives machine-readable output. |
| `gt open [target]` (`pr`) | Open a branch's PR in the browser. |

### History of a branch's commit

| Command | What it does |
| --- | --- |
| `gt history [branch]` (`h`) | Every version of a branch's commit, newest first: p1, p2, … with when and how each appeared, whether it reached the remote, and what it changed over the version before. `--json` gives machine-readable output. |
| `gt history show <n>` | What version `n` changed over its own base, as a diff. This is the PR as it was at that version. |
| `gt history compare [a b]` | What changed from version `a` to version `b`. With no versions it compares the previous version with the current one. `base` stands for the other side's base: its parent, or on a branch with several commits, the commit the oldest of them sits on. `--json` gives the endpoints, the two trees and the changed files. |
| `gt history revert <n> [-- paths…]` | Put the branch, or only the given paths, back on version `n`, then restack like `gt modify`. On a branch with several commits, the whole branch is reset to version `n`'s commit, which drops the commits after it, and reverting paths adds a commit instead. `--exact` takes the version as it was (its old base, or its files verbatim) for when re-applying it conflicts. `-y` skips the confirmation. |

These commands take a branch with `-b`. Versions are named `p3` or `3`, and any commit works too.

Since `gt modify` amends the branch's one commit, the branch's reflog holds every version of it. A move that keeps the patch, such as a restack or a revert back to an earlier version, folds into that version (by `git patch-id`) instead of adding a new one. What a version changed is an interdiff: the earlier patch is re-applied onto the later one's base before diffing, so whatever landed on the parent in between is left out. Reflogs are per-clone, so the history only covers this clone, and "pushed"/"fetched" only cover pushes and fetches made from it. Patch-ids and re-applied trees are cached under `<git-common-dir>/gt/history-v<N>.json`, where an entry is dropped once it goes 30 days without being read, and diff sizes share `gt loc`'s cache, so a warm run reads the reflogs and spawns a single `git rev-list`.

### Branches with several commits

One commit per branch is the simple case, and nothing changes for it. When a branch has several commits of its own, meaning the commits not on trunk or a branch below it:

- **Versions:** each commit is a version, and its patch is the branch's whole change up to that commit, measured from the commit the oldest one sits on. On a single-commit branch that is just the commit's diff, so the two cases follow one rule. A commit the branch only passed through also gets a version, such as the first two of three commits a fetch brought in at once (the reflog only records the third). `gt history` marks it "passed through", and `--json` gives it `"tip": false`. A restack of every commit, a `gt squash`, or a reset back to a commit keeps the branch's change, so it folds into the version it already had.
- **Restack** replays the commits one at a time, in memory, keeping each commit's message and author. A conflict stops at the first commit that doesn't apply, and resolving it is the same `git rebase` as for one commit. A branch whose own commits include a merge isn't restacked; `gt squash` folds it.
- **`gt modify`** adds a commit rather than amending the last one.
- **`gt history revert`** resets the branch to the version's commit and drops the commits after it. A version on an older base has its commits replayed onto the branch's current one. Reverting only some paths adds a commit.
- **`gt sync`** takes commits pushed on top of the branch as they are, as long as they continue it in a line. A single-commit branch still folds a push back into its one commit.
- **`gt submit`** opens the PR with the oldest commit's message as its title and body.
- **`gt ls`** shows the number of commits. A branch is restacked when its oldest commit sits on its parent.

### Moving around

| Command | What it does |
| --- | --- |
| `gt up [n]` (`u`) / `gt down [n]` (`d`) | Check out the branch `n` above or below (default 1). |
| `gt top` (`t`) / `gt bottom` (`b`) | Check out the top or bottom of the stack. |
| `gt trunk` | Check out the stack's trunk. |
| `gt checkout [target]` (`co`) | Check out by branch name or PR number. With no target, opens an interactive picker. |

### Editing

| Command | What it does |
| --- | --- |
| `gt create [name] -m <msg>` (`c`) | Create a branch above the current one and commit staged changes to it. `--base <branch>` puts it above that branch (name or PR number) instead; uncommitted changes come along, as with `git checkout -b`. `-a` stages everything first. The name comes from the message if you leave it out. The branch is local until submitted, unless it goes below a branch that's already shared. |
| `gt modify` (`m`) | Amend the current branch's commit (or make the first one if the branch has none), then restack everything above it. A branch that already has several commits gets a new commit instead, and `--new-commit` adds one to a single-commit branch too. On a branch outside any stack it just commits, with nothing to restack. `-a` stages everything, `-m` replaces the message (or sets it, for a new commit). With nothing staged, it only restacks. |
| `gt restack` (`r`) | Rebase each branch onto its parent, in memory. `--trunk` also moves the bottom branch onto the latest remote trunk. |
| `gt squash` | Fold each branch in the stack down to one commit. The content stays the same. |
| `gt continue` | Carry on after you resolve a restack conflict (finishes a `git rebase` in progress first). Once the whole stack is restacked, it checks out the branch you were on when the conflict started. |
| `gt abort` | Give up on a restack conflict: aborts the `git rebase`, marks the branch as needing a rebase, and checks out the branch you were on. Branches below it that already restacked cleanly stay restacked. |

### Syncing with GitHub

| Command | What it does |
| --- | --- |
| `gt submit` (`ss`) | Restack, push, create PRs (title and body from the commit message) or retarget them onto the right base, and link the stack on GitHub. This shares the current branch and every local branch below it; local branches above it stay local (restacked, not pushed). `--draft` opens new PRs as drafts. `--relink` replaces the GitHub stack if it doesn't match the local order. `--skip-conflicts` submits every branch below the nearest one that needs a rebase or would conflict, from anywhere in the stack, and leaves that branch and those above it untouched. |
| `gt sync` (`s`) | Fetch, mark merged PRs, pull in commits others pushed to your branches, fast-forward local trunks (including ones checked out in other worktrees), and restack onto trunk. It never pushes: it lists the branches that changed and aren't pushed, and `gt submit` pushes them. A branch that would conflict is left where it is and marked **needs rebase** in `gt log`/`gt ls`; the branches above it are restacked onto it. Sync never stops for a conflict or checks out a branch. See flags below. |
| `gt get <target>` | Pull down a stack from GitHub by PR number, URL or branch. |
| `gt track [branches…]` | Start tracking existing branches (bottom first) as a local stack. Defaults to the current branch. `--trunk` sets the trunk. |

To fix a branch that sync skipped, run `gt checkout <branch> && gt restack --trunk`. That starts a normal `git rebase` you can finish with `gt continue` or back out of with `gt abort`. The mark is stored as `branch.<name>.gtNeedsRebase` in git config and clears once the branch restacks cleanly.

When someone else pushes to your branch, even with a force push, sync replaces your local commit with theirs as long as yours has no edits the remote never had. To tell, gt records the remote commit it last pushed or pulled as `branch.<name>.gtSynced` in git config. If your local commit is that commit, or a restack of it (the same `git patch-id`), it takes the remote. For a branch gt has no record of, it asks GitHub for every commit the PR's head has pointed at, force pushes included, and takes the remote if your commit is one of them or a restack of one. This works even if another `git fetch` already brought the new commit in. If both sides changed, sync keeps yours and warns. `gt submit` refuses to push a branch whose remote holds a commit gt never synced with, so it can't overwrite someone else's push. Run `gt sync` first.

`gt sync` flags:

- `-a, --all`: sync every stack, not just the current one
- `--no-prune`: keep local branches of merged PRs, review branches included
- `--prefer-remote`: when a branch changed both locally and on the remote, take the remote (the default keeps local and warns)
- `--offline`: skip GitHub API calls and only run git fetch (merged review branches aren't pruned, and branches with no `gtSynced` record can't be checked against GitHub's push history)

### Local branches

A branch is either **shared**, kept in `.git/gh-stack` where `gh stack` sees it, or **local**, kept in gt's `.git/gt-stack`. `gt create` and `gt track` make local branches; `gt get` and `gt submit` make shared ones. gt shows both as one stack: with `A` submitted and `B` and `C` created on top of it, `gt ls` draws `A ← B ← C`, and restack, sync and navigation treat them as one stack. Only shared branches are pushed, get PRs, and join the GitHub stack.

`gt submit` on `B` shares `A` and `B` (opening both PRs and stacking them on GitHub) and leaves `C` local on top. A local branch that's pushed but has no PR stays local.

When `gt sync` finds that a local branch has a PR (opened some other way), it shares the branch if the PR's base is the branch below it and GitHub already stacks the two PRs. If GitHub doesn't stack them, sync asks whether to stack them there; say no and it won't ask again for that PR (`branch.<name>.gtNoLink`), though `gt submit` from the branch still stacks it. Without a terminal it only prints a hint. A PR based on some other branch stays local, with a warning. Only the lowest local branches can be shared this way, since gh-stack can't skip one.

Each `.git/gt-stack` entry is either a whole stack on a trunk, or local branches on top of a gh-stack stack (`on` names the branch below them). If `gh stack add` later adds one of those branches to gh-stack, gh-stack's copy wins.

### Cleaning up

| Command | What it does |
| --- | --- |
| `gt delete [target]` (`dl`) | Delete a branch and everything above it in its stack, locally only. Opens a picker if you give no target. `-f` skips the confirmation. |
| `gt clear-merged` (`cm`) | Delete every local branch whose PR merged, but only if the branch is exactly what merged. `-D` also deletes branches whose local commit differs from what merged. `-f` skips the confirmation. |

### Reviewing others' PRs

```sh
gt review            # pick from PRs where you're a requested reviewer (directly, not via a team), stacks grouped first
gt review 1234       # or by PR number, URL or branch name
```

The picker lists stacked PRs first, each stack top-down above its base branch, then the other PRs. A PR counts as stacked when its base is another listed PR's branch or anything but trunk.

`gt review` (`rv`) checks out a PR read-only and marks it with `branch.<name>.gtReview` in git config. Review branches appear below your stacks in `gt log` and `gt ls`. `gt sync` moves them to the PR head, even after a force push, and drops any local commits on them. It never restacks or pushes them, and it deletes them once their PR merges.

When the PR is stacked, `gt review` also pulls down and marks the rest of its stack: the PRs below it, nearest first, then every PR stacked above it (all branches, if several share a parent). Each walk stops at trunk, or at the first PR that is a draft, closed, from a fork, or a branch you already have locally that isn't a review branch (so your own work is never reset).

The Reviewing section of `gt log` and `gt ls` draws each reviewed stack on its own, top first, above the branch it sits on, with standalone PRs listed after the stacks. `gt review` records each PR's base in `branch.<name>.gtReviewBase` so `gt ls` can do this offline, and `gt log` refreshes it whenever GitHub reports a different base.

### Following your cloud agents' PRs

```sh
gt cloud             # pick from your open PRs that have no local branch, stacks grouped first, each with when it was opened
gt cloud 1234        # or by PR number, URL or branch name
```

`gt cloud` is `gt get` for PRs you authored that a cloud agent opened and keeps updating, so you can edit and submit them like any of your branches, and `gt sync` takes the agent's pushes while you haven't changed the branch. The list is your own PRs, and a PR drops off it once it has a local branch, so it won't reappear after you've pulled it down. A PR in a GitHub stack brings that stack, as `gt get` does. Any other PR is tracked even though GitHub doesn't stack it: alone on trunk, or with the PRs its base chains it to. That's the PRs below it down to trunk, then the PRs above it as long as exactly one sits on the last, drafts included, since agents often leave their work in draft. It won't track a PR from a fork; `gt review` checks one out read-only. A branch you'd already pulled down with `gt review` stops being a review branch.

## How `gt loc` classifies files

Each changed file is counted as one of:

- **gen**: lockfiles, `.svg`, `.map`, `.snap`, minified assets, protobuf output, anything under `generated/`, `vendor/` or `node_modules/`, and files with a "generated" header in their first lines
- **test**: `test_*.py`, `*_test.{py,go,rs}`, `*.test.*`, `*.spec.*`, `conftest.py`, anything under `tests/`, `test/`, `__tests__/`, `e2e/` and similar
- **doc**: `.md`, `.mdx`, `.rst`, `.adoc`, `.txt`, README/CHANGELOG/LICENSE/CONTRIBUTING, anything under `docs/`
- **src**: everything else

`linguist-generated`, `linguist-vendored` and `linguist-documentation` attributes in `.gitattributes` take priority. Inside Rust files, `#[cfg(test)]` items count as test (this assumes rustfmt'd code). Because a commit's diff never changes, results are cached forever under `<git-common-dir>/gt/loc-v<N>/<sha>.json` and shared across worktrees.

## Run log

Every run appends to `~/.local/state/gt/log/<date>.jsonl` (`$XDG_STATE_HOME` is honored, and `GT_LOG_DIR` overrides the directory), one JSON object per line, all tagged with the run's id: the command line and directory, the repo and branch, each git call (args, exit code, time, stderr) and GitHub call (endpoint or query, status, time), each info/warning/error line, what was picked or confirmed, and how the run ended. Stdout is kept as a line count, a hash and its first 20 lines. A run with a `start` but no `end` was killed. Logs older than 14 days are deleted. `GT_LOG=0` turns it off.

```sh
jq -c 'select(.ev == "end" and .ms > 2000)' ~/.local/state/gt/log/*.jsonl   # slow runs
jq -r 'select(.ev == "warn") | .msg' ~/.local/state/gt/log/*.jsonl | sort | uniq -c | sort -rn
```

## Development

```sh
cargo build
cargo test
cargo fmt      # max_width = 120, see rustfmt.toml
```

| File | Contents |
| --- | --- |
| `src/main.rs` | CLI definition (clap) |
| `src/commands.rs` | Every subcommand |
| `src/state.rs` | `.git/gh-stack` and `.git/gt-stack` read/write and locking |
| `src/ctx.rs` | Repo context: refs, worktrees, review branches, stack store |
| `src/restack.rs` | In-memory restack and squash |
| `src/git.rs` | Git plumbing (gix plus the `git` binary) |
| `src/github.rs` | GraphQL client |
| `src/loc.rs` | Diff line counting and classification |
| `src/history.rs` | Version history from the reflog, interdiffs and revert planning |
| `src/picker.rs` | Interactive picker |
| `src/ui.rs` | Terminal colors and messages |
