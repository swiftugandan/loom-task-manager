//! Error taxonomy for loom.
//!
//! Exit-code contract (agents script against this):
//!   0 = success
//!   1 = error (protocol violation, git failure, bad input)
//!   2 = budget tier exhausted — the task must be decomposed or escalated
//!   3 = lease race lost — pick another task
//!   4 = blocked on the human oracle (open escalation with no default)
//!   5 = verification gate — no valid independent verdict for this candidate

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("missing dependency: `{0}` not found on PATH")]
    MissingDependency(&'static str),

    #[error("not inside a loom workspace: no .work/ found upward from {0} (run `loom init`)")]
    WorkspaceNotFound(String),

    #[error("task '{0}' not found")]
    TaskNotFound(String),

    #[error("invalid {what} file {path}: {msg}")]
    Parse {
        what: &'static str,
        path: String,
        msg: String,
    },

    #[error("lease race lost for task '{task}': held by '{holder}' — pick another task")]
    LeaseRaceLost { task: String, holder: String },

    #[error("lease for '{task}' is held by '{holder}', not you")]
    NotLeaseHolder { task: String, holder: String },

    #[error("no lease exists for task '{0}'")]
    NoLease(String),

    #[error("budget tier {tier} is the last tier for task '{task}': decompose or escalate")]
    TierExhausted { task: String, tier: usize },

    #[error("task '{task}' is blocked on open escalation '{escalation}' with no default")]
    OracleBlocked { task: String, escalation: String },

    #[error("no approving verdict for '{task}' at {sha}: run `loom verify` from an independent agent (or set [verify] mode in policy)")]
    VerdictMissing { task: String, sha: String },

    #[error("verdict for '{task}' was issued by the implementer '{agent}'; [verify] mode = \"independent\" requires a different agent")]
    VerdictNotIndependent { task: String, agent: String },

    #[error("verdict for '{task}' is bound to {verdict_sha}, but HEAD is {head} — re-verify the current candidate")]
    VerdictStale {
        task: String,
        verdict_sha: String,
        head: String,
    },

    #[error("`loom done` could not commit atomically for '{task}': {msg} (task file restored; nothing was marked done)")]
    DoneNotCommitted { task: String, msg: String },

    #[error("nothing schedulable")]
    NothingSchedulable,

    #[error("git {args:?} failed (exit {code:?}): {stderr}")]
    Git {
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },

    #[error("unexpected output shape: {context}")]
    Shape { context: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::TierExhausted { .. } => 2,
            Error::LeaseRaceLost { .. } => 3,
            Error::OracleBlocked { .. } => 4,
            Error::VerdictMissing { .. }
            | Error::VerdictNotIndependent { .. }
            | Error::VerdictStale { .. } => 5,
            _ => 1,
        }
    }
}
