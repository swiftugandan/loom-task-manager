//! Command implementations.
//!
//! v2 structural guarantees (each closes a named v1 gap):
//! - Cross-agent decisions read the CANONICAL graph (origin/main), never the
//!   possibly branch-mutated worktree.
//! - `done` is atomic: verify gate → state flip → `git commit` in one
//!   operation, with rollback of the file write if the commit fails.
//! - Attempts are conflict-free refs, so concurrent fleet activity can never
//!   merge-conflict `.work/`.
//! - `heartbeat --daemon` makes the heartbeat/timebox discipline mechanical.
//! - `retro` is the read half of the telemetry loop.

use std::collections::BTreeMap;

use chrono::Utc;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::git::Git;
use crate::model::{
    new_id, parse_toml_str, AttemptRecord, Escalation, LeasePayload, Policy, Task, TaskState,
    Verdict, Workspace, KNOWLEDGE_DIR, WORK_DIR,
};
use crate::protocol::{self, Derived};
use crate::runner::Runner;

pub struct Ctx<'a> {
    pub runner: &'a dyn Runner,
    pub ws: Workspace,
    pub policy: Policy,
}

pub struct Snapshot {
    pub tasks: BTreeMap<String, Task>,
    pub leases: BTreeMap<String, LeasePayload>,
    pub attempts: BTreeMap<String, Vec<AttemptRecord>>,
    pub canonical: bool,
}

impl<'a> Ctx<'a> {
    pub fn git(&self) -> Git<'_> {
        Git::new(self.runner)
    }

    pub fn agent(&self) -> String {
        if let Ok(a) = std::env::var("LOOM_AGENT") {
            if !a.trim().is_empty() {
                return a;
            }
        }
        self.runner
            .try_run("git", &["config", "user.email"])
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("agent-{}", std::process::id()))
    }

    fn main_ref(&self) -> String {
        std::env::var("LOOM_MAIN").unwrap_or_else(|_| "main".into())
    }

    /// Canonical task graph: read from <remote>/<main>, falling back to the
    /// worktree (with a warning) only when remote main doesn't exist yet.
    fn canonical_tasks(&self) -> Result<(BTreeMap<String, Task>, bool)> {
        let git = self.git();
        match git.canonical_task_blobs(&self.main_ref())? {
            Some(blobs) => {
                let mut map = BTreeMap::new();
                for (path, blob) in blobs {
                    let t: Task = parse_toml_str("task", &path, &blob)?;
                    map.insert(t.id.clone(), t);
                }
                Ok((map, true))
            }
            None => {
                eprintln!(
                    "loom: warning: {}/{} not found — reading graph from worktree (fresh repo?)",
                    git.remote,
                    self.main_ref()
                );
                Ok((self.ws.load_all_tasks_worktree()?, false))
            }
        }
    }

    fn snapshot(&self) -> Result<Snapshot> {
        let git = self.git();
        git.fetch_loom_refs()?;
        let leases = git
            .leases()?
            .into_iter()
            .map(|(k, (p, _))| (k, p))
            .collect();
        let attempts = git.attempts()?;
        let (tasks, canonical) = self.canonical_tasks()?;
        Ok(Snapshot {
            tasks,
            leases,
            attempts,
            canonical,
        })
    }
}

// ------------------------------------------------------------------- init

pub fn init(runner: &dyn Runner) -> Result<()> {
    let cwd = std::env::current_dir()?;
    runner
        .run("git", &["rev-parse", "--show-toplevel"])
        .map_err(|_| Error::Other("loom init must run inside a git repository".into()))?;
    let work = cwd.join(WORK_DIR);
    std::fs::create_dir_all(work.join("tasks"))?;
    std::fs::create_dir_all(work.join("escalations"))?;
    std::fs::create_dir_all(cwd.join("knowledge"))?;
    let policy_path = work.join("policy.toml");
    if !policy_path.exists() {
        let body = toml::to_string_pretty(&Policy::default())
            .map_err(|e| Error::Other(format!("serialize default policy: {e}")))?;
        std::fs::write(&policy_path, body)?;
    }
    let keep = cwd.join("knowledge/README.md");
    if !keep.exists() {
        std::fs::write(
            keep,
            "# knowledge/\n\nCompounding institutional memory. One small fact file per topic;\nreference these files from task context manifests.\n",
        )?;
    }
    eprintln!(
        "loom: initialized .work/ and knowledge/ (commit and push so the fleet shares the graph)"
    );
    Ok(())
}

// ------------------------------------------------------------------ doctor

pub fn doctor(ctx: &Ctx) -> Result<()> {
    ctx.runner.run("git", &["rev-parse", "--show-toplevel"])?;
    ctx.runner
        .run("git", &["ls-remote", "--heads", &ctx.git().remote])
        .map_err(|e| Error::Other(format!("cannot reach remote '{}': {e}", ctx.git().remote)))?;
    let snap = ctx.snapshot()?;
    let mut dangling = Vec::new();
    for t in snap.tasks.values() {
        for d in &t.deps {
            if !snap.tasks.contains_key(d) {
                dangling.push(format!("{}→{}", t.id, d));
            }
        }
    }
    if !dangling.is_empty() {
        return Err(Error::Other(format!(
            "dangling deps: {}",
            dangling.join(", ")
        )));
    }
    println!(
        "ok: root={} graph={} tasks={} tiers={:?} lease_ttl={}m verify={} agent={}",
        ctx.ws.root.display(),
        if snap.canonical {
            "canonical(origin/main)"
        } else {
            "worktree(fallback)"
        },
        snap.tasks.len(),
        ctx.policy.budget.tiers_minutes,
        ctx.policy.lease.ttl_minutes,
        ctx.policy.verify.mode,
        ctx.agent()
    );
    Ok(())
}

// -------------------------------------------------------------- task crud

#[allow(clippy::too_many_arguments)]
pub fn task_create(
    ctx: &Ctx,
    goal: String,
    value: f64,
    deps: Vec<String>,
    contract: Vec<String>,
    accept: Vec<String>,
    context: Vec<String>,
    tier: usize,
) -> Result<()> {
    // Deps may exist canonically or in the worktree (batch seeding).
    let worktree = ctx.ws.load_all_tasks_worktree()?;
    for d in &deps {
        if !worktree.contains_key(d) {
            ctx.ws.load_task(d)?; // typed TaskNotFound
        }
    }
    let task = Task {
        id: new_id(),
        goal,
        state: TaskState::Open,
        value,
        deps,
        contract,
        accept,
        context,
        tier_initial: tier,
        created: Utc::now(),
    };
    ctx.ws.save_task(&task)?;
    println!(
        "{}",
        json!({"id": task.id, "needs_probe": task.accept.is_empty()})
    );
    eprintln!(
        "loom: '{}' created (commit and push .work/tasks/ so the canonical graph — and `loom next` — sees it)",
        task.id
    );
    Ok(())
}

pub fn tasks(ctx: &Ctx) -> Result<()> {
    let snap = ctx.snapshot()?;
    let rows: Vec<_> = snap
        .tasks
        .values()
        .map(|t| {
            let d = protocol::derive(t, &snap.tasks, &snap.leases, &snap.attempts);
            json!({
                "id": t.id, "goal": t.goal, "value": t.value,
                "tier": protocol::current_tier(t, &snap.attempts, &ctx.policy.budget.tiers_minutes),
                "state": format!("{:?}", d).to_lowercase(),
                "deps": t.deps,
                "attempts": snap.attempts.get(&t.id).map(Vec::len).unwrap_or(0),
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

pub fn show(ctx: &Ctx, id: &str) -> Result<()> {
    let snap = ctx.snapshot()?;
    let t = snap
        .tasks
        .get(id)
        .ok_or_else(|| Error::TaskNotFound(id.into()))?;
    let d = protocol::derive(t, &snap.tasks, &snap.leases, &snap.attempts);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "task": t,
            "derived": format!("{:?}", d).to_lowercase(),
            "tier": protocol::current_tier(t, &snap.attempts, &ctx.policy.budget.tiers_minutes),
            "unblocks": protocol::unblocks(id, &snap.tasks),
            "attempts": snap.attempts.get(id).cloned().unwrap_or_default(),
            "graph": if snap.canonical { "canonical" } else { "worktree" },
        }))?
    );
    Ok(())
}

// -------------------------------------------------------------- schedule

pub fn next(ctx: &Ctx, all_candidates: bool) -> Result<()> {
    let snap = ctx.snapshot()?;
    let sched = protocol::schedule(
        &snap.tasks,
        &snap.leases,
        &snap.attempts,
        Utc::now(),
        &ctx.policy,
    );
    if sched.is_empty() {
        return Err(Error::NothingSchedulable);
    }
    let row = |c: &protocol::Candidate| {
        json!({
            "id": c.id, "goal": c.goal, "score": (c.score * 100.0).round() / 100.0,
            "needs_probe": c.needs_probe, "resumes": c.resumes, "tier": c.tier,
        })
    };
    if all_candidates {
        println!(
            "{}",
            serde_json::to_string_pretty(&sched.iter().map(row).collect::<Vec<_>>())?
        );
    } else {
        println!("{}", serde_json::to_string_pretty(&row(&sched[0]))?);
    }
    Ok(())
}

// ---------------------------------------------------------------- leases

pub fn lease(ctx: &Ctx, id: &str) -> Result<()> {
    let snap = ctx.snapshot()?;
    let t = snap
        .tasks
        .get(id)
        .ok_or_else(|| Error::TaskNotFound(id.into()))?;
    match protocol::derive(t, &snap.tasks, &snap.leases, &snap.attempts) {
        Derived::Open | Derived::Parked => {}
        Derived::Leased(holder) => {
            return Err(Error::LeaseRaceLost {
                task: id.into(),
                holder,
            })
        }
        Derived::Blocked(deps) => {
            return Err(Error::Other(format!(
                "task '{id}' is blocked by open deps: {}",
                deps.join(", ")
            )))
        }
        Derived::Done | Derived::Dead => {
            return Err(Error::Other(format!("task '{id}' is not open")))
        }
    }
    for e in ctx.ws.load_all_escalations()? {
        if e.is_open() && e.default.is_none() && e.blocking.iter().any(|b| b == id) {
            return Err(Error::OracleBlocked {
                task: id.into(),
                escalation: e.id,
            });
        }
    }
    let tier = protocol::current_tier(t, &snap.attempts, &ctx.policy.budget.tiers_minutes);
    ctx.git().acquire_lease(id, &ctx.agent(), tier)?;
    eprintln!(
        "loom: leased '{id}' at tier {tier} ({} min budget)",
        ctx.policy
            .budget
            .tiers_minutes
            .get(tier)
            .copied()
            .unwrap_or(0),
    );
    eprintln!("loom: start the mechanical heartbeat now: `loom heartbeat {id} --daemon`");
    if t.accept.is_empty() {
        eprintln!("loom: no acceptance tests — PROBE first");
    }
    if snap
        .attempts
        .get(id)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        eprintln!(
            "loom: prior attempts exist — `loom show {id}` and read every lesson before starting"
        );
    }
    Ok(())
}

pub fn heartbeat(ctx: &Ctx, id: &str, daemon: bool, daemon_loop: bool) -> Result<()> {
    if daemon_loop {
        return heartbeat_daemon_loop(ctx, id);
    }
    if daemon {
        // Detach a child running the hidden loop; the agent's session is free.
        let exe = std::env::current_exe()?;
        std::process::Command::new(exe)
            .args(["heartbeat", id, "--daemon-loop"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        eprintln!(
            "loom: heartbeat daemon started for '{id}' (every {}m, stops at tier budget end so the timebox is structural)",
            ctx.policy.lease.heartbeat_minutes
        );
        return Ok(());
    }
    ctx.git().heartbeat_lease(id, &ctx.agent())
}

fn heartbeat_daemon_loop(ctx: &Ctx, id: &str) -> Result<()> {
    let agent = ctx.agent();
    let interval =
        std::time::Duration::from_secs((ctx.policy.lease.heartbeat_minutes.max(1) as u64) * 60);
    loop {
        std::thread::sleep(interval);
        // Re-read lease each cycle: stop if gone, foreign, or budget elapsed.
        ctx.git().fetch_loom_refs()?;
        let leases = ctx.git().leases()?;
        let Some((payload, _)) = leases.get(id) else {
            return Ok(());
        };
        if payload.agent != agent {
            return Ok(());
        }
        let acquired = payload.acquired.unwrap_or(payload.heartbeat);
        if !protocol::daemon_should_continue(
            acquired,
            Utc::now(),
            payload.tier,
            &ctx.policy.budget.tiers_minutes,
        ) {
            return Ok(()); // budget over: stop heartbeating; TTL sweep reclaims
        }
        let _ = ctx.git().heartbeat_lease(id, &agent);
    }
}

pub fn release(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.git().release_lease(id, &ctx.agent(), false)?;
    eprintln!("loom: released '{id}'");
    Ok(())
}

pub fn status(ctx: &Ctx, id: &str) -> Result<()> {
    let git = ctx.git();
    git.fetch_loom_refs()?;
    let leases = git.leases()?;
    let verdict = git.verdict(id)?;
    let out = match leases.get(id) {
        Some((p, _)) => {
            let acquired = p.acquired.unwrap_or(p.heartbeat);
            let budget = ctx
                .policy
                .budget
                .tiers_minutes
                .get(p.tier)
                .copied()
                .unwrap_or(0) as i64;
            let elapsed = (Utc::now() - acquired).num_minutes();
            json!({
                "leased_by": p.agent, "tier": p.tier,
                "elapsed_minutes": elapsed, "budget_minutes": budget,
                "remaining_minutes": (budget - elapsed).max(0),
                "over_budget": elapsed >= budget,
                "verdict": verdict,
            })
        }
        None => json!({ "leased_by": null, "verdict": verdict }),
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

// ------------------------------------------------------------- lifecycle

/// Atomic done: verify gate → flip state → commit code+tests+state together.
/// If the commit fails, the file write is rolled back — the tracker can never
/// claim done for uncommitted work.
pub fn done(ctx: &Ctx, id: &str, message: Option<String>) -> Result<()> {
    let mut t = ctx.ws.load_task(id)?;
    if t.accept.is_empty() {
        return Err(Error::Other(format!(
            "refusing to mark '{id}' done: it has no acceptance tests"
        )));
    }
    for a in &t.accept {
        if !ctx.ws.root.join(a).exists() {
            return Err(Error::Other(format!(
                "acceptance test '{a}' does not exist on disk"
            )));
        }
    }
    let head = ctx.runner.run("git", &["rev-parse", "HEAD"])?;
    let git = ctx.git();
    git.fetch_loom_refs()?;
    let verdict = git.verdict(id)?;
    protocol::verify_gate(
        id,
        &head,
        &ctx.agent(),
        verdict.as_ref(),
        &ctx.policy.verify.mode,
    )?;

    let path = ctx.ws.task_path(id);
    let original = std::fs::read(&path)?;
    t.state = TaskState::Done;
    ctx.ws.save_task(&t)?;

    let msg = message.unwrap_or_else(|| format!("task {id}: {}", t.goal));
    let commit = ctx
        .runner
        .run("git", &["add", "-A"])
        .and_then(|_| ctx.runner.run("git", &["commit", "-m", &msg]));
    if let Err(e) = commit {
        std::fs::write(&path, original)?; // rollback: nothing was marked done
        return Err(Error::DoneNotCommitted {
            task: id.into(),
            msg: e.to_string(),
        });
    }
    let _ = git.release_lease(id, &ctx.agent(), false);
    eprintln!(
        "loom: '{id}' done — state flip and implementation committed atomically (git push so the canonical graph sees it)"
    );
    Ok(())
}

pub fn dead(ctx: &Ctx, id: &str, why: &str) -> Result<()> {
    let mut t = ctx.ws.load_task(id)?;
    t.state = TaskState::Dead;
    ctx.ws.save_task(&t)?;
    let git = ctx.git();
    let _ = git.record_attempt(&AttemptRecord {
        task: id.into(),
        tier: t.tier_initial,
        sha: String::new(),
        outcome: "dead".into(),
        lesson: why.into(),
        agent: ctx.agent(),
        at: Utc::now(),
    });
    let _ = git.release_lease(id, &ctx.agent(), false);
    eprintln!("loom: '{id}' marked dead (commit the task file)");
    Ok(())
}

pub fn probe_done(ctx: &Ctx, id: &str, accept: Vec<String>, context: Vec<String>) -> Result<()> {
    if accept.is_empty() {
        return Err(Error::Other(
            "probe must produce at least one acceptance test".into(),
        ));
    }
    let mut t = ctx.ws.load_task(id)?;
    t.accept = accept;
    if !context.is_empty() {
        t.context = context;
    }
    ctx.ws.save_task(&t)?;
    eprintln!("loom: '{id}' is now executable (commit the failing tests + task file)");
    Ok(())
}

/// Record a failed attempt as a conflict-free ref. Tier escalation is
/// implicit (derived from attempt count); exit 2 when the ladder is done.
pub fn attempt(ctx: &Ctx, id: &str, sha: &str, outcome: &str, lesson: &str) -> Result<()> {
    let snap = ctx.snapshot()?;
    let t = snap
        .tasks
        .get(id)
        .cloned()
        .or_else(|| ctx.ws.load_task(id).ok())
        .ok_or_else(|| Error::TaskNotFound(id.into()))?;
    let tier_now = protocol::current_tier(&t, &snap.attempts, &ctx.policy.budget.tiers_minutes);
    let git = ctx.git();
    git.record_attempt(&AttemptRecord {
        task: id.into(),
        tier: tier_now,
        sha: sha.into(),
        outcome: outcome.into(),
        lesson: lesson.into(),
        agent: ctx.agent(),
        at: Utc::now(),
    })?;
    let _ = git.release_lease(id, &ctx.agent(), false);
    let after = snap.attempts.get(id).map(Vec::len).unwrap_or(0) + 1;
    if protocol::ladder_exhausted(&t, after, &ctx.policy.budget.tiers_minutes) {
        return Err(Error::TierExhausted {
            task: id.into(),
            tier: tier_now,
        });
    }
    eprintln!(
        "loom: '{id}' attempt at tier {tier_now} recorded; derived tier is now {} — released",
        tier_now + 1
    );
    Ok(())
}

// ---------------------------------------------------------- verification

pub fn verify(
    ctx: &Ctx,
    id: &str,
    approve: bool,
    sha: Option<String>,
    lesson: Option<String>,
) -> Result<()> {
    let head = match sha {
        Some(s) => s,
        None => ctx.runner.run("git", &["rev-parse", "HEAD"])?,
    };
    let git = ctx.git();
    git.fetch_loom_refs()?;
    let agent = ctx.agent();
    let implementer = git.leases()?.get(id).map(|(p, _)| p.agent.clone());
    let self_verdict = implementer.as_deref() == Some(agent.as_str());
    if approve {
        git.publish_verdict(&Verdict {
            task: id.into(),
            sha: head.clone(),
            verdict: "approve".into(),
            agent: agent.clone(),
            self_verdict,
            at: Utc::now(),
        })?;
        eprintln!(
            "loom: verdict approve for '{id}' at {head} by {agent}{}",
            if self_verdict {
                " (SELF — blocked at done unless [verify] mode=any)"
            } else {
                ""
            }
        );
    } else {
        let lesson = lesson.ok_or_else(|| Error::Other("--lesson is required when rejecting (state the reasons; better: add them as failing acceptance tests)".into()))?;
        git.publish_verdict(&Verdict {
            task: id.into(),
            sha: head.clone(),
            verdict: "reject".into(),
            agent: agent.clone(),
            self_verdict,
            at: Utc::now(),
        })?;
        git.record_attempt(&AttemptRecord {
            task: id.into(),
            tier: 0,
            sha: head,
            outcome: "review-reject".into(),
            lesson,
            agent,
            at: Utc::now(),
        })?;
        eprintln!("loom: verdict reject for '{id}' recorded (also logged as an attempt)");
    }
    Ok(())
}

// ------------------------------------------------------------ escalations

#[allow(clippy::too_many_arguments)]
pub fn escalate(
    ctx: &Ctx,
    question: String,
    options: Vec<String>,
    recommend: String,
    evidence: Vec<String>,
    blocking: Vec<String>,
    deadline: Option<String>,
    default: Option<String>,
) -> Result<()> {
    if options.len() < 2 {
        return Err(Error::Other(
            "an escalation needs at least two options".into(),
        ));
    }
    if !options.contains(&recommend) {
        return Err(Error::Other(
            "--recommend must be one of the --option values".into(),
        ));
    }
    if let Some(d) = &default {
        if !options.contains(d) {
            return Err(Error::Other(
                "--default must be one of the --option values".into(),
            ));
        }
    }
    let deadline = match deadline {
        Some(s) => Some(
            s.parse::<chrono::DateTime<Utc>>()
                .map_err(|e| Error::Other(format!("bad --deadline '{s}': {e}")))?,
        ),
        None => None,
    };
    if default.is_some() && deadline.is_none() {
        return Err(Error::Other("--default requires --deadline".into()));
    }
    let e = Escalation {
        id: new_id(),
        question,
        options,
        recommend,
        evidence,
        blocking,
        deadline,
        default,
        answer: None,
        answered_by: None,
        created: Utc::now(),
    };
    ctx.ws.save_escalation(&e)?;
    println!("{}", json!({"id": e.id, "hard_block": e.default.is_none()}));
    eprintln!(
        "loom: escalation '{}' filed (commit .work/escalations/ so the oracle sees it)",
        e.id
    );
    Ok(())
}

pub fn escalations(ctx: &Ctx, apply_defaults: bool) -> Result<()> {
    let mut list = ctx.ws.load_all_escalations()?;
    if apply_defaults {
        let now = Utc::now();
        for e in list.iter_mut().filter(|e| e.is_open()) {
            if let (Some(deadline), Some(default)) = (e.deadline, e.default.clone()) {
                if now >= deadline {
                    e.answer = Some(default);
                    e.answered_by = Some("deadline-default".into());
                    ctx.ws.save_escalation(e)?;
                    eprintln!("loom: escalation '{}' answered by deadline default", e.id);
                }
            }
        }
    }
    let rows: Vec<_> = list
        .iter()
        .map(|e| {
            json!({
                "id": e.id, "open": e.is_open(), "question": e.question,
                "blocking": e.blocking, "deadline": e.deadline, "default": e.default,
                "answer": e.answer,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

// ---------------------------------------------------------------- sweep

pub fn sweep(ctx: &Ctx) -> Result<()> {
    let git = ctx.git();
    git.fetch_loom_refs()?;
    let now = Utc::now();
    let mut reclaimed = 0;
    for (task, (payload, _sha)) in git.leases()? {
        if task == "merge-lock" {
            continue;
        }
        if protocol::lease_is_stale(&payload, now, ctx.policy.lease.ttl_minutes) {
            git.release_lease(&task, &ctx.agent(), true)?;
            // Fleet-visible record of the reclaim, so resumers see it.
            let _ = git.record_attempt(&AttemptRecord {
                task: task.clone(),
                tier: payload.tier,
                sha: String::new(),
                outcome: "lease-swept".into(),
                lesson: format!(
                    "lease held by {} went silent (last heartbeat {})",
                    payload.agent, payload.heartbeat
                ),
                agent: ctx.agent(),
                at: now,
            });
            eprintln!(
                "loom: reclaimed stale lease '{task}' (held by {})",
                payload.agent
            );
            reclaimed += 1;
        }
    }
    eprintln!("loom: sweep complete, {reclaimed} lease(s) reclaimed");
    Ok(())
}

// ----------------------------------------------------------------- lock

pub fn lock_acquire(ctx: &Ctx) -> Result<()> {
    ctx.git().acquire_merge_lock(&ctx.agent())?;
    eprintln!("loom: merge lock acquired — rebase, re-verify, merge, then `loom lock release`");
    Ok(())
}

pub fn lock_release(ctx: &Ctx) -> Result<()> {
    ctx.git().release_merge_lock()?;
    eprintln!("loom: merge lock released");
    Ok(())
}

// ------------------------------------------------------------- telemetry

pub fn telemetry(ctx: &Ctx, commit: &str, record_json: &str) -> Result<()> {
    let mut record: Value = serde_json::from_str(record_json)
        .map_err(|e| Error::Other(format!("--json must be valid JSON: {e}")))?;
    if let Some(obj) = record.as_object_mut() {
        obj.insert("agent".into(), json!(ctx.agent()));
        obj.insert("at".into(), json!(Utc::now()));
    }
    ctx.git().record_telemetry(commit, &record)
}

/// Knowledge files already on disk, repo-relative and sorted. Feeds retro so a
/// candidate can say whether its file exists — append versus create.
fn existing_knowledge(ctx: &Ctx) -> Vec<String> {
    let dir = ctx.ws.root.join(KNOWLEDGE_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
        .filter_map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| format!("{KNOWLEDGE_DIR}/{n}"))
        })
        .collect();
    out.sort();
    out
}

/// The read half of the learning loop: aggregate telemetry + attempts,
/// emit a report with mechanical policy suggestions.
pub fn retro(ctx: &Ctx) -> Result<()> {
    let git = ctx.git();
    git.fetch_loom_refs()?;
    let records = git.telemetry_records()?;
    let attempts = git.attempts()?;
    let report = protocol::retro(&records, &attempts, &ctx.policy, &existing_knowledge(ctx));
    println!("{}", serde_json::to_string_pretty(&report)?);
    eprintln!("loom: apply accepted suggestions as a reviewed diff to .work/policy.toml — retros that don't change config didn't happen");
    Ok(())
}

// -------------------------------------------------------------- context

pub fn context(ctx: &Ctx, id: &str) -> Result<()> {
    let t = ctx.ws.load_task(id).or_else(|_| {
        let (tasks, _) = ctx.canonical_tasks()?;
        tasks
            .get(id)
            .cloned()
            .ok_or_else(|| Error::TaskNotFound(id.into()))
    })?;
    let rows: Vec<_> = t
        .context
        .iter()
        .map(|entry| {
            if let Some(rest) = entry.strip_prefix("git:") {
                json!({"ref": rest, "kind": "git", "hint": "git show / git diff"})
            } else {
                let p = ctx.ws.root.join(entry);
                json!({"path": entry, "kind": "file", "exists": p.exists(),
                       "bytes": p.metadata().map(|m| m.len()).unwrap_or(0)})
            }
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "id": t.id, "goal": t.goal, "contract": t.contract,
            "accept": t.accept, "context": rows,
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::mock::{MockRunner, Step};

    // Tests below mutate the process-wide LOOM_AGENT env var, which races
    // under cargo test's default parallel execution. Serialize them.
    static LOOM_AGENT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn lock_agent_env() -> std::sync::MutexGuard<'static, ()> {
        LOOM_AGENT_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".work/tasks")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".work/escalations")).unwrap();
        let ws = Workspace::discover(tmp.path()).unwrap();
        (tmp, ws)
    }

    fn seed(ws: &Workspace, id: &str, accept: bool) {
        ws.save_task(&Task {
            id: id.into(),
            goal: "g".into(),
            state: TaskState::Open,
            value: 1.0,
            deps: vec![],
            contract: vec![],
            accept: if accept { vec!["t.rs".into()] } else { vec![] },
            context: vec![],
            tier_initial: 0,
            created: Utc::now(),
        })
        .unwrap();
    }

    #[test]
    fn done_is_gated_atomic_and_rolls_back_on_commit_failure() {
        let _guard = lock_agent_env();
        std::env::set_var("LOOM_AGENT", "impl-agent");
        let (_tmp, ws) = workspace();
        seed(&ws, "abc", true);
        std::fs::write(ws.root.join("t.rs"), "test").unwrap(); // accept test exists
        let approve = r#"refs/loom/mirror/verdict/abc"#;

        // 1) VerdictMissing (mode independent, no verdict) → exit 5, no writes.
        let r = MockRunner::new(vec![
            ("git rev-parse HEAD", Step::Ok("headsha")),
            ("git fetch origin --prune", Step::Ok("")),
            ("git for-each-ref", Step::Ok("")), // no verdict
        ]);
        let ctx = Ctx {
            runner: &r,
            ws: Workspace {
                root: ws.root.clone(),
            },
            policy: Policy::default(),
        };
        let err = done(&ctx, "abc", None).unwrap_err();
        assert_eq!(err.exit_code(), 5);
        r.assert_done();

        // 2) Self-verdict blocked in independent mode.
        let selfv = format!("{approve}\0s\0{{\"task\":\"abc\",\"sha\":\"headsha\",\"verdict\":\"approve\",\"agent\":\"impl-agent\",\"self_verdict\":true,\"at\":\"2026-07-01T00:00:00Z\"}}");
        let r2 = MockRunner::new(vec![
            ("git rev-parse HEAD", Step::Ok("headsha")),
            ("git fetch origin --prune", Step::Ok("")),
            (
                "git for-each-ref",
                Step::Ok(Box::leak(selfv.into_boxed_str())),
            ),
        ]);
        let ctx2 = Ctx {
            runner: &r2,
            ws: Workspace {
                root: ws.root.clone(),
            },
            policy: Policy::default(),
        };
        assert!(matches!(
            done(&ctx2, "abc", None).unwrap_err(),
            Error::VerdictNotIndependent { .. }
        ));
        r2.assert_done();

        // 3) Valid independent verdict, but the COMMIT FAILS → file restored,
        //    typed DoneNotCommitted. The v1 drift bug is structurally closed.
        let okv = format!("{approve}\0s\0{{\"task\":\"abc\",\"sha\":\"headsha\",\"verdict\":\"approve\",\"agent\":\"reviewer\",\"self_verdict\":false,\"at\":\"2026-07-01T00:00:00Z\"}}");
        let before = std::fs::read_to_string(ws.task_path("abc")).unwrap();
        let r3 = MockRunner::new(vec![
            ("git rev-parse HEAD", Step::Ok("headsha")),
            ("git fetch origin --prune", Step::Ok("")),
            (
                "git for-each-ref",
                Step::Ok(Box::leak(okv.clone().into_boxed_str())),
            ),
            ("git add -A", Step::Ok("")),
            ("git commit -m", Step::Fail("pre-commit hook failed")),
        ]);
        let ctx3 = Ctx {
            runner: &r3,
            ws: Workspace {
                root: ws.root.clone(),
            },
            policy: Policy::default(),
        };
        let err = done(&ctx3, "abc", None).unwrap_err();
        assert!(matches!(err, Error::DoneNotCommitted { .. }));
        let after = std::fs::read_to_string(ws.task_path("abc")).unwrap();
        assert_eq!(
            before, after,
            "task file must be rolled back on commit failure"
        );
        assert!(after.contains("state = \"open\""));
        r3.assert_done();

        // 4) Happy path: verdict ok, commit ok, lease release best-effort.
        let r4 = MockRunner::new(vec![
            ("git rev-parse HEAD", Step::Ok("headsha")),
            ("git fetch origin --prune", Step::Ok("")),
            (
                "git for-each-ref",
                Step::Ok(Box::leak(okv.into_boxed_str())),
            ),
            ("git add -A", Step::Ok("")),
            ("git commit -m", Step::Ok("committed")),
            ("git fetch origin --prune", Step::Ok("")),
            ("git for-each-ref", Step::Ok("")), // release: no lease found, fine
        ]);
        let ctx4 = Ctx {
            runner: &r4,
            ws: Workspace {
                root: ws.root.clone(),
            },
            policy: Policy::default(),
        };
        done(&ctx4, "abc", Some("msg".into())).unwrap();
        assert!(std::fs::read_to_string(ws.task_path("abc"))
            .unwrap()
            .contains("state = \"done\""));
        r4.assert_done();
    }

    #[test]
    fn attempt_records_ref_and_exit2_at_ladder_end() {
        let _guard = lock_agent_env();
        std::env::set_var("LOOM_AGENT", "a");
        let (_tmp, ws) = workspace();
        seed(&ws, "abc", true);
        // snapshot: fetch, leases(for-each-ref), attempts(for-each-ref),
        // canonical fetch fails → worktree fallback; then attempt ref pushes.
        let two_attempts = "refs/loom/mirror/attempt/abc/x\0s\0{\"task\":\"abc\",\"tier\":0,\"sha\":\"a\",\"outcome\":\"tests-red\",\"lesson\":\"1\",\"agent\":\"a\",\"at\":\"2026-07-01T00:00:00Z\"}\nrefs/loom/mirror/attempt/abc/y\0s\0{\"task\":\"abc\",\"tier\":1,\"sha\":\"b\",\"outcome\":\"tests-red\",\"lesson\":\"2\",\"agent\":\"a\",\"at\":\"2026-07-02T00:00:00Z\"}";
        let r = MockRunner::new(vec![
            ("git fetch origin --prune", Step::Ok("")),
            ("git for-each-ref", Step::Ok("")),           // leases
            ("git for-each-ref", Step::Ok(two_attempts)), // attempts
            ("git fetch origin main", Step::Fail("no ref")), // canonical → fallback
            ("git mktree", Step::Ok("tree")),
            ("git commit-tree", Step::Ok("csha")),
            ("git push origin csha:refs/loom/attempt/abc/", Step::Ok("")),
            ("git fetch origin --prune", Step::Ok("")), // release
            ("git for-each-ref", Step::Ok("")),
        ]);
        let ctx = Ctx {
            runner: &r,
            ws,
            policy: Policy::default(),
        };
        // third failure on a 3-tier ladder ⇒ exhausted
        let err = attempt(&ctx, "abc", "c", "tests-red", "3").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        r.assert_done();
    }

    #[test]
    fn canonical_graph_is_used_when_remote_main_exists() {
        let _guard = lock_agent_env();
        std::env::set_var("LOOM_AGENT", "a");
        let (_tmp, ws) = workspace();
        // Worktree has a DIFFERENT (mutated) copy — must be ignored.
        seed(&ws, "abc", true);
        let canonical_blob = "id=\"abc\"\ngoal=\"canonical goal\"\nstate=\"open\"\naccept=[\"t.rs\"]\ncreated=\"2026-07-01T00:00:00Z\"\n";
        let r = MockRunner::new(vec![
            ("git fetch origin --prune", Step::Ok("")),
            ("git for-each-ref", Step::Ok("")), // leases
            ("git for-each-ref", Step::Ok("")), // attempts
            ("git fetch origin main", Step::Ok("")),
            (
                "git ls-tree -r --name-only origin/main -- .work/tasks",
                Step::Ok(".work/tasks/abc.toml"),
            ),
            (
                "git show origin/main:.work/tasks/abc.toml",
                Step::Ok(Box::leak(canonical_blob.to_string().into_boxed_str())),
            ),
        ]);
        let ctx = Ctx {
            runner: &r,
            ws,
            policy: Policy::default(),
        };
        let snap = ctx.snapshot().unwrap();
        assert!(snap.canonical);
        assert_eq!(snap.tasks["abc"].goal, "canonical goal"); // not the worktree copy
        r.assert_done();
    }

    #[test]
    fn escalation_deadline_defaults_still_apply() {
        let _guard = lock_agent_env();
        std::env::set_var("LOOM_AGENT", "a");
        let (_tmp, ws) = workspace();
        ws.save_escalation(&Escalation {
            id: "e1".into(),
            question: "q".into(),
            options: vec!["a".into(), "b".into()],
            recommend: "a".into(),
            evidence: vec![],
            blocking: vec![],
            deadline: Some("2020-01-01T00:00:00Z".parse().unwrap()),
            default: Some("a".into()),
            answer: None,
            answered_by: None,
            created: "2019-12-01T00:00:00Z".parse().unwrap(),
        })
        .unwrap();
        let r = MockRunner::new(vec![]);
        let ctx = Ctx {
            runner: &r,
            ws,
            policy: Policy::default(),
        };
        escalations(&ctx, true).unwrap();
        let e = &ctx.ws.load_all_escalations().unwrap()[0];
        assert_eq!(e.answer.as_deref(), Some("a"));
        r.assert_done();
    }
}
