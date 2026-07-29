//! README coverage gate.
//!
//! The README's reference tables are a contract agents script against, so they
//! have to track `src/` rather than drift behind it. Each test below derives
//! ground truth from the source of record and asserts the matching README
//! section documents every item.
//!
//! Ground truth lives in exactly one place per axis:
//!   commands       — the `Cmd` enum in src/main.rs
//!   exit codes     — the `AFTER_HELP` block in src/main.rs
//!   environment    — `LOOM_*` reads across src/
//!   policy fields  — the policy structs in src/model.rs

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let path = root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// The text between `## <heading>` and the next `## ` heading.
fn readme_section(readme: &str, heading: &str) -> String {
    let marker = format!("## {heading}\n");
    let start = readme
        .find(&marker)
        .unwrap_or_else(|| panic!("README has no `## {heading}` section"))
        + marker.len();
    let rest = &readme[start..];
    let end = rest.find("\n## ").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_string()
}

/// The body of `<keyword> <name> {` up to the first line that closes it at
/// column 0, so a following item in the same file is never swept in.
fn block<'a>(src: &'a str, opener: &str) -> &'a str {
    let start = src
        .find(opener)
        .unwrap_or_else(|| panic!("source has no `{opener}`"))
        + opener.len();
    let rest = &src[start..];
    let end = rest
        .find("\n}")
        .unwrap_or_else(|| panic!("`{opener}` is never closed at column 0"));
    &rest[..end]
}

fn to_kebab(variant: &str) -> String {
    let mut out = String::new();
    for (i, ch) in variant.char_indices() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Subcommand names, derived from the `Cmd` enum's variants. Variants sit at
/// four-space indent and start uppercase; their fields are indented deeper and
/// start lowercase, so the indent-plus-case anchor picks out variants alone.
fn shipped_commands() -> BTreeSet<String> {
    let main = read("src/main.rs");
    block(&main, "enum Cmd {")
        .lines()
        .filter_map(|line| {
            let name: String = line
                .strip_prefix("    ")?
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            let first = name.chars().next()?;
            first.is_ascii_uppercase().then(|| to_kebab(&name))
        })
        .collect()
}

fn shipped_exit_codes() -> BTreeSet<u8> {
    let main = read("src/main.rs");
    let start = main
        .find("Exit codes:")
        .expect("AFTER_HELP lists exit codes");
    let end = main
        .find("Environment:")
        .expect("AFTER_HELP lists environment");
    assert!(start < end, "AFTER_HELP sections are out of order");
    main[start..end]
        .lines()
        .filter_map(|line| {
            line.strip_prefix("  ")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .collect()
}

/// Names the binary actually reads from the environment. Anchoring on the
/// `env::var("…")` call rather than on the `LOOM_` prefix keeps same-prefix Rust
/// identifiers — test statics, consts — out of the ground truth.
fn shipped_env_vars() -> BTreeSet<String> {
    const CALL: &str = "env::var(\"";
    let mut found = BTreeSet::new();
    for entry in fs::read_dir(root().join("src")).expect("src/ is readable") {
        let path = entry.expect("readable dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = fs::read_to_string(&path).expect("readable source file");
        let mut rest = src.as_str();
        while let Some(i) = rest.find(CALL) {
            rest = &rest[i + CALL.len()..];
            let name: String = rest.chars().take_while(|c| *c != '"').collect();
            if name.starts_with("LOOM_") {
                found.insert(name);
            }
        }
    }
    found
}

/// Policy table names paired with their field names, e.g. `budget.tiers_minutes`.
fn shipped_policy_fields() -> BTreeSet<String> {
    let model = read("src/model.rs");
    let fields_of = |opener: &str| -> Vec<String> {
        block(&model, opener)
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("pub ")?;
                Some(rest.split(':').next()?.trim().to_string())
            })
            .collect()
    };

    // `Policy`'s own fields name the TOML tables; each table's struct names its keys.
    let tables = [
        ("budget", "struct Budget {"),
        ("lease", "struct LeasePolicy {"),
        ("schedule", "struct SchedulePolicy {"),
        ("verify", "struct VerifyPolicy {"),
    ];
    let declared: BTreeSet<String> = fields_of("struct Policy {").into_iter().collect();
    let covered: BTreeSet<String> = tables.iter().map(|(t, _)| t.to_string()).collect();
    assert_eq!(
        declared, covered,
        "Policy gained or lost a table; teach this test about it before the README check is meaningful"
    );

    tables
        .iter()
        .flat_map(|(table, opener)| {
            fields_of(opener)
                .into_iter()
                .map(move |f| format!("{table}.{f}"))
        })
        .collect()
}

#[test]
fn readme_documents_every_command() {
    let section = readme_section(&read("README.md"), "Commands");
    let missing: Vec<_> = shipped_commands()
        .into_iter()
        .filter(|cmd| !section.contains(&format!("`{cmd}")))
        .collect();
    assert!(
        missing.is_empty(),
        "README `## Commands` is missing shipped subcommands: {missing:?}"
    );
}

#[test]
fn readme_documents_every_exit_code() {
    let section = readme_section(&read("README.md"), "Exit codes");
    let missing: Vec<_> = shipped_exit_codes()
        .into_iter()
        .filter(|code| !section.contains(&format!("| {code} |")))
        .collect();
    assert!(
        missing.is_empty(),
        "README `## Exit codes` is missing codes the binary can return: {missing:?}"
    );
}

#[test]
fn readme_documents_every_environment_variable() {
    let section = readme_section(&read("README.md"), "Environment");
    let missing: Vec<_> = shipped_env_vars()
        .into_iter()
        .filter(|var| !section.contains(var.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "README `## Environment` is missing variables src/ reads: {missing:?}"
    );
}

#[test]
fn readme_documents_every_policy_field() {
    let section = readme_section(&read("README.md"), "Policy");
    let missing: Vec<_> = shipped_policy_fields()
        .into_iter()
        .filter(|field| {
            let (table, key) = field.split_once('.').expect("qualified field");
            !(section.contains(&format!("[{table}]")) && section.contains(key))
        })
        .collect();
    assert!(
        missing.is_empty(),
        "README `## Policy` is missing policy fields: {missing:?}"
    );
}

/// The reference tables are only trustworthy if they stay pruned as well as
/// complete, so a removed subcommand has to fail just as loudly as a new one.
#[test]
fn readme_documents_no_retired_commands() {
    let section = readme_section(&read("README.md"), "Commands");
    let shipped = shipped_commands();
    let stale: Vec<_> = section
        .lines()
        .filter_map(|line| line.trim().strip_prefix("| `")?.split(['`', ' ']).next())
        .filter(|cmd| !shipped.contains(*cmd))
        .collect();
    assert!(
        stale.is_empty(),
        "README `## Commands` documents subcommands that no longer ship: {stale:?}"
    );
}
