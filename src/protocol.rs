//! Pure protocol logic — no I/O.
//!
//! v2 additions: the current budget tier is *derived* (initial + failed
//! attempts, capped), verification gating is a pure function, the heartbeat
//! daemon's continue/stop decision is pure (structural timebox), and retro
//! aggregation over telemetry is pure.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::model::{AttemptRecord, LeasePayload, Policy, Task, TaskState, Verdict};

// ------------------------------------------------------------ derived state

#[derive(Debug, Clone, PartialEq)]
pub enum Derived {
    Done,
    Dead,
    Blocked(Vec<String>),
    Leased(String),
    Parked,
    Open,
}

pub fn derive(
    task: &Task,
    all: &BTreeMap<String, Task>,
    leases: &BTreeMap<String, LeasePayload>,
    attempts: &BTreeMap<String, Vec<AttemptRecord>>,
) -> Derived {
    match task.state {
        TaskState::Done => return Derived::Done,
        TaskState::Dead => return Derived::Dead,
        TaskState::Open => {}
    }
    let open_deps: Vec<String> = task
        .deps
        .iter()
        .filter(|d| {
            all.get(*d)
                .map(|t| t.state != TaskState::Done)
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    if !open_deps.is_empty() {
        return Derived::Blocked(open_deps);
    }
    if let Some(lease) = leases.get(&task.id) {
        return Derived::Leased(lease.agent.clone());
    }
    if attempts
        .get(&task.id)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return Derived::Parked;
    }
    Derived::Open
}

// -------------------------------------------------------------- budget tier

/// Derived current tier: initial + failed attempts, capped at the last tier.
/// Pure derivation ⇒ no mutable tier field to conflict or drift.
pub fn current_tier(
    task: &Task,
    attempts: &BTreeMap<String, Vec<AttemptRecord>>,
    tiers: &[u64],
) -> usize {
    let failed = attempts.get(&task.id).map(Vec::len).unwrap_or(0);
    (task.tier_initial + failed).min(tiers.len().saturating_sub(1))
}

/// After recording this attempt, is the ladder exhausted?
pub fn ladder_exhausted(task: &Task, attempts_after: usize, tiers: &[u64]) -> bool {
    task.tier_initial + attempts_after >= tiers.len()
}

// -------------------------------------------------------------- scheduling

pub fn unblocks(id: &str, all: &BTreeMap<String, Task>) -> usize {
    all.values()
        .filter(|t| t.state == TaskState::Open && t.deps.iter().any(|d| d == id))
        .count()
}

pub fn staleness_factor(created: DateTime<Utc>, now: DateTime<Utc>, half_life_hours: f64) -> f64 {
    if half_life_hours <= 0.0 {
        return 1.0;
    }
    let age_h = (now - created).num_minutes().max(0) as f64 / 60.0;
    1.0 + age_h / half_life_hours
}

pub fn score(
    task: &Task,
    all: &BTreeMap<String, Task>,
    attempts: &BTreeMap<String, Vec<AttemptRecord>>,
    now: DateTime<Utc>,
    policy: &Policy,
) -> f64 {
    task.value
        * (1.0 + unblocks(&task.id, all) as f64)
        * staleness_factor(task.created, now, policy.schedule.staleness_half_life_hours)
        / (1.0 + current_tier(task, attempts, &policy.budget.tiers_minutes) as f64)
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub goal: String,
    pub score: f64,
    pub needs_probe: bool,
    pub tier: usize,
    pub resumes: bool,
}

pub fn schedule(
    all: &BTreeMap<String, Task>,
    leases: &BTreeMap<String, LeasePayload>,
    attempts: &BTreeMap<String, Vec<AttemptRecord>>,
    now: DateTime<Utc>,
    policy: &Policy,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = all
        .values()
        .filter(|t| {
            matches!(
                derive(t, all, leases, attempts),
                Derived::Open | Derived::Parked
            )
        })
        .map(|t| Candidate {
            id: t.id.clone(),
            goal: t.goal.clone(),
            score: score(t, all, attempts, now, policy),
            needs_probe: t.accept.is_empty(),
            tier: current_tier(t, attempts, &policy.budget.tiers_minutes),
            resumes: attempts.get(&t.id).map(|v| !v.is_empty()).unwrap_or(false),
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

// -------------------------------------------------------------- leases

pub fn lease_is_stale(lease: &LeasePayload, now: DateTime<Utc>, ttl_minutes: i64) -> bool {
    (now - lease.heartbeat).num_minutes() >= ttl_minutes
}

/// The heartbeat daemon's decision: keep the lease alive only while inside
/// the tier's budget. Stopping at budget end lets the TTL sweep reclaim the
/// task — the timebox is enforced by the system, not by agent discipline.
pub fn daemon_should_continue(
    acquired: DateTime<Utc>,
    now: DateTime<Utc>,
    tier: usize,
    tiers_minutes: &[u64],
) -> bool {
    let budget = tiers_minutes.get(tier).copied().unwrap_or(0) as i64;
    (now - acquired).num_minutes() < budget
}

// ---------------------------------------------------------- verification

/// Pure gate for `loom done`. `mode`: "independent" | "any" | "off".
pub fn verify_gate(
    task_id: &str,
    head: &str,
    implementer: &str,
    verdict: Option<&Verdict>,
    mode: &str,
) -> Result<()> {
    if mode == "off" {
        return Ok(());
    }
    let v = verdict.ok_or_else(|| Error::VerdictMissing {
        task: task_id.into(),
        sha: head.into(),
    })?;
    if v.verdict != "approve" || v.task != task_id {
        return Err(Error::VerdictMissing {
            task: task_id.into(),
            sha: head.into(),
        });
    }
    if v.sha != head {
        return Err(Error::VerdictStale {
            task: task_id.into(),
            verdict_sha: v.sha.clone(),
            head: head.into(),
        });
    }
    if mode == "independent" && v.agent == implementer {
        return Err(Error::VerdictNotIndependent {
            task: task_id.into(),
            agent: v.agent.clone(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------- retro

/// Aggregate telemetry + attempts into a report and mechanical policy
/// suggestions. The read half of the learning loop.
pub fn retro(
    telemetry: &[Value],
    attempts: &BTreeMap<String, Vec<AttemptRecord>>,
    policy: &Policy,
    existing_knowledge: &[String],
) -> Value {
    let _ = existing_knowledge;
    let merged: Vec<&Value> = telemetry
        .iter()
        .filter(|r| r.get("outcome").and_then(Value::as_str) == Some("merged"))
        .collect();
    let review_cycles: Vec<f64> = merged
        .iter()
        .filter_map(|r| r.get("review_cycles").and_then(Value::as_f64))
        .collect();
    let mean_review = if review_cycles.is_empty() {
        0.0
    } else {
        review_cycles.iter().sum::<f64>() / review_cycles.len() as f64
    };

    let total_attempts: usize = attempts.values().map(Vec::len).sum();
    let tasks_with_attempts = attempts.values().filter(|v| !v.is_empty()).count();
    let review_rejects = attempts
        .values()
        .flatten()
        .filter(|a| a.outcome == "review-reject")
        .count();
    let exhausted = attempts
        .values()
        .filter(|v| v.len() >= policy.budget.tiers_minutes.len())
        .count();

    let mut suggestions: Vec<String> = Vec::new();
    if tasks_with_attempts > 0 {
        let mean_attempts = total_attempts as f64 / tasks_with_attempts as f64;
        if mean_attempts >= 2.0 {
            suggestions.push(format!(
                "mean attempts/task = {mean_attempts:.1}: probes are under-specifying; raise tier-0 minutes ({} now) or demand stricter acceptance tests at probe-done",
                policy.budget.tiers_minutes.first().copied().unwrap_or(0)
            ));
        }
    }
    if total_attempts > 0 && review_rejects * 3 >= total_attempts {
        suggestions.push(
            "review-reject rate ≥ 33% of attempts: implementations are gaming or missing specs; tighten acceptance tests before execution".into(),
        );
    }
    if exhausted > 0 {
        suggestions.push(format!(
            "{exhausted} task(s) exhausted the ladder: decomposition is happening too late; lower the size threshold at probe time"
        ));
    }
    if mean_review > 1.5 {
        suggestions.push(format!(
            "mean review cycles = {mean_review:.1}: candidates arrive under-verified; run the mechanical gate before requesting verdicts"
        ));
    }

    json!({
        "merged": merged.len(),
        "telemetry_records": telemetry.len(),
        "attempts_total": total_attempts,
        "tasks_with_attempts": tasks_with_attempts,
        "review_rejects": review_rejects,
        "ladder_exhaustions": exhausted,
        "mean_review_cycles": (mean_review * 100.0).round() / 100.0,
        "suggestions": suggestions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, deps: &[&str], value: f64, tier: usize, accept: bool) -> Task {
        Task {
            id: id.into(),
            goal: format!("goal {id}"),
            state: TaskState::Open,
            value,
            deps: deps.iter().map(|s| s.to_string()).collect(),
            contract: vec![],
            accept: if accept { vec!["t".into()] } else { vec![] },
            context: vec![],
            tier_initial: tier,
            created: "2026-07-01T00:00:00Z".parse().unwrap(),
        }
    }

    fn graph(tasks: Vec<Task>) -> BTreeMap<String, Task> {
        tasks.into_iter().map(|t| (t.id.clone(), t)).collect()
    }

    fn attempt(task: &str, outcome: &str, at: &str) -> AttemptRecord {
        attempt_lesson(task, outcome, "l", at)
    }

    fn attempt_lesson(task: &str, outcome: &str, lesson: &str, at: &str) -> AttemptRecord {
        AttemptRecord {
            task: task.into(),
            tier: 0,
            sha: "s".into(),
            outcome: outcome.into(),
            lesson: lesson.into(),
            agent: "a".into(),
            at: at.parse().unwrap(),
        }
    }

    #[test]
    fn tier_is_derived_from_attempt_count() {
        let t = task("a", &[], 1.0, 0, true);
        let tiers = [10u64, 45, 180];
        let mut attempts = BTreeMap::new();
        assert_eq!(current_tier(&t, &attempts, &tiers), 0);
        attempts.insert(
            "a".into(),
            vec![attempt("a", "tests-red", "2026-07-01T00:00:00Z")],
        );
        assert_eq!(current_tier(&t, &attempts, &tiers), 1);
        attempts.get_mut("a").unwrap().extend([
            attempt("a", "tests-red", "2026-07-01T01:00:00Z"),
            attempt("a", "tests-red", "2026-07-01T02:00:00Z"),
        ]);
        assert_eq!(current_tier(&t, &attempts, &tiers), 2); // capped
        assert!(ladder_exhausted(&t, 3, &tiers));
        assert!(!ladder_exhausted(&t, 2, &tiers));
        // initial tier offsets the ladder
        let t1 = task("b", &[], 1.0, 1, true);
        assert!(ladder_exhausted(&t1, 2, &tiers));
    }

    #[test]
    fn derived_state_uses_attempt_refs_for_parked() {
        let a = task("a", &[], 1.0, 0, true);
        let all = graph(vec![a.clone()]);
        let leases = BTreeMap::new();
        let mut attempts = BTreeMap::new();
        assert_eq!(derive(&a, &all, &leases, &attempts), Derived::Open);
        attempts.insert(
            "a".into(),
            vec![attempt("a", "tests-red", "2026-07-01T00:00:00Z")],
        );
        assert_eq!(derive(&a, &all, &leases, &attempts), Derived::Parked);
    }

    #[test]
    fn schedule_orders_by_value_unblocks_and_derived_tier() {
        let all = graph(vec![
            task("c", &[], 2.0, 0, true),
            task("x", &["c"], 1.0, 0, true),
            task("y", &["c"], 1.0, 0, true),
            task("d", &[], 5.0, 0, true),
        ]);
        let leases = BTreeMap::new();
        // d has one failed attempt ⇒ derived tier 1 ⇒ score 5/2 = 2.5
        let mut attempts = BTreeMap::new();
        attempts.insert(
            "d".into(),
            vec![attempt("d", "tests-red", "2026-07-01T00:00:00Z")],
        );
        let now = "2026-07-01T00:00:00Z".parse().unwrap();
        let sched = schedule(&all, &leases, &attempts, now, &Policy::default());
        let ids: Vec<&str> = sched.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "d"]); // c: 6.0, d: 2.5; x,y blocked
        assert!(sched[1].resumes);
        assert_eq!(sched[1].tier, 1);
    }

    #[test]
    fn daemon_stops_at_budget_boundary() {
        let acquired: DateTime<Utc> = "2026-07-16T00:00:00Z".parse().unwrap();
        let tiers = [10u64, 45, 180];
        let inside: DateTime<Utc> = "2026-07-16T00:09:00Z".parse().unwrap();
        let outside: DateTime<Utc> = "2026-07-16T00:10:00Z".parse().unwrap();
        assert!(daemon_should_continue(acquired, inside, 0, &tiers));
        assert!(!daemon_should_continue(acquired, outside, 0, &tiers));
        assert!(daemon_should_continue(acquired, outside, 1, &tiers)); // 45m tier
    }

    #[test]
    fn verify_gate_enforces_presence_binding_and_independence() {
        let v = |agent: &str, sha: &str, verdict: &str| Verdict {
            task: "t".into(),
            sha: sha.into(),
            verdict: verdict.into(),
            agent: agent.into(),
            self_verdict: false,
            at: Utc::now(),
        };
        // off: anything goes
        verify_gate("t", "h", "me", None, "off").unwrap();
        // missing
        assert_eq!(
            verify_gate("t", "h", "me", None, "independent")
                .unwrap_err()
                .exit_code(),
            5
        );
        // reject verdict doesn't pass
        let r = v("rev", "h", "reject");
        assert!(verify_gate("t", "h", "me", Some(&r), "any").is_err());
        // stale sha
        let stale = v("rev", "old", "approve");
        assert!(matches!(
            verify_gate("t", "h", "me", Some(&stale), "independent").unwrap_err(),
            Error::VerdictStale { .. }
        ));
        // self-verdict blocked in independent, allowed in any
        let selfv = v("me", "h", "approve");
        assert!(matches!(
            verify_gate("t", "h", "me", Some(&selfv), "independent").unwrap_err(),
            Error::VerdictNotIndependent { .. }
        ));
        verify_gate("t", "h", "me", Some(&selfv), "any").unwrap();
        // the happy path
        let ok = v("rev", "h", "approve");
        verify_gate("t", "h", "me", Some(&ok), "independent").unwrap();
    }

    #[test]
    fn retro_aggregates_and_suggests() {
        let telemetry = vec![
            serde_json::json!({"outcome":"merged","review_cycles":2.0}),
            serde_json::json!({"outcome":"merged","review_cycles":2.0}),
        ];
        let mut attempts = BTreeMap::new();
        attempts.insert(
            "a".into(),
            vec![
                attempt("a", "tests-red", "2026-07-01T00:00:00Z"),
                attempt("a", "review-reject", "2026-07-01T01:00:00Z"),
                attempt("a", "tests-red", "2026-07-01T02:00:00Z"),
            ],
        );
        let report = retro(&telemetry, &attempts, &Policy::default(), &[]);
        assert_eq!(report["merged"], 2);
        assert_eq!(report["attempts_total"], 3);
        assert_eq!(report["ladder_exhaustions"], 1);
        let s = report["suggestions"].as_array().unwrap();
        assert!(s.len() >= 3, "expected multiple suggestions, got {s:?}");
    }

    /// The write half of the learning loop: lessons the fleet keeps re-learning
    /// become proposed `knowledge/<topic>.md` files, next to the policy
    /// suggestions that target `.work/policy.toml`.
    #[test]
    fn retro_proposes_knowledge_files_for_lessons_learned_twice() {
        let mut attempts = BTreeMap::new();
        attempts.insert(
            "a".into(),
            vec![attempt_lesson(
                "a",
                "tests-red",
                "Push probe output before continuing.",
                "2026-07-01T00:00:00Z",
            )],
        );
        attempts.insert(
            "b".into(),
            vec![
                attempt_lesson(
                    "b",
                    "review-reject",
                    "push probe output before continuing",
                    "2026-07-01T01:00:00Z",
                ),
                attempt_lesson(
                    "b",
                    "tests-red",
                    "a one-off detail nobody hits twice",
                    "2026-07-01T02:00:00Z",
                ),
            ],
        );

        let report = retro(
            &[],
            &attempts,
            &Policy::default(),
            &["knowledge/README.md".to_string()],
        );
        let cands = report["knowledge_candidates"]
            .as_array()
            .expect("retro emits knowledge_candidates alongside suggestions");

        let repeated = cands
            .iter()
            .find(|c| {
                c["lessons"]
                    .as_array()
                    .is_some_and(|ls| ls.iter().any(|l| l == "Push probe output before continuing."))
            })
            .unwrap_or_else(|| panic!("lesson learned on two tasks becomes a candidate: {cands:?}"));

        let path = repeated["path"].as_str().unwrap();
        assert!(
            path.starts_with("knowledge/") && path.ends_with(".md"),
            "candidate path is a knowledge file, got {path}"
        );
        assert_eq!(repeated["tasks"], json!(["a", "b"]));
        assert_eq!(repeated["exists"], json!(false));
        assert!(
            repeated["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "each candidate says why it earned a file"
        );

        assert!(
            !cands.iter().any(|c| c["lessons"]
                .as_array()
                .is_some_and(|ls| ls.iter().any(|l| l == "a one-off detail nobody hits twice"))),
            "a lesson seen once isn't institutional memory yet: {cands:?}"
        );
        assert!(report["suggestions"].is_array(), "policy half still emitted");
    }

    /// An outcome that keeps recurring across distinct tasks earns a file too,
    /// and candidates say whether that file already exists so the agent knows
    /// to append rather than clobber.
    #[test]
    fn retro_knowledge_candidates_group_recurring_outcomes_and_flag_existing_files() {
        let mut attempts = BTreeMap::new();
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            attempts.insert(
                (*id).into(),
                vec![attempt_lesson(
                    id,
                    "review-reject",
                    &format!("lesson from {id}"),
                    &format!("2026-07-0{}T00:00:00Z", i + 1),
                )],
            );
        }

        let report = retro(
            &[],
            &attempts,
            &Policy::default(),
            &["knowledge/attempt-review-reject.md".to_string()],
        );
        let cands = report["knowledge_candidates"].as_array().unwrap();
        let by_outcome = cands
            .iter()
            .find(|c| c["path"] == json!("knowledge/attempt-review-reject.md"))
            .unwrap_or_else(|| panic!("recurring outcome earns a file: {cands:?}"));

        assert_eq!(by_outcome["tasks"], json!(["a", "b", "c"]));
        assert_eq!(by_outcome["exists"], json!(true));
        let lessons = by_outcome["lessons"].as_array().unwrap();
        assert_eq!(lessons.len(), 3, "the file carries its source lessons");
    }

    #[test]
    fn staleness_and_lease_ttl() {
        let created = "2026-07-01T00:00:00Z".parse().unwrap();
        let now = "2026-07-04T00:00:00Z".parse().unwrap();
        assert!((staleness_factor(created, now, 72.0) - 2.0).abs() < 1e-9);
        let lease = LeasePayload {
            agent: "w".into(),
            task: "t".into(),
            tier: 0,
            acquired: None,
            heartbeat: "2026-07-16T00:40:00Z".parse().unwrap(),
        };
        let now2: DateTime<Utc> = "2026-07-16T01:00:00Z".parse().unwrap();
        assert!(lease_is_stale(&lease, now2, 15));
        assert!(!lease_is_stale(&lease, now2, 30));
    }
}
