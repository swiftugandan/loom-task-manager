#!/usr/bin/env bash
#
# Cases for the PreToolUse guard in plugins/loom-task-manager/hooks/scripts/guard-loom-state.sh.
#
# The allow cases matter more than the deny cases: a guard that blocks `git push`
# or reading a task file makes the plugin unusable, so every ordinary fleet command
# is pinned here.

set -uo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
guard="$repo_root/plugins/loom-task-manager/hooks/scripts/guard-loom-state.sh"
failures=0

expect() { # name, payload, deny|allow
  local name=$1 payload=$2 want=$3 out got
  out=$(printf '%s' "$payload" | bash "$guard" 2>&1)
  if printf '%s' "$out" | grep -q '"deny"'; then got=deny; else got=allow; fi
  if [ "$got" = "$want" ]; then
    printf 'ok   %s\n' "$name"
  else
    printf 'FAIL %s: expected %s, got %s\n%s\n' "$name" "$want" "$got" "$out" >&2
    failures=$((failures + 1))
  fi
}

# Writes that bypass loom's guarantees.
expect "edit a task file"          '{"tool_name":"Edit","tool_input":{"file_path":"/repo/.work/tasks/a1b2.toml"}}' deny
expect "write a task file"         '{"tool_name":"Write","tool_input":{"file_path":".work/tasks/new.toml"}}' deny
expect "update-ref a lease"        '{"tool_name":"Bash","tool_input":{"command":"git update-ref refs/loom/lease/a1b2 HEAD"}}' deny
expect "push a loom ref"           '{"tool_name":"Bash","tool_input":{"command":"git push origin refs/loom/lease/x"}}' deny
expect "delete a loom ref"         '{"tool_name":"Bash","tool_input":{"command":"git update-ref -d refs/loom/lease/x"}}' deny
expect "rm a task file"            '{"tool_name":"Bash","tool_input":{"command":"rm .work/tasks/a1b2.toml"}}' deny
expect "sed -i a task file"        '{"tool_name":"Bash","tool_input":{"command":"sed -i s/parked/done/ .work/tasks/a1b2.toml"}}' deny
expect "heredoc into a task file"  '{"tool_name":"Bash","tool_input":{"command":"cat > .work/tasks/x.toml <<EOF"}}' deny

# Ordinary fleet work, which must pass through untouched.
expect "read a task file"          '{"tool_name":"Read","tool_input":{"file_path":".work/tasks/a1b2.toml"}}' allow
expect "cat a task file"           '{"tool_name":"Bash","tool_input":{"command":"cat .work/tasks/a1b2.toml"}}' allow
expect "grep the tasks dir"        '{"tool_name":"Bash","tool_input":{"command":"grep -r goal .work/tasks/"}}' allow
expect "list loom refs"            '{"tool_name":"Bash","tool_input":{"command":"git for-each-ref refs/loom/"}}' allow
expect "log a loom ref"            '{"tool_name":"Bash","tool_input":{"command":"git log refs/loom/verdict/x"}}' allow
expect "plain git push"            '{"tool_name":"Bash","tool_input":{"command":"git push"}}' allow
expect "push a branch"             '{"tool_name":"Bash","tool_input":{"command":"git push origin main"}}' allow
expect "loom done then push"       '{"tool_name":"Bash","tool_input":{"command":"loom done a1b2 && git push"}}' allow
expect "loom lease with identity"  '{"tool_name":"Bash","tool_input":{"command":"LOOM_AGENT=claude-1 loom lease a1b2"}}' allow
expect "edit the policy"           '{"tool_name":"Edit","tool_input":{"file_path":".work/policy.toml"}}' allow
expect "edit source"               '{"tool_name":"Edit","tool_input":{"file_path":"src/main.rs"}}' allow
expect "clean the build dir"       '{"tool_name":"Bash","tool_input":{"command":"rm -rf target/"}}' allow

# Malformed input fails open rather than breaking the session.
expect "empty payload"             '' allow
expect "payload without a tool"    '{"hook_event_name":"PreToolUse"}' allow

if [ "$failures" -ne 0 ]; then
  printf '\n%d hook guard case(s) failed\n' "$failures" >&2
  exit 1
fi

printf '\nall hook guard cases passed\n'
