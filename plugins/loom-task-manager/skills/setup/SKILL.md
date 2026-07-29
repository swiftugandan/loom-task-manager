---
name: setup
description: Bootstrap loom in a repository - install the binary, run `loom init`, tune `.work/policy.toml`, push the canonical graph, and seed the first tasks. Use when a repo has no `.work/` directory yet and the user wants to adopt loom, start a fleet, or set up agent task coordination.
---

# Setting loom up in a repo

Use this once per repository, before the [loom](../loom/SKILL.md) work loop applies.
Everything below happens in the user's repo, not in loom's own source tree.

## 1. Confirm the binary

Run `loom --version`.
If the command is missing, loom is not published to crates.io, so install it straight from the source repo:

```sh
cargo install --git https://github.com/swiftugandan/loom-task-manager
```

This needs Rust 1.75+ and puts the `loom` binary in `~/.cargo/bin`.
If that directory is off `PATH`, tell the user rather than editing their shell profile for them.

## 2. Confirm the substrate

loom coordinates through a git remote, so the repo needs one before anything else works.
Check `git remote -v` and `git rev-parse --abbrev-ref HEAD`.

The canonical graph is read from `<remote>/<main>`, defaulting to `origin/main`.
When the repo uses different names, set `LOOM_REMOTE` and `LOOM_MAIN` and mention that every agent in the fleet needs the same two values.

## 3. Initialize

```sh
loom init
```

This creates `.work/tasks/`, `.work/escalations/`, `.work/policy.toml`, and `knowledge/`.

## 4. Tune the policy before the first push

Read `.work/policy.toml` with the user and settle three decisions while the graph is still empty:

- `budget.tiers_minutes` - the timebox ladder. A task escalates one tier per failed attempt, and exhausting the ladder forces decomposition or escalation rather than grinding.
- `lease.ttl_minutes` and `lease.heartbeat_minutes` - how long a silent agent holds a task before `loom sweep` reclaims it.
- `verify.mode` - `independent` (default) requires an approving verdict from an agent other than the implementer. Keep it unless the user is running a single agent, in which case `any` records self-verdicts for audit instead of blocking them.

## 5. Publish the graph

```sh
git add .work knowledge && git commit -m "loom: init" && git push
```

The push is the step that matters.
`loom next`, `tasks`, `show`, and `context` all read `<remote>/<main>`, so an unpushed `.work/` is invisible to every agent including this one.

## 6. Verify

```sh
loom doctor
```

This checks the repo, the remote, the canonical graph, dependency integrity, and the policy.
Resolve anything it reports before creating tasks.

## 7. Seed the first tasks

```sh
loom task-create --goal "<outcome, not activity>" --value <1-5>
git add .work && git commit -m "task: <goal>" && git push
```

Write goals as observable outcomes, since a task's acceptance tests are what eventually prove it done.
Each new task reports `needs_probe: true`, meaning the work loop starts by writing the failing acceptance tests that define done.

## 8. Hand over to the fleet

Tell the user two things before finishing:

- Every agent needs a distinct `LOOM_AGENT` value, for example `export LOOM_AGENT=claude-$(uuidgen | head -c 8)`. Without it agents collapse onto `git config user.email` and the independent-verdict gate cannot tell them apart.
- From here the [loom](../loom/SKILL.md) skill drives the loop: `next`, `lease`, `heartbeat --daemon`, work, then `verify` and `done`.
