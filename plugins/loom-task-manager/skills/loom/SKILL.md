---
name: loom
description: Work loom-managed tasks in this repo — pick, lease, execute, verify, and complete tasks tracked in .work/. Use when the user asks to work the task graph, pick up the next task, run the fleet loop, or when .work/tasks/ exists and a task needs picking up, leasing, attempting, verifying, or closing out.
---

# Working with loom

loom coordinates task execution through git refs, not through this conversation's memory.
Another agent, or another session of you, may be working the same graph concurrently.
Never infer task state from what you remember doing — always re-derive it from `loom` commands, since the canonical graph lives on `<remote>/main` and leases/attempts/verdicts live in `refs/loom/*`.

Set `LOOM_AGENT` to a stable identity for this session before running anything (e.g. `export LOOM_AGENT=<tool>-<short-id>`), so leases, attempts, and verdicts are attributable and so self-verdicts are correctly detected. If unset, loom falls back to `git config user.email`, which collapses every agent under one human's identity and breaks the independent-verdict gate.

## The loop

1. **Pick.** `loom next` prints the single best schedulable task (highest score: value × unblocks × staleness ÷ tier). Use `loom next --all` to see every candidate if the top pick doesn't fit the current session's scope.
2. **Lease.** `loom lease <id>` acquires the exclusive lock via CAS.
   - Exit 3 (lease race lost): another agent won it — do not retry the same task, call `loom next` again.
   - Exit 4 (oracle blocked): an open escalation blocks this task with no default — read it via `loom escalations` and stop; a human must answer it.
3. **Heartbeat.** Immediately run `loom heartbeat <id> --daemon`. This detaches a background process that keeps the lease alive and self-stops when the tier's budget minutes elapse — the timebox is enforced by loom, not by remembering to check a clock.
4. **Read before writing.** `loom show <id>` for prior attempts and their lessons — read every lesson before repeating work that already failed. `loom context <id>` for the hydration manifest (exact files to read, nothing more).
5. **Probe if needed.** If `loom show <id>` reports no acceptance tests (`needs_probe`), the task isn't executable yet. Write the failing acceptance test(s) that define "done," then `loom probe-done <id> --accept <path>... [--context <path>...]`. Commit **and push** the failing tests + task file before continuing — `probe-done` only writes locally, and other agents' `loom next`/`show`/`context` read the canonical graph on `<remote>/main`, not your worktree.
6. **Do the work**, scoped to the tier's budget (`loom status <id>` shows elapsed/remaining minutes).
7. **On failure**, don't grind past the budget: `loom attempt <id> --sha <candidate-sha> --outcome <tests-red|...> --lesson "<what to try differently next time>"`. This escalates the derived tier and releases the lease.
   - Exit 2 (tier exhausted): the ladder is done. Decompose the task into smaller `loom task-create` entries, or `loom escalate` — do not attempt again at the same scope. `task-create` also only writes locally: commit and push `.work/tasks/` before expecting the new task to show up in `loom next`.
8. **On success, get an independent verdict.** Someone other than the implementer runs `loom verify <id> --approve` (or `--reject --lesson "..."`) against the candidate sha. If your tool offers a separate reviewer subagent, delegate the verdict to it so the reviewer identity genuinely differs; the loom Claude Code plugin ships `loom-task-manager:verifier` for exactly this. If you are both implementer and the only reviewer available, say so explicitly rather than self-approving — a self-verdict is recorded as such and is blocked by default policy (`verify.mode = independent`).
9. **Close it out.** `loom done <id>` gates on that verdict, flips `state=done`, and commits the state flip with the implementation atomically — then `git push`. The commit is local-only; until it's pushed, the task still shows as its prior derived state (e.g. `parked`) to everyone reading the canonical graph.
   - Exit 5 (verification gate): no valid independent verdict bound to HEAD yet — get one before retrying.

## When to escalate instead of guessing

File `loom escalate --question "..." --option a --option b --recommend a [--evidence ...] [--blocking <id>] [--deadline <iso8601> --default <option>]` when a decision is a genuine judgment call outside the task's spec — not for questions the acceptance tests already answer. Always pass `--recommend`; state your best guess even when asking. Only pass `--default` when it's truly safe to auto-apply after the deadline unattended.

## Housekeeping

- Run `loom doctor` at the start of a session in an unfamiliar repo to confirm the remote, canonical graph, dep integrity, and policy are sane before trusting `loom next`.
- If a task looks stuck under a lease that's gone quiet, don't force anything manually — `loom sweep` reclaims leases past the policy TTL and logs the reclaim as a fleet-visible attempt.
- Never hand-edit `.work/tasks/*.toml` state or `refs/loom/*` directly. Every mutation goes through a `loom` subcommand so the CAS/atomicity guarantees hold.
