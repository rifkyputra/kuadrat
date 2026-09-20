//! The deploy state machine. `run` sequences Detect → Build → Secrets → Apply →
//! Route → Healthcheck, persisting the stage and emitting an event after each,
//! under the per-app lock. A stage failure compensates backward, returning
//! `RolledBack` when the undo succeeds and `Failed` when it also fails.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};

use crate::deploy::build::build;
use crate::deploy::detect::detect;
use crate::deploy::health::healthcheck;
use crate::deploy::{Ctx, DeployOutcome, DeployStatus, Stage};
use crate::events::{Event, EventStatus};
use crate::gateway::{apply_route, remove_route};
use crate::secrets::ensure_all;
use crate::spec::{slug, WorkloadSpec};
use crate::workloads::apply::{apply, remove};

/// Hard cap for the whole Healthcheck stage, independent of
/// [`crate::deploy::health`]'s internal wall-clock budget. That budget is
/// bounded on paper (60s total, 5s per attempt, `kill_on_drop` on the child),
/// but it assumes the runtime can yield to timers — a child that cancellation
/// does not detach from (the open possibility from the H7 acceptance hang in
/// docs/known-gaps.md) stalls the future without ever resolving it. This cap
/// sits outside that machinery: even then the stage fails with a named error
/// and the deploy rolls back instead of hanging a CLI invocation forever.
/// 90s = the 60s poll budget plus interval slack, so a normal unhealthy
/// deploy still gets its full budget; only a never-resolving health command
/// trips this.
const HEALTHCHECK_STAGE_CAP: Duration = Duration::from_secs(90);

/// Deploy `spec` from the repo at `repo`. Returns the terminal outcome
/// (`Done`/`RolledBack`/`Failed`); returns `Err` only when the deploy could not
/// begin (invalid spec, or another deploy already holds the lock).
pub async fn run(ctx: &Ctx<'_>, spec: WorkloadSpec, repo: &Path) -> Result<DeployOutcome> {
    spec.validate()?;
    let deploy_id = reserve(ctx, &spec.name).map_err(anyhow::Error::new)?;
    run_reserved(ctx, spec, repo, deploy_id).await
}

/// Why a deploy could not be reserved. Busy is a caller-visible conflict and
/// carries the already-finished attempt row; storage failures are internal and
/// must never be mislabeled as ordinary contention.
#[derive(Debug, thiserror::Error)]
pub enum ReserveError {
    #[error("another deploy of {name} is already in progress")]
    Busy { name: String, deploy_id: i64 },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Allocate a deploy row for `name` and take its lock, without running anything.
///
/// Split out of [`run`] for a caller that must answer "accepted, id N" before
/// the deploy starts — the daemon, which queues deploys behind a global
/// one-at-a-time semaphore and needs an addressable id for the page it
/// redirects to. The row is `in_progress` from this moment, so a crash while
/// queued leaves exactly what a crash mid-deploy leaves, and `reconcile` rolls
/// it back on the next start.
///
/// Errors when another deploy of the same app already holds the lock.
pub fn reserve(ctx: &Ctx<'_>, name: &str) -> std::result::Result<i64, ReserveError> {
    let deploy_id = ctx.store.create_deploy(name)?;
    if !ctx.store.acquire_lock(name, deploy_id)? {
        ctx.store.finish_deploy(
            deploy_id,
            DeployStatus::Failed,
            Some("another deploy is already in progress"),
        )?;
        return Err(ReserveError::Busy {
            name: name.to_string(),
            deploy_id,
        });
    }
    Ok(deploy_id)
}

/// Run the stages for a deploy already reserved by [`reserve`], releasing the
/// lock however it ends. The caller owns the id; this owns everything after it.
pub async fn run_reserved(
    ctx: &Ctx<'_>,
    spec: WorkloadSpec,
    repo: &Path,
    deploy_id: i64,
) -> Result<DeployOutcome> {
    spec.validate()?;
    let name = spec.name.clone();
    let slug = spec.slug();
    let previous = load_previous(ctx, &name)?;

    // Everything past the lock must release it, whatever happens. On the
    // common path this is a no-op: `run_stages` ends by calling `finished`,
    // which now releases this deploy's lock as part of the same transaction
    // that writes its terminal status — so a subscriber can never observe
    // "finished" while the lock is still held. This call stays as the
    // backstop for the paths where `run_stages` returns `Err` without ever
    // reaching a terminal write (a spec that fails to serialize, `put_spec`
    // erroring) — no transaction ran, so the lock is still held and only
    // this releases it. `release_lock` is idempotent, so keeping both is
    // free on the path that already released.
    let result = run_stages(ctx, spec, repo, &slug, deploy_id, &previous).await;
    ctx.store.release_lock(&name)?;
    result
}

/// Recover from crashed deploys. For every deploy still `in_progress` (a crash
/// left it un-finished with its lock held), roll it back to the last-good state
/// using the same compensation the driver uses, then release its lock. Returns
/// one outcome per reconciled deploy. Safe to call on every startup — a no-op
/// when nothing is in progress.
pub async fn reconcile(ctx: &Ctx<'_>) -> Result<Vec<DeployOutcome>> {
    let mut outcomes = Vec::new();

    for row in ctx.store.in_progress_deploys()? {
        let previous = load_previous(ctx, &row.app)?;
        let app_slug = slug(&row.app);

        let outcome = match compensate(ctx, &row.app, &app_slug, &previous, row.stage).await {
            Ok(()) => {
                let cause = format!(
                    "reconciled after restart (was in progress at {:?})",
                    row.stage
                );
                finished(ctx, row.id, DeployStatus::RolledBack, Some(cause.clone()))?;
                DeployOutcome::RolledBack {
                    failed_at: row.stage,
                    cause,
                }
            }
            Err(e) => {
                let cause = format!("reconcile compensation failed: {e:#}");
                finished(ctx, row.id, DeployStatus::Failed, Some(cause.clone()))?;
                DeployOutcome::Failed {
                    failed_at: row.stage,
                    cause,
                }
            }
        };

        // The deploy is terminally finished either way — release its lock.
        ctx.store.release_lock(&row.app)?;
        outcomes.push(outcome);
    }

    // Run after the in-progress rollbacks above, not before: a deploy this
    // very call just finished (and whose lock the loop above already
    // released) is harmless to re-check here, and a lock orphaned by an
    // older binary that predates `finish_deploy_with_event`'s own release —
    // or by any other path that reached a terminal status without going
    // through it — has no other recovery. `reconcile` runs on every daemon
    // start and via `kuadrat reconcile`, so this is what actually frees a
    // lock like the one found stuck on `h7servedemo`.
    let freed = ctx.store.release_orphaned_locks()?;
    for app in &freed {
        eprintln!("released orphaned lock: {app}");
    }

    Ok(outcomes)
}

fn load_previous(ctx: &Ctx<'_>, name: &str) -> Result<Option<WorkloadSpec>> {
    match ctx.store.current_spec(name)? {
        Some(json) => Ok(Some(
            serde_json::from_str(&json).context("parsing the previously stored spec")?,
        )),
        None => Ok(None),
    }
}

async fn run_stages(
    ctx: &Ctx<'_>,
    mut spec: WorkloadSpec,
    repo: &Path,
    slug: &str,
    deploy_id: i64,
    previous: &Option<WorkloadSpec>,
) -> Result<DeployOutcome> {
    begin(ctx, deploy_id, Stage::Detect)?;
    let plan = match detect(ctx.exec, ctx.fsys, repo).await {
        Ok(plan) => {
            ok(ctx, deploy_id, Stage::Detect)?;
            plan
        }
        Err(e) => return fail(ctx, deploy_id, &spec, slug, previous, Stage::Detect, e).await,
    };

    begin(ctx, deploy_id, Stage::Build)?;
    let image = match build(ctx.exec, &plan, slug).await {
        Ok(image) => {
            ok(ctx, deploy_id, Stage::Build)?;
            image
        }
        Err(e) => return fail(ctx, deploy_id, &spec, slug, previous, Stage::Build, e).await,
    };
    spec.image = image.clone();

    begin(ctx, deploy_id, Stage::Secrets)?;
    if let Err(e) = ensure_all(ctx.exec, &spec.secrets).await {
        return fail(ctx, deploy_id, &spec, slug, previous, Stage::Secrets, e).await;
    }
    ok(ctx, deploy_id, Stage::Secrets)?;

    begin(ctx, deploy_id, Stage::Apply)?;
    if let Err(e) = apply(ctx.exec, ctx.fsys, ctx.paths, &spec).await {
        return fail(ctx, deploy_id, &spec, slug, previous, Stage::Apply, e).await;
    }
    ok(ctx, deploy_id, Stage::Apply)?;

    begin(ctx, deploy_id, Stage::Route)?;
    if let Some(route) = &spec.route {
        if let Err(e) = apply_route(ctx.exec, ctx.fsys, ctx.paths, slug, route).await {
            return fail(ctx, deploy_id, &spec, slug, previous, Stage::Route, e).await;
        }
    }
    ok(ctx, deploy_id, Stage::Route)?;

    begin(ctx, deploy_id, Stage::Healthcheck)?;
    let health = tokio::time::timeout(HEALTHCHECK_STAGE_CAP, healthcheck(ctx.exec, &spec))
        .await
        .map_err(|_| {
            anyhow!(
                "healthcheck stage exceeded its {}s hard cap",
                HEALTHCHECK_STAGE_CAP.as_secs()
            )
        })
        // timeouts wrap the stage's own `Result`; flatten both layers so a
        // failing healthcheck still rolls back instead of passing the stage.
        .and_then(|res| res);
    if let Err(e) = health {
        return fail(ctx, deploy_id, &spec, slug, previous, Stage::Healthcheck, e).await;
    }
    ok(ctx, deploy_id, Stage::Healthcheck)?;

    let json = serde_json::to_string(&spec).context("serializing the deployed spec")?;
    ctx.store.put_spec(&spec.name, slug, &json)?;
    finished(ctx, deploy_id, DeployStatus::Done, None)?;
    Ok(DeployOutcome::Done { image })
}

/// Advance the durable stage and emit a Started event.
fn begin(ctx: &Ctx<'_>, deploy_id: i64, stage: Stage) -> Result<()> {
    ctx.store.advance_stage(deploy_id, stage)?;
    emit(ctx, deploy_id, stage, EventStatus::Started, None)
}

/// Emit a Succeeded event for a stage.
fn ok(ctx: &Ctx<'_>, deploy_id: i64, stage: Stage) -> Result<()> {
    emit(ctx, deploy_id, stage, EventStatus::Succeeded, None)
}

/// Persist one event, then publish it to the sink.
///
/// The order is load-bearing: the store is what a reconnecting subscriber
/// reads for the backlog, so an event must be durable before anyone can see
/// it. Publishing first would let a browser render a stage that a crash then
/// erases.
fn emit(
    ctx: &Ctx<'_>,
    deploy_id: i64,
    stage: Stage,
    status: EventStatus,
    detail: Option<String>,
) -> Result<()> {
    let event = Event::for_stage(deploy_id, stage, status, detail);
    let stored = ctx.store.append_event(&event)?;
    ctx.sink.emit(&stored);
    Ok(())
}

/// Persist the deploy-level terminal status and its event as one atomic
/// write, then publish it.
///
/// Uses `Store::finish_deploy_with_event` rather than a separate
/// `finish_deploy` followed by `append_event`: the SSE handler reads the
/// `deploys` row and the event backlog as two separate queries, so two
/// separate writes leave a window where a reader can see the row already
/// terminal but the backlog not yet carrying the terminal event — which the
/// handler would misread as "this deploy ends by a path that emits no
/// event" (`reserve`'s rejection is the only real case of that). The single
/// transaction removes the window: a reader always sees the row and the
/// event agree.
fn finished(
    ctx: &Ctx<'_>,
    deploy_id: i64,
    status: DeployStatus,
    detail: Option<String>,
) -> Result<()> {
    let stored = ctx
        .store
        .finish_deploy_with_event(deploy_id, status, detail.as_deref())?;
    ctx.sink.emit(&stored);
    Ok(())
}

/// Handle a stage failure: compensate backward. `RolledBack` when the undo
/// succeeds; `Failed` when compensation also fails (host state is unknown).
async fn fail(
    ctx: &Ctx<'_>,
    deploy_id: i64,
    spec: &WorkloadSpec,
    slug: &str,
    previous: &Option<WorkloadSpec>,
    stage: Stage,
    err: anyhow::Error,
) -> Result<DeployOutcome> {
    let cause = format!("{err:#}");
    emit(
        ctx,
        deploy_id,
        stage,
        EventStatus::Failed,
        Some(cause.clone()),
    )?;

    match compensate(ctx, &spec.name, slug, previous, stage).await {
        Ok(()) => {
            finished(
                ctx,
                deploy_id,
                DeployStatus::RolledBack,
                Some(cause.clone()),
            )?;
            Ok(DeployOutcome::RolledBack {
                failed_at: stage,
                cause,
            })
        }
        Err(comp) => {
            let combined = format!("{cause}; compensation also failed: {comp:#}");
            finished(ctx, deploy_id, DeployStatus::Failed, Some(combined.clone()))?;
            Ok(DeployOutcome::Failed {
                failed_at: stage,
                cause: combined,
            })
        }
    }
}

/// Undo the host changes made before `failed_at`, walking backward.
async fn compensate(
    ctx: &Ctx<'_>,
    name: &str,
    slug: &str,
    previous: &Option<WorkloadSpec>,
    failed_at: Stage,
) -> Result<()> {
    match failed_at {
        // The host was never touched — the old version is still serving.
        Stage::Detect | Stage::Build | Stage::Secrets => Ok(()),
        Stage::Apply => unwind_apply(ctx, name, previous).await,
        Stage::Route | Stage::Healthcheck => {
            unwind_route(ctx, slug, previous).await?;
            unwind_apply(ctx, name, previous).await
        }
    }
}

/// Restore the previous unit (re-apply the previous spec), or remove the unit
/// this deploy wrote when there was no previous.
async fn unwind_apply(ctx: &Ctx<'_>, name: &str, previous: &Option<WorkloadSpec>) -> Result<()> {
    match previous {
        Some(prev) => apply(ctx.exec, ctx.fsys, ctx.paths, prev).await,
        None => remove(ctx.exec, ctx.fsys, ctx.paths, name).await,
    }
}

/// Restore the previous route, or remove the route this deploy wrote when there
/// was no previous route.
async fn unwind_route(ctx: &Ctx<'_>, slug: &str, previous: &Option<WorkloadSpec>) -> Result<()> {
    match previous.as_ref().and_then(|p| p.route.as_ref()) {
        Some(prev_route) => apply_route(ctx.exec, ctx.fsys, ctx.paths, slug, prev_route).await,
        None => remove_route(ctx.exec, ctx.fsys, ctx.paths, slug).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::fake::FakeSink;
    use crate::events::null::NullSink;
    use crate::events::{EventKind, EventSink, StoredEvent};
    use crate::exec::fake::FakeExecutor;
    use crate::exec::CommandOutput;
    use crate::exec::Executor;
    use crate::fs::fake::FakeFileSystem;
    use crate::spec::WorkloadSpec;
    use crate::store::Store;
    use crate::workloads::paths::Paths;
    use std::path::Path;
    use tempfile::tempdir;

    fn out(status: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            status,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    /// Script a `FakeExecutor` for a clean deploy of an app with no secrets,
    /// no route, and no health_cmd. `start_result` lets a test fail the Apply.
    fn script_clean(exec: &FakeExecutor, sha: &str, slug: &str, start_result: CommandOutput) {
        exec.expect_call(
            "git",
            &["-C", "/repo", "rev-parse", "HEAD"],
            out(0, &format!("{sha}\n"), ""),
        );
        exec.expect_call(
            "podman",
            &[
                "build",
                "-t",
                &format!("localhost/kuadrat-{slug}:{sha}"),
                "-f",
                "/repo/Containerfile",
                "/repo",
            ],
            out(0, "", ""),
        );
        exec.expect_call(
            "podman",
            &["secret", "ls", "--format", "{{.Name}}"],
            out(0, "", ""),
        );
        exec.expect_call("systemctl", &["daemon-reload"], out(0, "", ""));
        exec.expect_call(
            "systemctl",
            &["start", &format!("kuadrat-{slug}")],
            start_result,
        );
        exec.expect_call(
            "systemctl",
            &["is-active", &format!("kuadrat-{slug}")],
            out(0, "active\n", ""),
        );
    }

    fn fsys_with_repo() -> FakeFileSystem {
        let fsys = FakeFileSystem::new();
        fsys.insert("/repo/Containerfile", "FROM alpine\n");
        fsys
    }

    /// Owns the fakes so a `Ctx` can borrow them, and keeps the `TempDir`
    /// alive — dropping it would delete the database mid-test.
    struct Harness {
        exec: FakeExecutor,
        fsys: FakeFileSystem,
        store: Store,
        paths: Paths,
        _dir: tempfile::TempDir,
    }

    impl Harness {
        /// `start_result` is what `systemctl start` returns, which is the knob
        /// that decides whether Apply succeeds.
        fn new(start_result: CommandOutput) -> Self {
            let dir = tempdir().unwrap();
            let store = Store::open(&dir.path().join("k.db")).unwrap();
            let paths = Paths::rooted(dir.path());
            let fsys = fsys_with_repo();
            let exec = FakeExecutor::new();
            script_clean(&exec, "abc123", "web", start_result);
            Self {
                exec,
                fsys,
                store,
                paths,
                _dir: dir,
            }
        }

        fn ctx<'a>(&'a self, sink: &'a dyn EventSink) -> Ctx<'a> {
            Ctx {
                exec: &self.exec,
                fsys: &self.fsys,
                store: &self.store,
                paths: &self.paths,
                sink,
            }
        }
    }

    #[test]
    fn a_busy_reservation_reports_the_attempt_it_finished() {
        let h = harness_ok();
        let first = h.store.create_deploy("web").expect("first row");
        assert!(h.store.acquire_lock("web", first).expect("first lock"));
        let sink = NullSink;

        let err = reserve(&h.ctx(&sink), "web").expect_err("busy");
        let attempt_id = match err {
            ReserveError::Busy { name, deploy_id } => {
                assert_eq!(name, "web");
                deploy_id
            }
            ReserveError::Internal(err) => panic!("unexpected internal error: {err:#}"),
        };
        let attempt = h
            .store
            .deploy(attempt_id)
            .expect("read attempt")
            .expect("attempt row");
        assert_eq!(attempt.status, DeployStatus::Failed);
        assert!(attempt.detail.as_deref().unwrap_or("").contains("progress"));
    }

    /// Every stage succeeds; the deploy reaches Done.
    fn harness_ok() -> Harness {
        Harness::new(out(0, "", ""))
    }

    /// `systemctl start` fails, so Apply fails and compensation removes the
    /// unit — the same sequence
    /// `a_first_deploy_failing_at_apply_rolls_back_by_removing_the_unit`
    /// already proves. The second `daemon-reload` that `remove` runs is
    /// already scripted by `script_clean`.
    fn harness_apply_fails() -> Harness {
        let h = Harness::new(out(1, "", "boom"));
        h.exec
            .expect_call("systemctl", &["stop", "kuadrat-web"], out(0, "", ""));
        h
    }

    #[tokio::test]
    async fn a_clean_deploy_runs_every_stage_and_returns_done() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("k.db")).unwrap();
        let paths = Paths::rooted(dir.path());
        let fsys = fsys_with_repo();
        let exec = FakeExecutor::new();
        script_clean(&exec, "abc123", "web", out(0, "", ""));

        let ctx = Ctx {
            exec: &exec,
            fsys: &fsys,
            store: &store,
            paths: &paths,
            sink: &NullSink,
        };
        let outcome = run(
            &ctx,
            WorkloadSpec::new("web", "placeholder"),
            Path::new("/repo"),
        )
        .await
        .expect("deploy");

        assert_eq!(
            outcome,
            DeployOutcome::Done {
                image: "localhost/kuadrat-web:abc123".into()
            }
        );
        // The lock was released: a fresh acquire succeeds.
        assert!(store.acquire_lock("web", 999).unwrap());
        // The spec was stored.
        assert!(store.current_spec("web").unwrap().is_some());
    }

    #[tokio::test]
    async fn a_first_deploy_failing_at_apply_rolls_back_by_removing_the_unit() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("k.db")).unwrap();
        let paths = Paths::rooted(dir.path());
        let fsys = fsys_with_repo();
        let exec = FakeExecutor::new();
        // Apply writes the unit, daemon-reload ok, start FAILS.
        script_clean(&exec, "abc123", "web", out(1, "", "boom"));
        // Compensation: no previous spec → remove the unit we just wrote.
        exec.expect_call("systemctl", &["stop", "kuadrat-web"], out(0, "", ""));
        // (remove also runs a second daemon-reload — already scripted by script_clean)

        let ctx = Ctx {
            exec: &exec,
            fsys: &fsys,
            store: &store,
            paths: &paths,
            sink: &NullSink,
        };
        let outcome = run(
            &ctx,
            WorkloadSpec::new("web", "placeholder"),
            Path::new("/repo"),
        )
        .await
        .expect("terminal outcome");

        match outcome {
            DeployOutcome::RolledBack { failed_at, .. } => assert_eq!(failed_at, Stage::Apply),
            other => panic!("expected RolledBack at Apply, got {other:?}"),
        }
        // The half-written unit was removed by compensation.
        let unit = paths.quadlet_dir.join("kuadrat-web.container");
        assert!(
            fsys.contents(&unit).is_none(),
            "the unit should have been removed"
        );
        assert!(store.acquire_lock("web", 999).unwrap(), "lock released");
    }

    #[tokio::test]
    async fn a_failure_at_healthcheck_unwinds_route_then_apply() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("k.db")).unwrap();
        let paths = Paths::rooted(dir.path());
        let fsys = fsys_with_repo();
        let exec = FakeExecutor::new();

        // Full forward path succeeds until healthcheck. No route on the spec, so
        // Route runs no command; is-active reports NOT active → Healthcheck fails.
        exec.expect_call(
            "git",
            &["-C", "/repo", "rev-parse", "HEAD"],
            out(0, "abc123\n", ""),
        );
        exec.expect_call(
            "podman",
            &[
                "build",
                "-t",
                "localhost/kuadrat-web:abc123",
                "-f",
                "/repo/Containerfile",
                "/repo",
            ],
            out(0, "", ""),
        );
        exec.expect_call(
            "podman",
            &["secret", "ls", "--format", "{{.Name}}"],
            out(0, "", ""),
        );
        exec.expect_call("systemctl", &["daemon-reload"], out(0, "", ""));
        exec.expect_call("systemctl", &["start", "kuadrat-web"], out(0, "", ""));
        exec.expect_call(
            "systemctl",
            &["is-active", "kuadrat-web"],
            out(3, "failed\n", ""),
        );
        // Compensation for a Healthcheck failure: unwind_route then unwind_apply.
        // The spec has no route, so no fragment was written — `remove_route` sees
        // an absent (unowned) fragment and returns early WITHOUT reloading Caddy,
        // so there is NO `reload caddy` call to script. unwind_apply has no
        // previous spec, so it removes the unit: `stop` then `daemon-reload`
        // (daemon-reload is already scripted above).
        exec.expect_call("systemctl", &["stop", "kuadrat-web"], out(0, "", ""));

        let ctx = Ctx {
            exec: &exec,
            fsys: &fsys,
            store: &store,
            paths: &paths,
            sink: &NullSink,
        };
        let outcome = run(
            &ctx,
            WorkloadSpec::new("web", "placeholder"),
            Path::new("/repo"),
        )
        .await
        .expect("terminal outcome");

        match outcome {
            DeployOutcome::RolledBack { failed_at, .. } => {
                assert_eq!(failed_at, Stage::Healthcheck)
            }
            other => panic!("expected RolledBack at Healthcheck, got {other:?}"),
        }
    }

    /// A health check that never resolves must not stall the deploy machine:
    /// the stage-level cap (`HEALTHCHECK_STAGE_CAP`) fails the stage and rolls
    /// back even when `poll_health`'s own budget can never fire — a child that
    /// cancellation does not detach from is exactly the case neither the
    /// per-attempt timeout nor `kill_on_drop` can end.
    #[tokio::test(start_paused = true)]
    async fn a_healthcheck_that_never_resolves_is_cut_by_the_stage_cap_and_rolls_back() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("k.db")).unwrap();
        let paths = Paths::rooted(dir.path());
        let fsys = fsys_with_repo();

        // Forward path succeeds until the healthcheck. The podman healthcheck
        // call is intercepted by the executor below and never resolves.
        let fake = FakeExecutor::new();
        script_clean(&fake, "abc123", "web", out(0, "", ""));
        // Compensation: no previous spec → remove the unit we wrote (stop +
        // the daemon-reload already scripted above).
        fake.expect_call("systemctl", &["stop", "kuadrat-web"], out(0, "", ""));

        // A `podman healthcheck` that never resolves; everything else
        // delegates to the fake above. An unscripted call would panic in the
        // fake, so the wrong path fails the test on its own.
        struct HangingHealthcheck {
            inner: FakeExecutor,
        }

        #[async_trait::async_trait]
        impl Executor for HangingHealthcheck {
            async fn run(
                &self,
                program: &str,
                args: &[String],
            ) -> anyhow::Result<crate::exec::CommandOutput> {
                if program == "podman" && args.first().map(String::as_str) == Some("healthcheck") {
                    // Never resolves: cut by the stage cap, not by finishing.
                    std::future::pending().await
                }
                self.inner.run(program, args).await
            }
        }

        let mut spec = WorkloadSpec::new("web", "placeholder");
        spec.health_cmd = Some("true".into());
        let exec = HangingHealthcheck { inner: fake };
        let ctx = Ctx {
            exec: &exec,
            fsys: &fsys,
            store: &store,
            paths: &paths,
            sink: &NullSink,
        };
        let outcome = run(&ctx, spec, Path::new("/repo"))
            .await
            .expect("terminal outcome");

        match outcome {
            DeployOutcome::RolledBack { failed_at, .. } => {
                assert_eq!(failed_at, Stage::Healthcheck)
            }
            other => panic!("expected RolledBack at Healthcheck, got {other:?}"),
        }
        assert!(store.acquire_lock("web", 999).unwrap(), "lock released");
    }

    #[tokio::test]
    async fn a_failed_rollback_reports_failed_not_rolled_back() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("k.db")).unwrap();
        let paths = Paths::rooted(dir.path());
        let fsys = fsys_with_repo();
        let exec = FakeExecutor::new();
        // Apply's start fails...
        script_clean(&exec, "abc123", "web", out(1, "", "boom"));
        // ...and compensation's remove (stop) ALSO fails.
        exec.expect_call(
            "systemctl",
            &["stop", "kuadrat-web"],
            out(1, "", "stop refused"),
        );

        let ctx = Ctx {
            exec: &exec,
            fsys: &fsys,
            store: &store,
            paths: &paths,
            sink: &NullSink,
        };
        let outcome = run(
            &ctx,
            WorkloadSpec::new("web", "placeholder"),
            Path::new("/repo"),
        )
        .await
        .expect("terminal outcome");

        match outcome {
            DeployOutcome::Failed { failed_at, cause } => {
                assert_eq!(failed_at, Stage::Apply);
                assert!(cause.contains("compensation also failed"), "cause: {cause}");
            }
            other => {
                panic!("expected Failed (not RolledBack) when compensation fails, got {other:?}")
            }
        }
        // Even here the lock is released.
        assert!(store.acquire_lock("web", 999).unwrap());
    }

    #[tokio::test]
    async fn reconcile_is_a_noop_when_nothing_is_in_progress() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("k.db")).unwrap();
        let paths = Paths::rooted(dir.path());
        let fsys = FakeFileSystem::new();
        let exec = FakeExecutor::new();
        let ctx = Ctx {
            exec: &exec,
            fsys: &fsys,
            store: &store,
            paths: &paths,
            sink: &NullSink,
        };
        assert!(reconcile(&ctx).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reconcile_rolls_back_a_crash_at_detect_with_no_host_changes() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("k.db")).unwrap();
        let paths = Paths::rooted(dir.path());
        let fsys = FakeFileSystem::new();
        // A crash left an in_progress deploy stuck at Detect, lock held.
        let id = store.create_deploy("web").unwrap();
        store.advance_stage(id, Stage::Detect).unwrap();
        store.acquire_lock("web", id).unwrap();

        let exec = FakeExecutor::new(); // Detect touched nothing, so no host calls
        let ctx = Ctx {
            exec: &exec,
            fsys: &fsys,
            store: &store,
            paths: &paths,
            sink: &NullSink,
        };
        let outcomes = reconcile(&ctx).await.unwrap();

        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            outcomes[0],
            DeployOutcome::RolledBack {
                failed_at: Stage::Detect,
                ..
            }
        ));
        assert!(
            store.in_progress_deploys().unwrap().is_empty(),
            "row finished"
        );
        assert!(store.acquire_lock("web", 999).unwrap(), "lock released");
    }

    /// A crash-rolled-back deploy must end its timeline explicitly. Nobody is
    /// subscribed while reconcile runs — it finishes before the listener binds
    /// — but the stored event is what `/deploy/:id` renders afterwards, and
    /// what lets the stream for that id close instead of waiting forever.
    #[tokio::test]
    async fn reconcile_records_a_terminal_event_for_what_it_rolled_back() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("k.db")).unwrap();
        let paths = Paths::rooted(dir.path());
        let fsys = FakeFileSystem::new();
        // A crash left an in_progress deploy stuck at Detect, lock held.
        let id = store.create_deploy("web").unwrap();
        store.advance_stage(id, Stage::Detect).unwrap();
        store.acquire_lock("web", id).unwrap();

        let exec = FakeExecutor::new(); // Detect touched nothing, so no host calls
        let sink = FakeSink::new();
        let ctx = Ctx {
            exec: &exec,
            fsys: &fsys,
            store: &store,
            paths: &paths,
            sink: &sink,
        };
        reconcile(&ctx).await.unwrap();

        let stored = store.events_for(id).expect("events");
        let last = stored.last().expect("a terminal event");
        assert_eq!(
            last.event.kind,
            EventKind::Finished {
                status: DeployStatus::RolledBack
            }
        );
        assert!(
            last.event.detail.is_some(),
            "the cause reconcile wrote to the row belongs on the event too"
        );
    }

    #[tokio::test]
    async fn reconcile_restores_the_previous_spec_after_a_crash_at_apply() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("k.db")).unwrap();
        let paths = Paths::rooted(dir.path());
        let fsys = FakeFileSystem::new();
        // A prior successful deploy stored a spec.
        let prev = WorkloadSpec::new("web", "old:1");
        store
            .put_spec("web", "web", &serde_json::to_string(&prev).unwrap())
            .unwrap();
        // The next deploy crashed at Apply.
        let id = store.create_deploy("web").unwrap();
        store.advance_stage(id, Stage::Apply).unwrap();
        store.acquire_lock("web", id).unwrap();

        // Reconcile re-applies the previous spec: daemon-reload + start.
        let exec = FakeExecutor::new();
        exec.expect_call("systemctl", &["daemon-reload"], out(0, "", ""));
        exec.expect_call("systemctl", &["start", "kuadrat-web"], out(0, "", ""));

        let ctx = Ctx {
            exec: &exec,
            fsys: &fsys,
            store: &store,
            paths: &paths,
            sink: &NullSink,
        };
        let outcomes = reconcile(&ctx).await.unwrap();

        assert!(matches!(
            outcomes[0],
            DeployOutcome::RolledBack {
                failed_at: Stage::Apply,
                ..
            }
        ));
        assert!(store.in_progress_deploys().unwrap().is_empty());
        assert!(store.acquire_lock("web", 999).unwrap());
    }

    #[tokio::test]
    async fn reconcile_removes_a_partial_unit_from_a_crashed_first_deploy() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("k.db")).unwrap();
        let paths = Paths::rooted(dir.path());
        let fsys = FakeFileSystem::new();
        // The crashed first deploy wrote a marker-owned unit before dying at Apply.
        let unit = paths.quadlet_dir.join("kuadrat-web.container");
        fsys.insert(&unit, "# kuadrat-managed: true\n[Container]\nImage=x\n");
        let id = store.create_deploy("web").unwrap();
        store.advance_stage(id, Stage::Apply).unwrap();
        store.acquire_lock("web", id).unwrap();
        // No previous spec → reconcile removes the unit: stop + daemon-reload.
        let exec = FakeExecutor::new();
        exec.expect_call("systemctl", &["stop", "kuadrat-web"], out(0, "", ""));
        exec.expect_call("systemctl", &["daemon-reload"], out(0, "", ""));

        let ctx = Ctx {
            exec: &exec,
            fsys: &fsys,
            store: &store,
            paths: &paths,
            sink: &NullSink,
        };
        let outcomes = reconcile(&ctx).await.unwrap();

        assert!(matches!(outcomes[0], DeployOutcome::RolledBack { .. }));
        assert!(fsys.contents(&unit).is_none(), "partial unit removed");
        assert!(store.acquire_lock("web", 999).unwrap());
    }

    #[tokio::test]
    async fn a_concurrent_deploy_is_rejected_while_the_lock_is_held() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("k.db")).unwrap();
        let paths = Paths::rooted(dir.path());
        let fsys = fsys_with_repo();
        let exec = FakeExecutor::new(); // no stage runs, so nothing to script

        // Another deploy already holds the lock.
        let other = store.create_deploy("web").unwrap();
        store.acquire_lock("web", other).unwrap();

        let ctx = Ctx {
            exec: &exec,
            fsys: &fsys,
            store: &store,
            paths: &paths,
            sink: &NullSink,
        };
        let err = run(
            &ctx,
            WorkloadSpec::new("web", "placeholder"),
            Path::new("/repo"),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("already in progress"),
            "message: {err}"
        );
    }

    /// A deploy that reaches Done must have emitted Started and Succeeded for
    /// all six stages, in order. This is what the browser renders.
    #[tokio::test]
    async fn a_successful_deploy_emits_every_stage_in_order() {
        let h = harness_ok();
        let sink = FakeSink::new();
        let ctx = h.ctx(&sink);

        let outcome = run(
            &ctx,
            WorkloadSpec::new("web", "placeholder"),
            Path::new("/repo"),
        )
        .await
        .expect("run");
        assert!(matches!(outcome, DeployOutcome::Done { .. }), "{outcome:?}");

        use EventStatus::{Started, Succeeded};
        assert_eq!(
            sink.timeline(),
            vec![
                (Stage::Detect, Started),
                (Stage::Detect, Succeeded),
                (Stage::Build, Started),
                (Stage::Build, Succeeded),
                (Stage::Secrets, Started),
                (Stage::Secrets, Succeeded),
                (Stage::Apply, Started),
                (Stage::Apply, Succeeded),
                (Stage::Route, Started),
                (Stage::Route, Succeeded),
                (Stage::Healthcheck, Started),
                (Stage::Healthcheck, Succeeded),
            ]
        );
    }

    /// Every emitted event must carry the id the store assigned it, and the
    /// ids must ascend. A subscriber filters on `id > last_seen`; a zero or a
    /// repeat would silently drop events.
    #[tokio::test]
    async fn emitted_events_carry_ascending_store_ids() {
        let h = harness_ok();
        let sink = FakeSink::new();
        let ctx = h.ctx(&sink);

        run(
            &ctx,
            WorkloadSpec::new("web", "placeholder"),
            Path::new("/repo"),
        )
        .await
        .expect("run");

        let ids: Vec<i64> = sink.events().iter().map(|e| e.id).collect();
        assert!(!ids.is_empty(), "no events emitted");
        assert!(
            ids.iter().all(|&i| i > 0),
            "ids must be real rowids: {ids:?}"
        );
        assert!(
            ids.windows(2).all(|w| w[1] > w[0]),
            "ids must ascend: {ids:?}"
        );
    }

    /// The last stage event before a rollback is the Failed event for the
    /// stage that broke — that is what the UI highlights and what the
    /// webhook forwards. (The terminal `Finished` event follows it; see
    /// `a_rolled_back_deploy_says_so_after_the_failed_stage_event`.)
    #[tokio::test]
    async fn a_failed_stage_emits_a_failed_event_naming_that_stage() {
        let h = harness_apply_fails();
        let sink = FakeSink::new();
        let ctx = h.ctx(&sink);

        let outcome = run(
            &ctx,
            WorkloadSpec::new("web", "placeholder"),
            Path::new("/repo"),
        )
        .await
        .expect("terminal outcome");
        assert!(
            matches!(outcome, DeployOutcome::RolledBack { .. }),
            "{outcome:?}"
        );

        let events = sink.events();
        let last = events
            .get(events.len() - 2)
            .cloned()
            .expect("at least two events");
        assert_eq!(
            last.event.kind,
            EventKind::Stage {
                stage: Stage::Apply,
                status: EventStatus::Failed
            }
        );
        assert!(
            last.event.detail.is_some(),
            "a Failed event must carry its cause"
        );
    }

    /// Persist-before-publish: everything the sink saw is also in the store,
    /// with the same ids. A reconnecting browser reads the store for the
    /// backlog, so a gap here is an event the user can never recover.
    #[tokio::test]
    async fn every_emitted_event_is_also_durable_with_the_same_id() {
        let h = harness_ok();
        let sink = FakeSink::new();
        let ctx = h.ctx(&sink);

        run(
            &ctx,
            WorkloadSpec::new("web", "placeholder"),
            Path::new("/repo"),
        )
        .await
        .expect("run");

        let emitted: Vec<i64> = sink.events().iter().map(|e| e.id).collect();
        let deploy_id = emitted_deploy_id(&sink);
        let stored: Vec<i64> = ctx
            .store
            .events_for(deploy_id)
            .expect("read")
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(emitted, stored);
    }

    fn emitted_deploy_id(sink: &FakeSink) -> i64 {
        sink.events()
            .first()
            .expect("at least one event")
            .event
            .deploy_id
    }

    /// A sink that asserts, at the moment of `emit`, that the store already
    /// has the event it was just handed. `every_emitted_event_is_also_durable_with_the_same_id`
    /// checks the same property after the fact, which would still pass under
    /// a publish-then-persist ordering — this checks it inline, so it fails
    /// immediately if the two calls in `emit` (append, then sink.emit) were
    /// ever swapped.
    struct AssertingSink<'a> {
        store: &'a Store,
    }

    impl EventSink for AssertingSink<'_> {
        fn emit(&self, event: &StoredEvent) {
            let durable = self
                .store
                .events_for(event.event.deploy_id)
                .expect("read back events");
            assert!(
                durable.iter().any(|e| e.id == event.id),
                "event {} was published before it was durable",
                event.id
            );
        }
    }

    /// Pins persist-before-publish as an ordering property, not just a
    /// same-ids-in-the-end property: the assertion runs inside `emit`, before
    /// the deploy loop moves on, so it can only pass if the store already has
    /// the row.
    #[tokio::test]
    async fn every_event_is_durable_at_the_moment_it_is_emitted() {
        let h = harness_ok();
        let sink = AssertingSink { store: &h.store };
        let ctx = h.ctx(&sink);

        let outcome = run(
            &ctx,
            WorkloadSpec::new("web", "placeholder"),
            Path::new("/repo"),
        )
        .await
        .expect("run");
        assert!(matches!(outcome, DeployOutcome::Done { .. }), "{outcome:?}");
    }

    /// The gap this closes: before it, a `Done` deploy was indistinguishable
    /// from one that stalled after the last stage event.
    #[tokio::test]
    async fn a_successful_deploy_emits_a_terminal_done_event_last() {
        let h = harness_ok();
        let sink = FakeSink::new();
        let ctx = h.ctx(&sink);
        run(
            &ctx,
            WorkloadSpec::new("web", "placeholder"),
            Path::new("/repo"),
        )
        .await
        .expect("deploy");

        assert_eq!(sink.terminal(), Some(DeployStatus::Done));
        let last = sink.events().last().cloned().expect("events");
        assert_eq!(
            last.event.kind,
            EventKind::Finished {
                status: DeployStatus::Done
            },
            "the terminal event must be the last thing a watcher sees"
        );
    }

    /// The other half of the gap: a rollback that *succeeded* was invisible.
    /// A watcher saw "Apply failed" and then nothing.
    #[tokio::test]
    async fn a_rolled_back_deploy_says_so_after_the_failed_stage_event() {
        let h = harness_apply_fails();
        let sink = FakeSink::new();
        let ctx = h.ctx(&sink);
        run(
            &ctx,
            WorkloadSpec::new("web", "placeholder"),
            Path::new("/repo"),
        )
        .await
        .expect("deploy");

        let kinds: Vec<EventKind> = sink.events().iter().map(|e| e.event.kind).collect();
        let last_two = &kinds[kinds.len() - 2..];
        assert_eq!(
            last_two,
            [
                EventKind::Stage {
                    stage: Stage::Apply,
                    status: EventStatus::Failed
                },
                EventKind::Finished {
                    status: DeployStatus::RolledBack
                },
            ]
        );
    }

    /// The terminal event is durable like every other one, and carries the
    /// cause the deploys row carries.
    #[tokio::test]
    async fn the_terminal_event_is_stored_with_the_failure_cause() {
        let h = harness_apply_fails();
        let sink = FakeSink::new();
        let ctx = h.ctx(&sink);
        run(
            &ctx,
            WorkloadSpec::new("web", "placeholder"),
            Path::new("/repo"),
        )
        .await
        .expect("deploy");

        let deploy_id = emitted_deploy_id(&sink);
        let stored = h.store.events_for(deploy_id).expect("events");
        let last = stored.last().expect("at least one event");
        assert_eq!(
            last.event.kind,
            EventKind::Finished {
                status: DeployStatus::RolledBack
            }
        );
        assert!(
            last.event.detail.is_some(),
            "the terminal event must carry the cause"
        );
    }
}
