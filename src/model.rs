//! The on-disk model.
//!
//! v2 invariant: **task files are near-immutable**. They hold identity, spec,
//! and the two single-writer-under-lease mutations (probe output; the done
//! flip). Everything high-churn — attempts, verdicts, leases — lives in
//! conflict-free git refs (see `git.rs`), so no fleet activity can ever
//! produce a merge conflict in `.work/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const WORK_DIR: &str = ".work";

/// Compounding institutional memory: one small fact file per topic, referenced
/// from task context manifests. Retro proposes writes here.
pub const KNOWLEDGE_DIR: &str = "knowledge";

// ------------------------------------------------------------------- task

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Open,
    Done,
    Dead,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub goal: String,
    pub state: TaskState,
    #[serde(default = "default_value")]
    pub value: f64,
    #[serde(default)]
    pub deps: Vec<String>,
    /// Interface contract path(s) this task implements or consumes.
    #[serde(default)]
    pub contract: Vec<String>,
    /// Failing acceptance tests that define done. Empty ⇒ needs a probe.
    #[serde(default)]
    pub accept: Vec<String>,
    /// Context manifest: exactly what to read to start. An output of probes.
    #[serde(default)]
    pub context: Vec<String>,
    /// Starting budget tier. The *current* tier is derived:
    /// tier_initial + failed attempts (see protocol::current_tier).
    #[serde(default, alias = "budget_tier")]
    pub tier_initial: usize,
    pub created: DateTime<Utc>,
}

fn default_value() -> f64 {
    1.0
}

// ------------------------------------------------------------------ policy

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub budget: Budget,
    pub lease: LeasePolicy,
    pub schedule: SchedulePolicy,
    pub verify: VerifyPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Budget {
    pub tiers_minutes: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LeasePolicy {
    pub ttl_minutes: i64,
    pub heartbeat_minutes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulePolicy {
    pub staleness_half_life_hours: f64,
}

/// Gate on `loom done`.
/// - "independent": an approving verdict from a *different* agent, bound to
///   the exact candidate sha, is required. The default — the review discipline
///   is structural, not instructional.
/// - "any": a verdict is required but self-verdicts are allowed (recorded as
///   such for audit).
/// - "off": no verdict gate (tests-green still required).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyPolicy {
    pub mode: String,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            tiers_minutes: vec![10, 45, 180],
        }
    }
}
impl Default for LeasePolicy {
    fn default() -> Self {
        Self {
            ttl_minutes: 15,
            heartbeat_minutes: 5,
        }
    }
}
impl Default for SchedulePolicy {
    fn default() -> Self {
        Self {
            staleness_half_life_hours: 72.0,
        }
    }
}
impl Default for VerifyPolicy {
    fn default() -> Self {
        Self {
            mode: "independent".into(),
        }
    }
}

// -------------------------------------------------------------- escalation

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Escalation {
    pub id: String,
    pub question: String,
    pub options: Vec<String>,
    pub recommend: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub blocking: Vec<String>,
    #[serde(default)]
    pub deadline: Option<DateTime<Utc>>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub answer: Option<String>,
    #[serde(default)]
    pub answered_by: Option<String>,
    pub created: DateTime<Utc>,
}

impl Escalation {
    pub fn is_open(&self) -> bool {
        self.answer.is_none()
    }
}

// ----------------------------------------------------------- ref payloads

/// JSON payload in a lease commit message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeasePayload {
    pub agent: String,
    pub task: String,
    pub tier: usize,
    /// When the lease was first acquired (budget clock starts here).
    #[serde(default)]
    pub acquired: Option<DateTime<Utc>>,
    pub heartbeat: DateTime<Utc>,
}

/// JSON payload in an attempt ref (`refs/loom/attempt/<task>/<id>`).
/// Append-only, fleet-visible instantly, conflict-free by unique id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub task: String,
    pub tier: usize,
    pub sha: String,
    pub outcome: String,
    pub lesson: String,
    pub agent: String,
    pub at: DateTime<Utc>,
}

/// JSON payload in a verdict ref (`refs/loom/verdict/<task>`), bound to the
/// exact candidate sha it approves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub task: String,
    pub sha: String,
    pub verdict: String, // "approve" | "reject"
    pub agent: String,
    #[serde(default)]
    pub self_verdict: bool,
    pub at: DateTime<Utc>,
}

// --------------------------------------------------------------- workspace

pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    pub fn discover(start: &Path) -> Result<Self> {
        let mut dir = Some(start);
        while let Some(d) = dir {
            if d.join(WORK_DIR).is_dir() {
                return Ok(Self {
                    root: d.to_path_buf(),
                });
            }
            dir = d.parent();
        }
        Err(Error::WorkspaceNotFound(start.display().to_string()))
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.root.join(WORK_DIR).join("tasks")
    }
    pub fn escalations_dir(&self) -> PathBuf {
        self.root.join(WORK_DIR).join("escalations")
    }
    pub fn policy_path(&self) -> PathBuf {
        self.root.join(WORK_DIR).join("policy.toml")
    }
    pub fn task_path(&self, id: &str) -> PathBuf {
        self.tasks_dir().join(format!("{id}.toml"))
    }

    pub fn load_policy(&self) -> Result<Policy> {
        let p = self.policy_path();
        if !p.is_file() {
            return Ok(Policy::default());
        }
        parse_toml("policy", &p)
    }

    pub fn load_task(&self, id: &str) -> Result<Task> {
        let p = self.task_path(id);
        if !p.is_file() {
            return Err(Error::TaskNotFound(id.to_string()));
        }
        parse_toml("task", &p)
    }

    pub fn save_task(&self, task: &Task) -> Result<()> {
        std::fs::create_dir_all(self.tasks_dir())?;
        let body = toml::to_string_pretty(task)
            .map_err(|e| Error::Other(format!("serialize task {}: {e}", task.id)))?;
        std::fs::write(self.task_path(&task.id), body)?;
        Ok(())
    }

    /// Worktree read — for leased single-writer operations only. Cross-agent
    /// decisions must use the canonical graph (commands::canonical_tasks).
    pub fn load_all_tasks_worktree(&self) -> Result<BTreeMap<String, Task>> {
        let mut out = BTreeMap::new();
        let dir = self.tasks_dir();
        if !dir.is_dir() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                let task: Task = parse_toml("task", &path)?;
                out.insert(task.id.clone(), task);
            }
        }
        Ok(out)
    }

    pub fn load_all_escalations(&self) -> Result<Vec<Escalation>> {
        let mut out = Vec::new();
        let dir = self.escalations_dir();
        if !dir.is_dir() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                out.push(parse_toml::<Escalation>("escalation", &path)?);
            }
        }
        out.sort_by_key(|a| a.created);
        Ok(out)
    }

    pub fn save_escalation(&self, e: &Escalation) -> Result<()> {
        std::fs::create_dir_all(self.escalations_dir())?;
        let body = toml::to_string_pretty(e)
            .map_err(|err| Error::Other(format!("serialize escalation {}: {err}", e.id)))?;
        std::fs::write(self.escalations_dir().join(format!("{}.toml", e.id)), body)?;
        Ok(())
    }
}

pub fn parse_toml_str<T: serde::de::DeserializeOwned>(
    what: &'static str,
    origin: &str,
    raw: &str,
) -> Result<T> {
    toml::from_str(raw).map_err(|e| Error::Parse {
        what,
        path: origin.to_string(),
        msg: e.to_string(),
    })
}

fn parse_toml<T: serde::de::DeserializeOwned>(what: &'static str, path: &Path) -> Result<T> {
    let raw = std::fs::read_to_string(path)?;
    parse_toml_str(what, &path.display().to_string(), &raw)
}

/// Short id: hex of nanos-since-epoch xor pid, 10 chars.
pub fn new_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    format!("{:x}", nanos ^ (pid << 64))
        .chars()
        .rev()
        .take(10)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_roundtrip_defaults_and_budget_tier_alias() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".work/tasks")).unwrap();
        let ws = Workspace::discover(tmp.path()).unwrap();
        let t = Task {
            id: "abc".into(),
            goal: "do a thing".into(),
            state: TaskState::Open,
            value: 5.0,
            deps: vec!["xyz".into()],
            contract: vec![],
            accept: vec!["tests/accept/abc.rs".into()],
            context: vec![],
            tier_initial: 1,
            created: Utc::now(),
        };
        ws.save_task(&t).unwrap();
        assert_eq!(ws.load_task("abc").unwrap().tier_initial, 1);
        // v1 files with budget_tier still parse (alias)
        std::fs::write(
            ws.task_path("old"),
            "id=\"old\"\ngoal=\"g\"\nstate=\"open\"\nbudget_tier=2\ncreated=\"2026-01-01T00:00:00Z\"\n",
        )
        .unwrap();
        assert_eq!(ws.load_task("old").unwrap().tier_initial, 2);
    }

    #[test]
    fn workspace_discovery_walks_up() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".work")).unwrap();
        let nested = tmp.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(Workspace::discover(&nested).unwrap().root, tmp.path());
        let orphan = tempfile::tempdir().unwrap();
        assert!(matches!(
            Workspace::discover(orphan.path()),
            Err(Error::WorkspaceNotFound(_))
        ));
    }

    #[test]
    fn policy_defaults_include_independent_verification() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".work")).unwrap();
        let ws = Workspace::discover(tmp.path()).unwrap();
        let p = ws.load_policy().unwrap();
        assert_eq!(p.budget.tiers_minutes, vec![10, 45, 180]);
        assert_eq!(p.verify.mode, "independent");
    }

    #[test]
    fn ids_are_unique_enough() {
        let a = new_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_ne!(a, new_id());
    }
}
