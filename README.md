# gt

A fast, Graphite-style CLI for stacked PRs, built on `gh stack`'s state and GitHub's stacks API.

`gt` reads and writes the same `.git/gh-stack` file that `gh stack` uses (unknown fields are kept, and it takes the same lock files), so you can switch between the two tools freely. It restacks in memory with `git merge-tree` + `git commit-tree`, so restacking never checks out a branch or touches a working tree, and it batches GitHub calls into single GraphQL round trips.

**It assumes one commit per branch.** That rule keeps restacking simple: a branch's old base is always its tip's parent. `gt squash` folds a branch with several commits back into one.

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
gt sync                                 # fetch, prune merged, restack onto trunk, push
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
| `gt create [name] -m <msg>` (`c`) | Create a branch above the current one and commit staged changes to it. `-a` stages everything first. The name comes from the message if you leave it out. |
| `gt modify` (`m`) | Amend the current branch's commit (or make the first one if the branch has none), then restack everything above it. `-a` stages everything, `-m` replaces the message. |
| `gt restack` (`r`) | Rebase each branch onto its parent, in memory. `--trunk` also moves the bottom branch onto the latest remote trunk. |
| `gt squash` | Fold each branch in the stack down to one commit. The content stays the same. |
| `gt continue` | Carry on after you resolve a restack conflict (finishes a `git rebase` in progress first). |

### Syncing with GitHub

| Command | What it does |
| --- | --- |
| `gt submit` (`ss`) | Restack, push, create PRs (title and body from the commit message) or retarget them onto the right base, and link the stack on GitHub. `--draft` opens new PRs as drafts. `--relink` replaces the GitHub stack if it doesn't match the local order. |
| `gt sync` (`s`) | Fetch, mark merged PRs, pull in commits others pushed to your branches, fast-forward local trunks (including ones checked out in other worktrees), restack onto trunk, and push branches that already exist on the remote. See flags below. |
| `gt get <target>` | Pull down a stack from GitHub by PR number, URL or branch. |
| `gt track [branches…]` | Start tracking existing branches (bottom first) as a stack. Defaults to the current branch. `--trunk` sets the trunk. |

`gt sync` flags:

- `-a, --all`: sync every stack, not just the current one
- `--no-push`: don't push
- `--no-prune`: keep local branches of merged PRs
- `--prefer-remote`: when a branch changed both locally and on the remote, take the remote (the default keeps local and warns)
- `--offline`: skip GitHub API calls and only run git fetch/push
- `--no-verify`: skip pre-push hooks

### Cleaning up

| Command | What it does |
| --- | --- |
| `gt delete [target]` (`dl`) | Delete a branch and everything above it in its stack, locally only. Opens a picker if you give no target. `-f` skips the confirmation. |
| `gt clear-merged` (`cm`) | Delete every local branch whose PR merged, but only if the branch is exactly what merged. `-D` also deletes branches whose local commit differs from what merged. `-f` skips the confirmation. |

### Reviewing others' PRs

```sh
gt review            # pick from PRs where you're a requested reviewer (directly, not via a team)
gt review 1234       # or by PR number, URL or branch name
```

`gt review` (`rv`) checks out a PR read-only and marks it with `branch.<name>.gtReview` in git config. Review branches appear below your stacks in `gt log` and `gt ls`. `gt sync` moves them to the PR head, even after a force push, and drops any local commits on them. It never restacks or pushes them.

## How `gt loc` classifies files

Each changed file is counted as one of:

- **gen**: lockfiles, `.svg`, `.map`, `.snap`, minified assets, protobuf output, anything under `generated/`, `vendor/` or `node_modules/`, and files with a "generated" header in their first lines
- **test**: `test_*.py`, `*_test.{py,go,rs}`, `*.test.*`, `*.spec.*`, `conftest.py`, anything under `tests/`, `test/`, `__tests__/`, `e2e/` and similar
- **doc**: `.md`, `.mdx`, `.rst`, `.adoc`, `.txt`, README/CHANGELOG/LICENSE/CONTRIBUTING, anything under `docs/`
- **src**: everything else

`linguist-generated`, `linguist-vendored` and `linguist-documentation` attributes in `.gitattributes` take priority. Inside Rust files, `#[cfg(test)]` items count as test (this assumes rustfmt'd code). Because a commit's diff never changes, results are cached forever under `<git-common-dir>/gt/loc-v<N>/<sha>.json` and shared across worktrees.

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
| `src/state.rs` | `.git/gh-stack` read/write and locking |
| `src/ctx.rs` | Repo context: refs, worktrees, review branches, stack store |
| `src/restack.rs` | In-memory restack and squash |
| `src/git.rs` | Git plumbing (gix plus the `git` binary) |
| `src/github.rs` | GraphQL client |
| `src/loc.rs` | Diff line counting and classification |
| `src/picker.rs` | Interactive picker |
| `src/ui.rs` | Terminal colors and messages |
