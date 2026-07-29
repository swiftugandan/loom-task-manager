//! Git-native coordination primitives.
//!
//! v2 moves ALL mutable fleet state into refs so `.work/` can never merge-
//! conflict: leases (`refs/loom/lease/<task>`), append-only attempts
//! (`refs/loom/attempt/<task>/<id>` — unique ids, conflict-free by
//! construction), verdicts (`refs/loom/verdict/<task>`, bound to a candidate
//! sha), and the merge lock. Payloads are JSON in commit messages over the
//! empty tree; creation/update/delete use `--force-with-lease`, a true
//! server-side compare-and-swap. Telemetry is git notes.

use std::collections::BTreeMap;

use chrono::Utc;

use crate::error::{Error, Result};
use crate::model::{new_id, AttemptRecord, LeasePayload, Verdict};
use crate::runner::Runner;

pub const LEASE_PREFIX: &str = "refs/loom/lease/";
pub const ATTEMPT_PREFIX: &str = "refs/loom/attempt/";
pub const VERDICT_PREFIX: &str = "refs/loom/verdict/";
pub const MIRROR: &str = "refs/loom/mirror/"; // local mirror of all remote loom refs
pub const MERGE_LOCK: &str = "refs/loom/merge-lock";
pub const NOTES_REF: &str = "refs/notes/loom-telemetry";

pub struct Git<'a> {
    pub runner: &'a dyn Runner,
    pub remote: String,
}

impl<'a> Git<'a> {
    pub fn new(runner: &'a dyn Runner) -> Self {
        Self {
            runner,
            remote: std::env::var("LOOM_REMOTE").unwrap_or_else(|_| "origin".into()),
        }
    }

    fn payload_commit(&self, payload: &str) -> Result<String> {
        let tree = self.runner.run("git", &["mktree"])?;
        let tree = if tree.is_empty() {
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()
        } else {
            tree
        };
        self.runner
            .run("git", &["commit-tree", &tree, "-m", payload])
    }

    fn cas_create(&self, refname: &str, payload: &str) -> Result<()> {
        let sha = self.payload_commit(payload)?;
        self.runner.run(
            "git",
            &[
                "push",
                &self.remote,
                &format!("{sha}:{refname}"),
                &format!("--force-with-lease={refname}:"),
            ],
        )?;
        Ok(())
    }

    fn cas_update(&self, refname: &str, expected_sha: &str, payload: &str) -> Result<()> {
        let sha = self.payload_commit(payload)?;
        self.runner.run(
            "git",
            &[
                "push",
                &self.remote,
                &format!("{sha}:{refname}"),
                &format!("--force-with-lease={refname}:{expected_sha}"),
            ],
        )?;
        Ok(())
    }

    fn cas_delete(&self, refname: &str, expected_sha: &str) -> Result<()> {
        self.runner.run(
            "git",
            &[
                "push",
                &self.remote,
                &format!(":{refname}"),
                &format!("--force-with-lease={refname}:{expected_sha}"),
            ],
        )?;
        Ok(())
    }

    /// Mirror all remote loom refs locally (leases, attempts, verdicts).
    pub fn fetch_loom_refs(&self) -> Result<()> {
        self.runner.run(
            "git",
            &[
                "fetch",
                &self.remote,
                "--prune",
                &format!("+refs/loom/*:{MIRROR}*"),
            ],
        )?;
        Ok(())
    }

    /// Read mirrored refs under a loom prefix: suffix -> (message, sha).
    fn read_mirror(&self, prefix: &str) -> Result<Vec<(String, String, String)>> {
        let mirror_prefix = format!("{MIRROR}{}", prefix.trim_start_matches("refs/loom/"));
        let out = self
            .runner
            .try_run(
                "git",
                &[
                    "for-each-ref",
                    "--format=%(refname)%00%(objectname)%00%(contents:subject)%(contents:body)",
                    &mirror_prefix,
                ],
            )?
            .unwrap_or_default();
        let mut rows = Vec::new();
        for line in out.lines().filter(|l| !l.trim().is_empty()) {
            let mut parts = line.splitn(3, '\0');
            if let (Some(refname), Some(sha), Some(msg)) =
                (parts.next(), parts.next(), parts.next())
            {
                let suffix = refname.trim_start_matches(&mirror_prefix).to_string();
                rows.push((suffix, sha.to_string(), msg.trim().to_string()));
            }
        }
        Ok(rows)
    }

    // ------------------------------------------------------------- leases

    /// task-id -> (payload, sha)
    pub fn leases(&self) -> Result<BTreeMap<String, (LeasePayload, String)>> {
        let mut map = BTreeMap::new();
        for (task, sha, msg) in self.read_mirror(LEASE_PREFIX)? {
            let payload: LeasePayload = serde_json::from_str(&msg).map_err(|e| Error::Shape {
                context: format!("lease payload for {task}: {e}"),
            })?;
            map.insert(task, (payload, sha));
        }
        Ok(map)
    }

    pub fn acquire_lease(&self, task: &str, agent: &str, tier: usize) -> Result<()> {
        let now = Utc::now();
        let payload = serde_json::to_string(&LeasePayload {
            agent: agent.to_string(),
            task: task.to_string(),
            tier,
            acquired: Some(now),
            heartbeat: now,
        })?;
        self.cas_create(&format!("{LEASE_PREFIX}{task}"), &payload)
            .map_err(|e| match e {
                Error::Git { .. } => Error::LeaseRaceLost {
                    task: task.to_string(),
                    holder: "another agent".into(),
                },
                other => other,
            })
    }

    pub fn heartbeat_lease(&self, task: &str, agent: &str) -> Result<()> {
        self.fetch_loom_refs()?;
        let leases = self.leases()?;
        let (payload, sha) = leases
            .get(task)
            .ok_or_else(|| Error::NoLease(task.to_string()))?;
        if payload.agent != agent {
            return Err(Error::NotLeaseHolder {
                task: task.to_string(),
                holder: payload.agent.clone(),
            });
        }
        let new_payload = serde_json::to_string(&LeasePayload {
            agent: agent.to_string(),
            task: task.to_string(),
            tier: payload.tier,
            acquired: payload.acquired,
            heartbeat: Utc::now(),
        })?;
        self.cas_update(&format!("{LEASE_PREFIX}{task}"), sha, &new_payload)
    }

    pub fn release_lease(&self, task: &str, agent: &str, force: bool) -> Result<()> {
        self.fetch_loom_refs()?;
        let leases = self.leases()?;
        let (payload, sha) = leases
            .get(task)
            .ok_or_else(|| Error::NoLease(task.to_string()))?;
        if payload.agent != agent && !force {
            return Err(Error::NotLeaseHolder {
                task: task.to_string(),
                holder: payload.agent.clone(),
            });
        }
        self.cas_delete(&format!("{LEASE_PREFIX}{task}"), sha)
    }

    // ----------------------------------------------------------- attempts

    /// Append an attempt record: unique ref name ⇒ conflict-free, instantly
    /// fleet-visible, independent of any branch.
    pub fn record_attempt(&self, rec: &AttemptRecord) -> Result<()> {
        let payload = serde_json::to_string(rec)?;
        self.cas_create(
            &format!("{ATTEMPT_PREFIX}{}/{}", rec.task, new_id()),
            &payload,
        )
    }

    /// task-id -> attempts (chronological).
    pub fn attempts(&self) -> Result<BTreeMap<String, Vec<AttemptRecord>>> {
        let mut map: BTreeMap<String, Vec<AttemptRecord>> = BTreeMap::new();
        for (suffix, _sha, msg) in self.read_mirror(ATTEMPT_PREFIX)? {
            let task = suffix.split('/').next().unwrap_or(&suffix).to_string();
            let rec: AttemptRecord = serde_json::from_str(&msg).map_err(|e| Error::Shape {
                context: format!("attempt payload {suffix}: {e}"),
            })?;
            map.entry(task).or_default().push(rec);
        }
        for v in map.values_mut() {
            v.sort_by_key(|r| r.at);
        }
        Ok(map)
    }

    // ----------------------------------------------------------- verdicts

    /// Publish (or replace) the verdict for a task. Verifier authority:
    /// unconditional force update.
    pub fn publish_verdict(&self, v: &Verdict) -> Result<()> {
        let payload = serde_json::to_string(v)?;
        let sha = self.payload_commit(&payload)?;
        self.runner.run(
            "git",
            &[
                "push",
                "--force",
                &self.remote,
                &format!("{sha}:{VERDICT_PREFIX}{}", v.task),
            ],
        )?;
        Ok(())
    }

    pub fn verdict(&self, task: &str) -> Result<Option<Verdict>> {
        for (suffix, _sha, msg) in self.read_mirror(VERDICT_PREFIX)? {
            if suffix == task {
                let v: Verdict = serde_json::from_str(&msg).map_err(|e| Error::Shape {
                    context: format!("verdict payload {task}: {e}"),
                })?;
                return Ok(Some(v));
            }
        }
        Ok(None)
    }

    // ------------------------------------------------------------- lock

    pub fn acquire_merge_lock(&self, agent: &str) -> Result<()> {
        let now = Utc::now();
        let payload = serde_json::to_string(&LeasePayload {
            agent: agent.to_string(),
            task: "merge-lock".into(),
            tier: 0,
            acquired: Some(now),
            heartbeat: now,
        })?;
        self.cas_create(MERGE_LOCK, &payload).map_err(|e| match e {
            Error::Git { .. } => Error::LeaseRaceLost {
                task: "merge-lock".into(),
                holder: "another agent".into(),
            },
            other => other,
        })
    }

    pub fn release_merge_lock(&self) -> Result<()> {
        self.runner
            .run("git", &["push", &self.remote, &format!(":{MERGE_LOCK}")])?;
        Ok(())
    }

    // --------------------------------------------------------- telemetry

    pub fn record_telemetry(&self, commit: &str, record: &serde_json::Value) -> Result<()> {
        let body = serde_json::to_string(record)?;
        self.runner.run(
            "git",
            &["notes", "--ref", NOTES_REF, "append", "-m", &body, commit],
        )?;
        let _ = self.runner.try_run(
            "git",
            &["push", &self.remote, &format!("{NOTES_REF}:{NOTES_REF}")],
        )?;
        Ok(())
    }

    /// All telemetry note bodies (one JSON object per line across all notes).
    pub fn telemetry_records(&self) -> Result<Vec<serde_json::Value>> {
        // Pull remote notes best-effort so retro sees the fleet, not just us.
        let _ = self.runner.try_run(
            "git",
            &["fetch", &self.remote, &format!("+{NOTES_REF}:{NOTES_REF}")],
        )?;
        let list = self
            .runner
            .try_run("git", &["notes", "--ref", NOTES_REF, "list"])?
            .unwrap_or_default();
        let mut out = Vec::new();
        for line in list.lines() {
            let Some(note_obj) = line.split_whitespace().next() else {
                continue;
            };
            let body = self.runner.run("git", &["cat-file", "blob", note_obj])?;
            for l in body.lines().filter(|l| !l.trim().is_empty()) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                    out.push(v);
                }
            }
        }
        Ok(out)
    }

    /// Canonical graph source: fetch main and read task files from it, never
    /// from the (possibly branch-mutated) worktree.
    pub fn canonical_task_blobs(&self, main_ref: &str) -> Result<Option<Vec<(String, String)>>> {
        if self
            .runner
            .try_run("git", &["fetch", &self.remote, main_ref])?
            .is_none()
        {
            return Ok(None); // remote main missing (fresh repo) — caller falls back
        }
        let rev = format!("{}/{}", self.remote, main_ref);
        let Some(listing) = self.runner.try_run(
            "git",
            &["ls-tree", "-r", "--name-only", &rev, "--", ".work/tasks"],
        )?
        else {
            return Ok(None);
        };
        let mut out = Vec::new();
        for path in listing.lines().filter(|p| p.ends_with(".toml")) {
            let blob = self
                .runner
                .run("git", &["show", &format!("{rev}:{path}")])?;
            out.push((path.to_string(), blob));
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::mock::{MockRunner, Step};

    #[test]
    fn acquire_lease_is_one_cas_push_and_maps_race() {
        let r = MockRunner::new(vec![
            ("git mktree", Step::Ok("4b825dc642cb6eb9a060e54bf8d69288fbee4904")),
            ("git commit-tree", Step::Ok("deadbeef")),
            ("git push origin deadbeef:refs/loom/lease/7f3a --force-with-lease=refs/loom/lease/7f3a:", Step::Ok("")),
        ]);
        let g = Git {
            runner: &r,
            remote: "origin".into(),
        };
        g.acquire_lease("7f3a", "agent-1", 0).unwrap();
        r.assert_done();

        let r2 = MockRunner::new(vec![
            (
                "git mktree",
                Step::Ok("4b825dc642cb6eb9a060e54bf8d69288fbee4904"),
            ),
            ("git commit-tree", Step::Ok("deadbeef")),
            ("git push", Step::Fail("[rejected] (stale info)")),
        ]);
        let g2 = Git {
            runner: &r2,
            remote: "origin".into(),
        };
        let err = g2.acquire_lease("7f3a", "agent-2", 0).unwrap_err();
        assert_eq!(err.exit_code(), 3);
        r2.assert_done();
    }

    #[test]
    fn attempts_group_and_sort_by_task() {
        let r = MockRunner::new(vec![(
            "git for-each-ref",
            Step::Ok(concat!(
                "refs/loom/mirror/attempt/t1/b\0s1\0{\"task\":\"t1\",\"tier\":1,\"sha\":\"y\",\"outcome\":\"tests-red\",\"lesson\":\"l2\",\"agent\":\"a\",\"at\":\"2026-07-02T00:00:00Z\"}\n",
                "refs/loom/mirror/attempt/t1/a\0s2\0{\"task\":\"t1\",\"tier\":0,\"sha\":\"x\",\"outcome\":\"tests-red\",\"lesson\":\"l1\",\"agent\":\"a\",\"at\":\"2026-07-01T00:00:00Z\"}",
            )),
        )]);
        let g = Git {
            runner: &r,
            remote: "origin".into(),
        };
        let map = g.attempts().unwrap();
        let recs = &map["t1"];
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].lesson, "l1"); // chronological
        r.assert_done();
    }

    #[test]
    fn verdict_roundtrip_shape() {
        let r = MockRunner::new(vec![(
            "git for-each-ref",
            Step::Ok("refs/loom/mirror/verdict/t1\0s\0{\"task\":\"t1\",\"sha\":\"abc\",\"verdict\":\"approve\",\"agent\":\"rev\",\"self_verdict\":false,\"at\":\"2026-07-01T00:00:00Z\"}"),
        )]);
        let g = Git {
            runner: &r,
            remote: "origin".into(),
        };
        let v = g.verdict("t1").unwrap().unwrap();
        assert_eq!(v.verdict, "approve");
        assert_eq!(v.sha, "abc");
        r.assert_done();
    }

    #[test]
    fn canonical_blobs_fall_back_when_remote_main_missing() {
        let r = MockRunner::new(vec![(
            "git fetch origin main",
            Step::Fail("couldn't find remote ref"),
        )]);
        let g = Git {
            runner: &r,
            remote: "origin".into(),
        };
        assert!(g.canonical_task_blobs("main").unwrap().is_none());
        r.assert_done();
    }
}
