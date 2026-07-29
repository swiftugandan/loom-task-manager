# knowledge/

Compounding institutional memory. One small fact file per topic;
reference these files from task context manifests.

## The shape of a file

- `<topic>.md`, named for the thing an agent would search for, one topic wide.
- State the fact, then how to act on it. A fact nobody can act on is a note, not knowledge.
- Append to an existing topic file when one already covers the ground.
- Write it before `loom done`, so it lands in the same atomic commit as the work that taught it.
- Cite it from related tasks via `--context` on `loom task-create` and `loom probe-done`.

`loom retro` proposes candidates for this directory under `knowledge_candidates`,
derived from lessons that recurred across distinct tasks.
