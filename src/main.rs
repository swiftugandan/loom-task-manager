//! loom v2 — agent-native task management on a git substrate.

mod commands;
mod error;
mod git;
mod model;
mod protocol;
mod runner;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::commands::Ctx;
use crate::model::Workspace;
use crate::runner::SystemRunner;

const AFTER_HELP: &str = "\
Exit codes:
  0  success
  1  error (protocol violation, git failure, bad input)
  2  budget tier exhausted — decompose or escalate; grinding is not an option
  3  lease race lost — pick another task
  4  blocked on the human oracle (open escalation with no default)
  5  verification gate — no valid independent verdict for this candidate

Environment:
  LOOM_AGENT    Agent identity for leases/verdicts/telemetry (default: git user.email)
  LOOM_REMOTE   Git remote for coordination refs (default: origin)
  LOOM_MAIN     Canonical branch the graph is read from (default: main)";

#[derive(Parser)]
#[command(
    name = "loom",
    version,
    about = "Agent-native task management: canonical git graph, CAS leases, conflict-free attempt refs, derived state and tiers, structural timeboxes, independent-verdict done gate, retro loop",
    after_help = AFTER_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize .work/ (tasks, escalations, policy) and knowledge/ here
    Init,
    /// Verify git repo, remote, canonical graph, dep integrity, policy, knowledge/
    Doctor,
    /// Create a task file in .work/tasks/ (prints its id)
    TaskCreate {
        #[arg(long)]
        goal: String,
        #[arg(long, default_value_t = 1.0)]
        value: f64,
        #[arg(long = "dep")]
        deps: Vec<String>,
        #[arg(long)]
        contract: Vec<String>,
        /// Failing acceptance test path(s). Omit ⇒ task needs a probe first.
        #[arg(long)]
        accept: Vec<String>,
        #[arg(long)]
        context: Vec<String>,
        /// Starting tier; the CURRENT tier is derived as tier + failed attempts
        #[arg(long, default_value_t = 0)]
        tier: usize,
    },
    /// List all tasks with derived state and derived tier (canonical graph)
    Tasks,
    /// Show one task: derived state/tier, attempts with lessons, unblock count
    Show { id: String },
    /// Print the best schedulable task (or all candidates with --all)
    Next {
        #[arg(long)]
        all: bool,
    },
    /// Acquire the exclusive work lease (atomic CAS; exit 3 on race, 4 on oracle block)
    Lease { id: String },
    /// Heartbeat the lease. --daemon detaches a background heartbeater that
    /// stops at the tier budget's end, making the timebox structural.
    Heartbeat {
        id: String,
        #[arg(long)]
        daemon: bool,
        #[arg(long, hide = true)]
        daemon_loop: bool,
    },
    /// Lease clock: elapsed vs budget, remaining minutes, current verdict
    Status { id: String },
    /// Release your lease without finishing (work stays on the branch)
    Release { id: String },
    /// Record a probe's output: acceptance tests + tightened context manifest
    ProbeDone {
        id: String,
        #[arg(long, required = true)]
        accept: Vec<String>,
        #[arg(long)]
        context: Vec<String>,
    },
    /// Record a failed attempt as a conflict-free ref (tier escalates by
    /// derivation; exit 2 when the ladder is exhausted)
    Attempt {
        id: String,
        #[arg(long)]
        sha: String,
        #[arg(long)]
        outcome: String,
        #[arg(long)]
        lesson: String,
    },
    /// Publish a verdict for the candidate at HEAD (or --sha). Rejections
    /// require --lesson and are logged as attempts.
    Verify {
        id: String,
        #[arg(long, conflicts_with = "reject")]
        approve: bool,
        #[arg(long)]
        reject: bool,
        #[arg(long)]
        sha: Option<String>,
        #[arg(long)]
        lesson: Option<String>,
    },
    /// Gate + flip state=done + commit code, tests, and state ATOMICALLY.
    /// Fails (exit 5) without a valid verdict; rolls back if the commit fails.
    Done {
        id: String,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Mark a task dead with a reason (superseded, invalidated, decomposed)
    Dead {
        id: String,
        #[arg(long)]
        why: String,
    },
    /// File a typed question for the human oracle
    Escalate {
        #[arg(long)]
        question: String,
        #[arg(long = "option", required = true)]
        options: Vec<String>,
        #[arg(long)]
        recommend: String,
        #[arg(long)]
        evidence: Vec<String>,
        #[arg(long)]
        blocking: Vec<String>,
        #[arg(long)]
        deadline: Option<String>,
        #[arg(long)]
        default: Option<String>,
    },
    /// List escalations; --apply-defaults answers past-deadline ones
    Escalations {
        #[arg(long)]
        apply_defaults: bool,
    },
    /// Reclaim stale leases (also logs a fleet-visible 'lease-swept' attempt)
    Sweep,
    /// Take/release the integration serialization lock
    Lock {
        #[command(subcommand)]
        op: LockOp,
    },
    /// Append a structured telemetry record (git note) to a commit
    Telemetry {
        commit: String,
        #[arg(long)]
        json: String,
    },
    /// Read the telemetry + attempt history and emit a report: mechanical
    /// policy suggestions plus knowledge/ file candidates
    Retro,
    /// Print a task's hydration manifest: context files with existence/size,
    /// plus a standing index of knowledge/
    Context { id: String },
}

#[derive(Subcommand)]
enum LockOp {
    Acquire,
    Release,
}

fn run() -> error::Result<()> {
    let cli = Cli::parse();
    let runner = SystemRunner;

    if matches!(cli.command, Cmd::Init) {
        return commands::init(&runner);
    }

    let cwd = std::env::current_dir()?;
    let ws = Workspace::discover(&cwd)?;
    let policy = ws.load_policy()?;
    let ctx = Ctx {
        runner: &runner,
        ws,
        policy,
    };

    match cli.command {
        Cmd::Init => unreachable!(),
        Cmd::Doctor => commands::doctor(&ctx),
        Cmd::TaskCreate {
            goal,
            value,
            deps,
            contract,
            accept,
            context,
            tier,
        } => commands::task_create(&ctx, goal, value, deps, contract, accept, context, tier),
        Cmd::Tasks => commands::tasks(&ctx),
        Cmd::Show { id } => commands::show(&ctx, &id),
        Cmd::Next { all } => commands::next(&ctx, all),
        Cmd::Lease { id } => commands::lease(&ctx, &id),
        Cmd::Heartbeat {
            id,
            daemon,
            daemon_loop,
        } => commands::heartbeat(&ctx, &id, daemon, daemon_loop),
        Cmd::Status { id } => commands::status(&ctx, &id),
        Cmd::Release { id } => commands::release(&ctx, &id),
        Cmd::ProbeDone {
            id,
            accept,
            context,
        } => commands::probe_done(&ctx, &id, accept, context),
        Cmd::Attempt {
            id,
            sha,
            outcome,
            lesson,
        } => commands::attempt(&ctx, &id, &sha, &outcome, &lesson),
        Cmd::Verify {
            id,
            approve,
            reject,
            sha,
            lesson,
        } => {
            if approve == reject {
                return Err(error::Error::Other(
                    "pass exactly one of --approve / --reject".into(),
                ));
            }
            commands::verify(&ctx, &id, approve, sha, lesson)
        }
        Cmd::Done { id, message } => commands::done(&ctx, &id, message),
        Cmd::Dead { id, why } => commands::dead(&ctx, &id, &why),
        Cmd::Escalate {
            question,
            options,
            recommend,
            evidence,
            blocking,
            deadline,
            default,
        } => commands::escalate(
            &ctx, question, options, recommend, evidence, blocking, deadline, default,
        ),
        Cmd::Escalations { apply_defaults } => commands::escalations(&ctx, apply_defaults),
        Cmd::Sweep => commands::sweep(&ctx),
        Cmd::Lock { op } => match op {
            LockOp::Acquire => commands::lock_acquire(&ctx),
            LockOp::Release => commands::lock_release(&ctx),
        },
        Cmd::Telemetry { commit, json } => commands::telemetry(&ctx, &commit, &json),
        Cmd::Retro => commands::retro(&ctx),
        Cmd::Context { id } => commands::context(&ctx, &id),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("loom: error: {e}");
            ExitCode::from(e.exit_code() as u8)
        }
    }
}
