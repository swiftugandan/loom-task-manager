# loom

Agent-native task management on a git substrate.

loom coordinates a fleet of coding agents working against a shared task graph, using nothing but a git remote as the coordination medium.
There is no server and no database.
Every piece of mutable fleet state — leases, attempts, verdicts, telemetry — lives in git refs, so concurrent agents can never merge-conflict each other's coordination data.

## Why

Running several autonomous agents against the same repo raises a few hard problems: who owns a task right now, what happens when an agent goes silent mid-task, how much budget an agent gets before it must stop and escalate, and how a "done" claim gets verified without trusting the agent that made it.
loom answers each of these structurally, not by convention:

- **Canonical graph.** Cross-agent decisions (scheduling, dependency resolution) read the task graph from `<remote>/main`, never a possibly-mutated local worktree.
- **CAS leases.** Acquiring a task is an atomic compare-and-swap against a git ref (`git push --force-with-lease`), so two agents can race for the same task and exactly one wins.
- **Conflict-free attempts.** Every attempt is its own uniquely-named ref (`refs/loom/attempt/<task>/<id>`), so fleet activity never produces a merge conflict in `.work/`.
- **Derived tiers, not stored ones.** A task's current budget tier is `initial tier + failed attempts`, capped by the policy's tier ladder. There's no mutable tier field to drift out of sync.
- **Structural timeboxes.** `loom heartbeat --daemon` detaches a background heartbeater that stops itself once the tier budget elapses — the timebox is enforced by the system, not by agent discipline.
- **Independent-verdict done gate.** `loom done` is gated on an approving verdict bound to the exact candidate sha, from an agent other than the implementer (configurable). The state flip and the code/test commit happen atomically; if the commit fails, the state flip is rolled back.
- **Typed human-oracle escalation.** Agents that hit a genuine judgment call file a typed question with options, a recommendation, and (optionally) a deadline default, instead of guessing or stalling silently.
- **Retro loop.** `loom retro` reads back telemetry and attempt history and emits two mechanical outputs: policy suggestions meant to change `.work/policy.toml`, and knowledge candidates — lessons that recurred across distinct tasks, shaped into proposed `knowledge/<topic>.md` files with the source lessons attached. Config learns from the numbers; `knowledge/` learns from the lessons.

## Install

Requires Rust 1.75+. Not published to crates.io — install straight from this repo.

Directly from GitHub, no clone needed:

```sh
cargo install --git https://github.com/swiftugandan/loom-task-manager
```

Or from a local clone:

```sh
git clone https://github.com/swiftugandan/loom-task-manager
cd loom-task-manager
cargo install --path .
```

Either way this builds the `loom` binary from `src/main.rs` and installs it to `~/.cargo/bin` (make sure it's on `PATH`).

## Quickstart

```sh
# In a git repo with a remote:
loom init                       # creates .work/{tasks,escalations,policy.toml} and knowledge/
git add .work knowledge && git commit -m "loom: init" && git push

loom doctor                     # sanity-checks repo, remote, graph, policy, and knowledge/

loom task-create --goal "add rate limiting to the API" --value 3
# -> {"id": "...", "needs_probe": true}
git add .work && git commit -m "task: add rate limiting" && git push
# task-create only writes .work/tasks/ locally — until this is pushed,
# `loom next`/`tasks` (which read origin/main, not the worktree) won't see it

loom next                       # best schedulable task by score
loom lease <id>                 # atomic CAS acquire; exit 3 if another agent won the race
loom heartbeat <id> --daemon    # background heartbeat; self-stops at the tier budget

# task has no acceptance tests yet: probe first
loom probe-done <id> --accept tests/accept/<id>.rs
git add tests .work && git commit -m "probe: <id>" && git push

# ... do the work ...
loom verify <id> --approve      # a different agent publishes a verdict bound to HEAD
loom done <id>                  # gated, atomic: verify → flip state=done → commit
git push                        # done commits locally only — push so the fleet sees it
```

If an attempt fails:

```sh
loom attempt <id> --sha <candidate-sha> --outcome tests-red --lesson "..."
# tier escalates by derivation; exits 2 once the ladder is exhausted
```

If an agent needs a human call:

```sh
loom escalate --question "..." --option a --option b --recommend a \
  --blocking <id> --deadline 2026-08-01T00:00:00Z --default a
```

## Using loom with an agent

loom is meant to be driven by coding agents, not typed by hand. The loop is the same regardless of which agent runs it: `next` → `lease` → `heartbeat --daemon` → do the work → `attempt` (on failure) or `verify` + `done` (on success), reacting to exit codes 2/3/4/5 instead of retrying blindly. Give each agent a distinct `LOOM_AGENT` identity — without it, every agent collapses onto `git config user.email` and the independent-verdict gate can't tell them apart.

This repo ships that loop as an [Agent Skill](https://github.blog/changelog/2025-12-18-github-copilot-now-supports-agent-skills/) - a `SKILL.md` describing when to use loom and how to react to each exit code, in the open format shared by Claude Code and GitHub Copilot.

The canonical copy lives inside the plugin, at [`plugins/loom-task-manager/skills/loom/SKILL.md`](plugins/loom-task-manager/skills/loom/SKILL.md).
`scripts/sync-agent-skill.sh` mirrors it to [`.github/skills/loom/SKILL.md`](.github/skills/loom/SKILL.md), the path GitHub Copilot discovers in this repo (VS Code/JetBrains agent mode, Copilot CLI, and the cloud coding agent).
Edit the canonical copy and regenerate rather than editing the mirror.

To use loom in another project with Copilot, copy the file to `.github/skills/loom/SKILL.md` there, or to `~/.copilot/skills/loom/SKILL.md` to make it available across every repo.
Claude Code reads the canonical copy straight from the installed plugin below, in this repo and every other one, so it needs no copying.

### Claude Code plugin

This repo doubles as a [plugin marketplace](https://code.claude.com/docs/en/plugin-marketplaces), so Claude Code can install the whole workflow in one step:

```
/plugin marketplace add swiftugandan/loom-task-manager
/plugin install loom-task-manager@loom-task-manager
```

That adds four things on top of the bare skill:

| Component | Name | Purpose |
|---|---|---|
| Skill | `loom-task-manager:loom` | The work loop above, keyed to loom's exit codes |
| Skill | `loom-task-manager:setup` | One-time bootstrap for a repo: install, `loom init`, policy decisions, first push, first tasks |
| Agent | `loom-task-manager:verifier` | Independent reviewer that publishes a verdict bound to a candidate sha, under its own `LOOM_AGENT` identity |
| Hook | `PreToolUse` | Denies hand-writes to `.work/tasks/*.toml` and direct `refs/loom/*` ref writes, naming the owning subcommand instead |

The last two are the point. `verify.mode = independent` needs a reviewer that is genuinely a different agent, and every loom guarantee holds only while mutations flow through a `loom` subcommand — so the plugin supplies a real second agent and a guard that makes the bypass impossible, rather than asking one agent to remember both rules. See [`plugins/loom-task-manager/README.md`](plugins/loom-task-manager/README.md) for details.

## Commands

| Command | Purpose |
|---|---|
| `init` | Initialize `.work/` (tasks, escalations, policy) and `knowledge/` |
| `doctor` | Verify git repo, remote, canonical graph, dep integrity, policy, and `knowledge/` health |
| `task-create` | Create a task file in `.work/tasks/` (prints its id) |
| `tasks` | List all tasks with derived state and derived tier |
| `show <id>` | Show one task: derived state/tier, attempts with lessons, unblock count |
| `next` | Print the best schedulable task (or all candidates with `--all`) |
| `lease <id>` | Acquire the exclusive work lease |
| `heartbeat <id>` | Heartbeat the lease; `--daemon` detaches a self-stopping background heartbeater |
| `status <id>` | Lease clock: elapsed vs. budget, remaining minutes, current verdict |
| `release <id>` | Release the lease without finishing (work stays on the branch) |
| `probe-done <id>` | Record a probe's output: acceptance tests + tightened context manifest |
| `attempt <id>` | Record a failed attempt as a conflict-free ref |
| `verify <id>` | Publish a verdict for the candidate at HEAD (or `--sha`) |
| `done <id>` | Gate + flip `state=done` + commit code, tests, and state atomically |
| `dead <id>` | Mark a task dead with a reason |
| `escalate` | File a typed question for the human oracle |
| `escalations` | List escalations; `--apply-defaults` answers past-deadline ones |
| `sweep` | Reclaim stale leases |
| `lock acquire` / `lock release` | Take/release the integration serialization lock |
| `telemetry <commit>` | Append a structured telemetry record (git note) to a commit |
| `retro` | Aggregate telemetry + attempt history into a report with policy suggestions and `knowledge/` file candidates |
| `context <id>` | Print a task's hydration manifest (context files with existence/size, plus a standing `knowledge/` index) |

Run `loom <command> --help` for full flag details.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Error (protocol violation, git failure, bad input) |
| 2 | Budget tier exhausted — decompose or escalate; grinding is not an option |
| 3 | Lease race lost — pick another task |
| 4 | Blocked on the human oracle (open escalation with no default) |
| 5 | Verification gate — no valid independent verdict for this candidate |

## Environment

| Variable | Purpose | Default |
|---|---|---|
| `LOOM_AGENT` | Agent identity for leases/verdicts/telemetry | `git config user.email` |
| `LOOM_REMOTE` | Git remote for coordination refs | `origin` |
| `LOOM_MAIN` | Canonical branch the graph is read from | `main` |

## Policy

`.work/policy.toml` controls budgets, lease TTLs, scheduling, and the verification gate:

```toml
[budget]
tiers_minutes = [10, 45, 180]

[lease]
ttl_minutes = 15
heartbeat_minutes = 5

[schedule]
staleness_half_life_hours = 72.0

[verify]
mode = "independent"  # "independent" | "any" | "off"
```

`verify.mode`:
- `independent` — an approving verdict from a different agent, bound to the exact candidate sha, is required. This is the default: review discipline is structural, not instructional.
- `any` — a verdict is required but self-verdicts are allowed (recorded as such for audit).
- `off` — no verdict gate (tests-green is still required).

## How coordination works

All fleet-mutable state lives under `refs/loom/` on the remote, with payloads as JSON in commit messages over the empty tree:

- `refs/loom/lease/<task>` — the exclusive work lease, created/updated/deleted via `git push --force-with-lease` (a server-side compare-and-swap).
- `refs/loom/attempt/<task>/<id>` — one ref per attempt, uniquely named, append-only, and therefore conflict-free by construction.
- `refs/loom/verdict/<task>` — the latest verdict, bound to the candidate sha it approves.
- `refs/loom/merge-lock` — the integration serialization lock.
- `refs/notes/loom-telemetry` — structured telemetry as git notes.

Task identity, spec, and the two single-writer-under-lease mutations (probe output, the done flip) live in `.work/tasks/*.toml` instead, since those files are near-immutable and change under an exclusive lease.

## Refining a task's spec

`deps`, `goal`, `value`, and `contract` lock in at `task-create` and stay fixed for the task's life.
Only `probe-done` and `done` write to an existing task file afterward, and each touches a narrow field (`accept`/`context`, and `state`) under the exclusive lease.
This keeps a scheduling decision or a verdict bound to the spec it actually saw, even as other agents work the graph concurrently.

Two paths follow from that:

- **Deps, goal, and value.** Settle these before `task-create` runs — draft the decomposition as plain notes first, and treat `task-create` as the commit point.
For a correction after creation, `loom dead <id> --reason "..."` and recreate under a fresh id.
This is cheap before any lease or attempt exists, since there's no history yet to carry over.
One thing to watch: a derived state only clears a dep once it reaches `done`, not `dead`, so recreate any task whose `--dep` already points at the old id too.
- **Acceptance criteria.** `probe-done <id> --accept <path>... [--context <path>...]` carries no lease or state check, so it's a genuine do-over — call it again whenever the test surface needs tightening.

## Development

```sh
cargo build
cargo test
```

### Pre-commit hook

Hooks live in [`.githooks/`](.githooks) and are versioned with the repo. Enable them once per clone:

```sh
git config core.hooksPath .githooks
```

`pre-commit` then gates every commit on five checks: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, the plugin hook-guard cases, and `scripts/sync-agent-skill.sh --check`.

It runs them against a clean checkout of the index rather than the working tree, so what gets verified is exactly what gets committed - staging a broken file and fixing it locally afterwards still fails. The checkout shares `target/`, so the whole gate takes a few seconds warm. Bypass with `git commit --no-verify`.
