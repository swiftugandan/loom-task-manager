#!/usr/bin/env bash
#
# The loom work-loop skill ships to three discovery paths:
#
#   plugins/loom-task-manager/skills/loom/SKILL.md   canonical - the Claude Code plugin
#   .claude/skills/loom/SKILL.md                     Claude Code, working in this repo
#   .github/skills/loom/SKILL.md                     GitHub Copilot, working in this repo
#
# The plugin copy is the source of truth. This script regenerates the other two,
# or verifies they match with --check (use that in CI and pre-commit).

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
canonical="$repo_root/plugins/loom-task-manager/skills/loom/SKILL.md"
targets=(
  "$repo_root/.claude/skills/loom/SKILL.md"
  "$repo_root/.github/skills/loom/SKILL.md"
)

if [ ! -f "$canonical" ]; then
  echo "missing canonical skill: $canonical" >&2
  exit 1
fi

mode=${1:-sync}
status=0

for target in "${targets[@]}"; do
  rel=${target#"$repo_root"/}
  case "$mode" in
    --check)
      if ! diff -q "$canonical" "$target" >/dev/null 2>&1; then
        echo "out of sync: $rel" >&2
        diff -u "$canonical" "$target" >&2 || true
        status=1
      fi
      ;;
    sync)
      mkdir -p "$(dirname "$target")"
      cp "$canonical" "$target"
      echo "synced $rel"
      ;;
    *)
      echo "usage: $(basename "$0") [--check]" >&2
      exit 2
      ;;
  esac
done

if [ "$mode" = "--check" ] && [ "$status" -ne 0 ]; then
  echo "run scripts/sync-agent-skill.sh to regenerate from plugins/loom-task-manager/skills/loom/SKILL.md" >&2
fi

exit "$status"
