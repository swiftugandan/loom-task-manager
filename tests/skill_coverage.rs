//! Work-loop skill coverage gate.
//!
//! The skill is what an agent reads instead of the source, so a capability the
//! binary ships and the skill never mentions is a capability the fleet doesn't
//! use. `knowledge/` is the sharp case: attempt lessons live in refs that only
//! `loom show` surfaces, per task, so a lesson only compounds once an agent
//! writes it into a knowledge file and cites it from the next task.
//!
//! Ground truth lives in exactly one place per axis:
//!   knowledge dir  — `KNOWLEDGE_DIR` in src/model.rs
//!   skill text     — the plugin copy, which sync-agent-skill.sh mirrors

use std::fs;
use std::path::PathBuf;

const CANONICAL_SKILL: &str = "plugins/loom-task-manager/skills/loom/SKILL.md";
const MIRRORED_SKILL: &str = ".github/skills/loom/SKILL.md";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let path = root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// The directory the binary itself calls institutional memory.
fn knowledge_dir() -> String {
    const DECL: &str = "pub const KNOWLEDGE_DIR: &str = \"";
    let model = read("src/model.rs");
    let start = model
        .find(DECL)
        .expect("src/model.rs declares KNOWLEDGE_DIR")
        + DECL.len();
    let rest = &model[start..];
    let end = rest.find('"').expect("KNOWLEDGE_DIR literal is closed");
    rest[..end].to_string()
}

/// The text between `## <heading>` and the next `## ` heading.
fn section(doc: &str, heading: &str) -> String {
    let marker = format!("## {heading}\n");
    let start = doc
        .find(&marker)
        .unwrap_or_else(|| panic!("skill has no `## {heading}` section"))
        + marker.len();
    let rest = &doc[start..];
    let end = rest.find("\n## ").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_string()
}

/// Sentences mentioning the knowledge directory, so an assertion can ask what a
/// single instruction says rather than what the whole document happens to contain.
fn sentences_naming(text: &str, needle: &str) -> Vec<String> {
    text.split(|c| c == '.' || c == '\n')
        .filter(|s| s.contains(needle))
        .map(str::to_string)
        .collect()
}

#[test]
fn skill_loop_directs_writing_knowledge_files_before_done() {
    let dir = knowledge_dir();
    let path_form = format!("{dir}/");
    let skill = read(CANONICAL_SKILL);
    let the_loop = section(&skill, "The loop");

    let mention = the_loop.find(&path_form).unwrap_or_else(|| {
        panic!("`## The loop` never tells the agent to write into {path_form}")
    });
    let close_out = the_loop
        .find("loom done")
        .expect("`## The loop` closes out with `loom done`");
    assert!(
        mention < close_out,
        "knowledge capture must come before `loom done`, or the lease ends first"
    );
    assert!(
        the_loop.contains("<topic>"),
        "the loop names the file shape `{path_form}<topic>.md`, so files stay one fact wide"
    );
}

#[test]
fn skill_directs_citing_knowledge_files_as_task_context() {
    let dir = knowledge_dir();
    let skill = read(CANONICAL_SKILL);
    let cites: Vec<String> = sentences_naming(&skill, &format!("{dir}/"))
        .into_iter()
        .filter(|s| s.contains("--context"))
        .collect();
    assert!(
        !cites.is_empty(),
        "no instruction connects {dir}/ to `--context`: written knowledge nothing hydrates is dead weight"
    );
}

#[test]
fn skill_points_at_the_command_that_proposes_knowledge_files() {
    let skill = read(CANONICAL_SKILL);
    assert!(
        skill.contains("knowledge_candidates") || skill.contains("loom retro"),
        "the skill never mentions where knowledge-file candidates come from (`loom retro`)"
    );
}

#[test]
fn both_shipped_skill_copies_agree() {
    assert_eq!(
        read(CANONICAL_SKILL),
        read(MIRRORED_SKILL),
        "{MIRRORED_SKILL} drifted from {CANONICAL_SKILL}; run scripts/sync-agent-skill.sh"
    );
}
