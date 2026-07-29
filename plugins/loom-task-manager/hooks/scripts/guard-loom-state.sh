#!/usr/bin/env bash
#
# PreToolUse guard for loom's coordination state.
#
# loom's guarantees (CAS leases, conflict-free attempts, the atomic done flip)
# hold only while every mutation goes through a `loom` subcommand. A hand-written
# edit to `.work/tasks/*.toml` or a direct `refs/loom/*` ref write bypasses the
# compare-and-swap and can silently desync the fleet.
#
# This hook denies those two classes of write and names the right command instead.
# Reads are always allowed. Anything unexpected fails open: a guard that breaks a
# session is worse than no guard.

set -uo pipefail

payload=$(cat)
[ -n "$payload" ] || exit 0
command -v jq >/dev/null 2>&1 || exit 0

jq_get() {
  printf '%s' "$payload" | jq -r "$1" 2>/dev/null
}

deny() {
  jq -n --arg reason "$1" '{
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      permissionDecision: "deny",
      permissionDecisionReason: $reason
    }
  }'
  exit 0
}

tool=$(jq_get '.tool_name // empty')
[ -n "$tool" ] || exit 0

case "$tool" in
  Edit | Write | MultiEdit | NotebookEdit)
    path=$(jq_get '.tool_input.file_path // .tool_input.notebook_path // empty')
    [ -n "$path" ] || exit 0
    case "$path" in
      *.work/tasks/*.toml)
        deny "Blocked by the loom plugin: $path is loom-managed state.

Task files are written by loom subcommands so the single-writer-under-lease
guarantee holds. Use the command that owns the field you are changing:
  new task            -> loom task-create --goal \"...\" --value <n>
  acceptance tests    -> loom probe-done <id> --accept <path>...
  state=done          -> loom done <id>
  state=dead          -> loom dead <id> --reason \"...\"
  failed attempt      -> loom attempt <id> --sha <sha> --outcome <kind> --lesson \"...\"

Run \`loom show <id>\` first if you are unsure which field is derived rather than stored."
        ;;
    esac
    ;;

  Bash)
    cmd=$(jq_get '.tool_input.command // empty')
    [ -n "$cmd" ] || exit 0

    # Direct writes to refs/loom/* bypass the compare-and-swap that makes leases safe.
    if printf '%s' "$cmd" | grep -q 'refs/loom' &&
      printf '%s' "$cmd" | grep -Eq '(^|[[:space:]])(update-ref|symbolic-ref|push)([[:space:]]|$)|[[:space:]]-[dD]([[:space:]]|$)'; then
      deny "Blocked by the loom plugin: this writes refs/loom/* directly.

Fleet-mutable state lives behind git push --force-with-lease so two agents racing
for the same task cannot both win. A direct ref write skips that compare-and-swap.
Use the owning subcommand instead:
  lease/release       -> loom lease <id> / loom release <id>
  keep a lease alive  -> loom heartbeat <id> --daemon
  reclaim a dead one  -> loom sweep
  attempts, verdicts  -> loom attempt <id> ... / loom verify <id> --approve|--reject
  merge lock          -> loom lock acquire / loom lock release

Reading refs/loom (git log, git show, git for-each-ref) is fine."
    fi

    # Shell-level mutation of task files, which the Edit/Write branch would otherwise catch.
    if printf '%s' "$cmd" | grep -q '\.work/tasks' &&
      printf '%s' "$cmd" | grep -Eq '(^|[[:space:]|;&])(rm|mv|truncate|tee)([[:space:]]|$)|sed[[:space:]]+-i|>[[:space:]]*[^[:space:]]*\.work/tasks'; then
      deny "Blocked by the loom plugin: this mutates .work/tasks/ outside loom.

Task files are written by loom subcommands (task-create, probe-done, done, dead,
attempt) so the graph other agents read from <remote>/main stays consistent.
Reading .work/tasks/ is fine; use \`loom tasks\` or \`loom show <id>\` for derived state."
    fi
    ;;
esac

exit 0
