# loom plugin for Claude Code

Packages the loom fleet workflow as an installable plugin, so any repo running loom gets the same loop, the same reviewer discipline, and the same guardrails without copying files around.

```
/plugin marketplace add swiftugandan/loom-task-manager
/plugin install loom-task-manager@loom-task-manager
```

The plugin drives the `loom` binary, which it expects on `PATH`.
Install it with `cargo install --git https://github.com/swiftugandan/loom-task-manager`, or let the `setup` skill walk through it.

## What it ships

| Component | Name | Purpose |
|---|---|---|
| Skill | `loom-task-manager:loom` | The work loop: pick, lease, heartbeat, probe, attempt, verify, done, keyed to loom's exit codes |
| Skill | `loom-task-manager:setup` | One-time bootstrap for a repo: install, `loom init`, policy decisions, first push, first tasks |
| Agent | `loom-task-manager:verifier` | Independent reviewer that publishes a verdict bound to a candidate sha, under its own agent identity |
| Hook | `PreToolUse` | Denies hand-writes to `.work/tasks/*.toml` and direct `refs/loom/*` ref writes |

## Why an agent, not an instruction

loom's default policy (`verify.mode = independent`) accepts a `done` only when the approving verdict comes from an agent other than the implementer.
Telling one agent to review its own work satisfies the letter of that and none of the intent, and loom records the self-verdict as such.

`loom-task-manager:verifier` runs as a separate agent with a separate `LOOM_AGENT` identity and no write tools, so it can reject a candidate but cannot quietly repair one.
Delegate to it once the implementation is complete, then run `loom done` against the sha it approved.

## Why a hook, not a warning

Every loom guarantee rests on mutations flowing through a `loom` subcommand.
Leases are compare-and-swap pushes, attempts are uniquely named refs, and the `done` flip commits state with the implementation atomically.
A hand-edited `.work/tasks/*.toml` or a direct `git update-ref refs/loom/...` skips all of that and desyncs the fleet in a way the next agent reads as truth.

The `PreToolUse` guard turns that from a rule agents are asked to remember into one they cannot break, and names the owning subcommand in the denial message.
Reads pass through untouched, `git push` on ordinary branches passes through untouched, and malformed hook input fails open.
The cases are pinned in [`scripts/test-plugin-hooks.sh`](../../scripts/test-plugin-hooks.sh) at the repo root.

## Development

The work-loop skill is published to two discovery paths, with this plugin as the source of truth:

- `plugins/loom-task-manager/skills/loom/SKILL.md` - canonical, and what Claude Code loads through the installed plugin
- `.github/skills/loom/SKILL.md` - GitHub Copilot working in the loom repo itself

Edit the canonical copy, then regenerate and verify:

```sh
scripts/sync-agent-skill.sh          # regenerate the Copilot mirror
scripts/sync-agent-skill.sh --check  # verify they match, for CI
scripts/test-plugin-hooks.sh         # exercise the PreToolUse guard
claude plugin validate ./plugins/loom-task-manager --strict
claude plugin validate . --strict    # the marketplace catalog
```
