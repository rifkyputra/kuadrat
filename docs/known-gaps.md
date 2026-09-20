# Known gaps

Carried forward from the phase-1 whole-branch review and its fix wave. Each entry is real, judged
deferrable at the time, and worth re-reading before the phase it names.

## From H1 — `reconcile` emits no events

**Narrowed 2026-08-11 in H5.** `reconcile` now appends the deploy-level terminal event for each row
it rolls back or fails, so a crash-recovered deploy has an explicit ending in its timeline and its
stream closes instead of hanging. Nobody is subscribed while reconcile runs — it completes before
the listener binds — so the value is entirely in the stored record that `/deploy/:id` renders
afterwards.

Still open: reconcile emits no *stage* events, so the UI shows the ending without showing that a
crash recovery produced it. The terminal event's `detail` carries the cause, which is the part that
matters; a dedicated "recovered by reconcile" signal can wait for a group that has a UI to show it.

## From H1 — no terminal event

**Closed 2026-08-11 in H5.** `EventKind::Finished { status: DeployStatus }` is a stored event, so
the `deploys` table and the event log now agree about where a deploy's story ends, and a rollback
that succeeded is visible rather than inferred from silence. It is stored as the literal `"deploy"`
in the `events.stage` column with the `DeployStatus` in `status` — no schema change, and rows
written before H5 read back unaltered. The SSE handler closes on that event.

Of the two options recorded here, the second was taken. Polling the `deploys` row would have closed
the stream but left the successful-rollback case invisible, which was half the complaint.

One path still finishes a deploy without an event: `reserve` rejecting a duplicate writes `Failed`
and returns `Err`, so the caller gets a 409 and is never handed the id. The stream covers that row
by checking its status before deciding to wait.

A few other paths leave a deploy with no terminal event and, worse, no terminal *status* either —
the row stays `in_progress` — so an already-connected stream sits on `rx.recv()` forever and
`KeepAlive` holds the connection open indefinitely: `run_reserved` returning `Err` before
`run_stages` ever runs (e.g. a bad stored spec fails `spec.validate()`), the `deploy_slot.acquire()`
error return in `crates/daemon/src/api.rs` (only when the semaphore itself has closed, i.e. the
daemon is shutting down), and the terminal write itself failing (`finish_deploy_with_event`
returning `Err`, e.g. disk full). This is bounded in practice rather than fixed: the client that
opened the stream eventually goes away (tab closed, browser gives up), and the deploy row itself is
not stuck forever — a restart's `reconcile` sees anything still `in_progress` and settles it, which
is exactly what reconcile exists for. There is no server-side timeout that closes a stream on its
own; building one is a design decision (how long is long enough for a real slow deploy?) left for a
future group, not a mechanical fix folded into this one.

## From G5 — a per-row store error aborts the whole reconcile batch

`deploy::reconcile` `?`-propagates store errors (`load_previous`/`finish_deploy`/`release_lock`) per
row. If one `in_progress` deploy has, say, a corrupt stored spec JSON, reconcile returns `Err` and
leaves the remaining `in_progress` rows unreconciled and that row's lock still held. It self-heals on
the next startup run (reconcile is startup-idempotent), so it was deferred. A future pass could
collect per-row errors into the returned outcomes and continue the batch instead of aborting.

## From G4b — a `release_lock` DB error drops the terminal outcome

`deploy::run` propagates `release_lock(&name)?` after `run_stages`. If the SQLite `DELETE` fails
(disk full, corruption), `run` returns that error and the real `Done`/`RolledBack`/`Failed` outcome
is dropped, and the lock stays held until G5's reconciliation releases it. A local `DELETE` failing
is rare, and reconciliation recovers a stuck lock, so this was deferred. A clean fix wants a way to
surface the real outcome while still signalling the release failure — worth a look once core has any
logging/telemetry, or fold into G5 where reconciliation already owns stuck locks.

## From H2 — CLI-deployed apps have no registration (CLOSED by H7, 2026-08-11)

H2 added `app_config` (registered apps) alongside `apps` (deployed apps), but nothing back-filled
`app_config` for an app that was already deployed from the CLI before registration existed, or that
was deployed straight from argv without ever calling `register_app`.

When this was written, it was rated "nothing is broken" because `current_spec`, `apply`, and the
rest of the deploy path read `apps`, not `app_config`. That was true at the time and stopped being
true the moment H7 landed: the daemon's `POST /api/apps/:name/deploy` — which `kuadrat deploy`
tries first, before ever falling back to running locally — starts by reading `app_config` and 404s
when the row is missing. A gap graded "display consequence, not a data one" against the code that
existed at H2 became load-bearing for correctness against code a later group wrote, without
changing a line of the code the H2 review actually looked at. **The lesson for next time: a gap's
severity is a claim about the whole system as it exists *when read*, not just at the moment it was
written down — re-check it whenever a later group starts depending on the path it describes, not
only when that path itself changes.**

Closed by option 2 of the two listed here originally: `kuadrat deploy` now calls
`Store::register_app` with its own (canonicalised) repo path and `--route`, before attempting the
daemon handoff, on every run — so the first deploy of an app the daemon has never heard of no
longer 404s, and every later deploy keeps the registration converged on whatever was last asked
for. See `crates/cli/src/main.rs`, the `Command::Deploy` arm.

## Acceptance — PASSED 2026-08-10

Phase 1's done criterion is met. `scripts/acceptance.sh` ran on a real host (Ubuntu 24.04.4,
Podman 4.9.3, cgroups v2) and passed 16/16: apply wrote a correct unit, systemd reported it active,
podman showed the container, `list`/`status` agreed with reality, and remove cleaned up both.

It also regression-tested the two Critical findings against a real host, which is where they would
actually bite: **C1** — kuadrat refused both to overwrite and to delete a planted foreign
`.container` file; **C2** — a spec with `1\nUser=root` in an env value was rejected rather than
rendered; **I1** — `sh -c "echo …; sleep 3600"` rendered as one quoted argument, not four.

Re-run it after any change to rendering, paths, or the ownership guard:

```bash
cd ~/devbox/kuadrat && PATH=$HOME/.cargo/bin:$PATH cargo build --release
sudo bash scripts/acceptance.sh
```

## Before phase 2 starts

~~**`FakeExecutor` scripts output per program, not per argv.**~~ **Closed 2026-08-10.**
`FakeExecutor::expect_call(program, args, output)` matches an exact `(program, args)` pair and takes
precedence over the program-wide `expect()`, which still works — so existing tests were untouched.
`apply_fails_at_start_after_a_successful_reload` (`workloads/apply.rs`) is the previously
inexpressible case, now covered: the reload succeeds, the start fails, and the test asserts on both
the error and the call sequence. Phase 2's per-stage compensation tests can be written directly.

**`apply()` writes the unit before `daemon-reload` succeeds**, so a failed reload leaves an orphan
file. Acceptable today because units are derived artifacts and the ownership guard means the next
apply overwrites it — but the deploy state machine's per-stage compensation must handle it, and
should be built on the same ownership check rather than a second one.

## G3 — secrets

- **`secrets::set` upserts via remove-then-create, not an atomic replace.** podman 4.9.3's
  `secret create --replace NAME -` is broken for a not-yet-existing secret (it errors trying to
  delete a nonexistent old one), so `set` does a best-effort `podman secret rm` followed by
  `podman secret create`. There is a window between the two where the secret is absent — and, if
  `create` fails after the `rm`, permanently until re-set; a subsequent `ensure_all` gate catches
  the missing name so a deploy fails safe rather than serving half-state. Acceptable for a
  single-host tool: a container reads its mounted secret at start, not while `kuadrat secret set`
  is running, so the window does not race a live read. Revisit if kuadrat ever needs to rotate a
  secret under a running container without a restart.

## Before G4

- **`render` does not know the built image reference.** It emits `Image={spec.image}` straight
  from the spec's free-form `image` field; it never calls `image_reference`. So G4's Apply stage
  must set `spec.image = image_reference(slug, plan.commit)` itself, and must derive the spec's
  name/slug from the same identity the build used — otherwise the image tag and the container
  namespace can drift apart. Recorded here so G4 does not assume `render` already knows the built
  reference.

## Injection family (same root as C2)

`WorkloadSpec::validate()` rejects `\n` and `\r` in every rendered field, closing directive
injection. It does **not** escape `%`. Quadlet copies `Exec=` into the generated `ExecStart=`, where
systemd expands specifiers (`%H`, `%i`, …); the same applies to `Environment=`. A literal `%` needs
`%%`. Pre-existing, narrower than C2, and unchanged by the fix wave — but it is the residual of the
same family and should be closed when secrets handling lands.

## Smaller items

- **Slug collisions.** `"My App"`, `"my_app"`, and `"my-app"` all slug to `my-app`, so two distinct
  specs silently target the same unit. Deferred to a phase-2 registry that can reject a duplicate.
- **Validation boundary is `apply`-only.** `remove` and `status` skip `validate()`, so an
  empty-slug name reaches `unit_path` as `kuadrat-.container`. Harmless — the file never exists, so
  no `systemctl` call is made — but the asymmetry is worth removing.
- **ADR-0002's reviewer rule is stated too absolutely.** Clause 2 says `std::fs` and
  `Path::exists()` do not appear in the crate; both legitimately appear in `#[cfg(test)]` blocks.
  A literal grep-enforcement fires false positives. One sentence to fix.
- **`FakeFileSystem::read_dir` returns files only, never subdirectories.** Irrelevant to the
  `.container` scan today; would mask a future bug about directory entries.
- **`Paths` is reachable by two public paths** — `workloads::apply::Paths` (a re-export) and
  `workloads::paths::Paths`. Consumers are split between them. Pick one.
- **No crate-root API surface.** `lib.rs` re-exports nothing, so consumers write
  `kuadrat_core::workloads::apply::apply`. Add root re-exports while there is still one consumer.
- **`thiserror` is declared but unused.** Either land the design's stage-tagged error enum in phase
  2 or drop the dependency.
- ~~**The CLI has no tests of its own.**~~ **Closed 2026-08-11.** The surface grew as predicted, and
  two arms had stopped being pure dispatch: `--route` parsing and the app name `build` derives from
  a repo path. Both moved to `crates/cli/src/args.rs` (`parse_route`, `app_name`) with 13 tests, and
  the `match` arms call them. The extraction closed three holes the inline versions had: `:3000`
  parsed as an empty domain, `https://example.com:3000` parsed the scheme as part of the domain, and
  `:0` parsed as port 0 — each of which would have rendered a Caddy fragment that loads but serves
  nothing. `app_name` now also rejects a directory whose name slugs to empty, which would have
  produced the image tag `localhost/kuadrat-:<sha>`. The remaining arms are still pure dispatch and
  are covered by the acceptance scripts.

## From H3 — journald content is not sanitised

`logs::tail` and `logs::search` return whatever the application wrote to its
stdout and stderr. A workload that logs a token or a password will have that value returned by
this module and, from H4 onward, rendered in the web UI and served over the API. kuadrat's own
code never writes a secret to a log — the secrets stage reports names only — but it cannot vouch
for what a deployed application logs.

This is why the daemon binds loopback and ships no authentication: log content is as sensitive as
the least careful app on the host. Log lines also pass through lossy UTF-8 conversion and may
carry ANSI escapes, control characters, and HTML such as `<script>`; escaping belongs at the
render boundary in the web UI, not in this module, because escaping here would corrupt the JSON
consumer.

## From H3 — the privilege signal trusts an inherited environment variable

`logs::journal_unreadable` tells "this app is quiet" apart from "this process cannot read the
journal" by matching journald's stderr hint. That hint is emitted through systemd's logging,
which honours `$SYSTEMD_LOG_LEVEL` from the process's environment, and `LocalExecutor` runs
`Command::output()` inheriting whatever environment the daemon started with. A daemon started
with `SYSTEMD_LOG_LEVEL=warning` or higher gets journalctl's hint suppressed exactly as `-q`
would have — empty stderr, exit 0 — and `logs::tail`/`logs::search` silently return `Ok(vec![])`
for a journal they were never able to read, indistinguishable from a genuinely quiet unit.

`Executor` has no environment parameter today, so this cannot be closed inside `logs`. Two
things carry forward: operationally, `SYSTEMD_LOG_LEVEL` must not be set to `warning` or above
for the daemon's process; and structurally, pinning `SYSTEMD_LOG_LEVEL=info` and `LC_ALL=C` for
the journalctl child specifically belongs with a future `Executor` env parameter, not a
workaround in this module.

## From H3 — live tailing was deferred to phase 4 (CLOSED, 2026-08-13)

Phase 4 landed it, rather than the gap surviving as a standing note in this file (H3's own
checklist only required recording journald sanitisation, above — the deferral itself lived in the
phase-3 design docs, not here). `Executor::run_streaming` is a new seam
(`crates/core/src/exec/mod.rs`), with a default impl that `bail!`s so a future executor — the
fleet driver's SSH one — compiles before it opts in; `LocalExecutor` implements it by holding the
`Child` alongside the stream, so dropping the stream (`kill_on_drop`) is what actually stops
`journalctl -f`. `logs::follow` (`crates/core/src/logs/mod.rs`) runs the existing bounded `tail`
as a pre-flight first, because journald's "you may not read this journal" hint arrives on stderr
while the process still exits 0 and prints `-- No entries --` to stdout — a stream carrying stdout
alone cannot tell that apart from a quiet app — and its backlog is clamped to `logs::MAX_LINES`
like every other read.

Two endpoints serve the same stream: `GET /api/apps/:name/logs/stream` (JSON) and the same
`GET /app/:name` route with `?follow=1` (SSE-driven htmx, via `lines_sse` — a second, simpler SSE
engine than the deploy-events one, because log lines have no store ids to dedupe or resume from).
The backlog is 100 lines and the connection is capped at 30 minutes. A page-level pre-flight
catches an unreadable journal before the follow view ever opens its `EventSource`, rendering the
same `#log-error` text the static tail shows — without that check the operator would see a
permanently blank list, because `EventSource` retries a JSON 500 forever rather than surfacing it.

The second consumer of the JSON endpoint — the MCP surface — has not landed yet.

## From H6 — vendored frontend assets

`crates/daemon/assets/` carries htmx 2.0.10 and `htmx-ext-sse` 2.2.4, vendored because the daemon
binds loopback on a host that may have no outbound network. They are the only third-party code in
the repository and nothing updates them automatically: a published security advisory against either
will not surface here. `assets/PROVENANCE.md` records the upstream URL, version, and SHA-256 of
each, which is what makes an update auditable — check it when either project publishes a release.

## From H6 — the form routes have no CSRF defence

`POST /apps` and the browser branch of `POST /api/apps/:name/deploy` are plain HTML forms: no CSRF
token, no `Origin` or `SameSite` check. They are the first state-changing routes this daemon exposes
to a browser.

Loopback-only is not by itself a defence here. Any page open in the operator's browser can submit a
cross-origin form POST to `127.0.0.1` — the browser will send it, and nothing on these routes
distinguishes it from a click on kuadrat's own page. What loopback does buy is that the attacker
must already have the operator loading their page; what it does not buy is immunity.

Deliberately not fixed in H6: the phase binds loopback and ships no authentication at all, so a
token would be the only control on a surface that has no others, and it would suggest a boundary
that is not there. It must be fixed in the same change that gives the daemon authentication or
reachability beyond loopback — whichever comes first — and not after.

## From H7 — no socket activation, and what it would take

The daemon binds its own port, so it holds its memory whenever it runs — a few megabytes that a
socket-activated unit would not. Socket activation was rejected in H7 because it moves the listen
address into the `.socket` unit, where `Config::validate`'s loopback refusal no longer governs it.

Revisit only with a replacement for that guard: the daemon would have to check the address of the
socket it inherits and refuse a non-loopback one, which is the same rule enforced one step later.

## H7 acceptance — UNRUN

`scripts/serve-acceptance.sh` exists and is wired into `scripts/verify-all.sh`, but it has never
been run. It needs root (system Quadlet units + `daemon-reload`) and starts a real daemon on this
host, which is why it is not run automatically. Run it with:

```bash
PATH=$HOME/.cargo/bin:$PATH cargo build --release
sudo bash scripts/serve-acceptance.sh
```

## From H7 acceptance — a local deploy hung in Healthcheck, unexplained

**Re-tested 2026-09-20 on kyy-acer (Ubuntu 26.04, Podman 5.7, root acceptance): the
hang did not reproduce.** `scripts/serve-acceptance.sh` check 8 — the in-process
fallback deploy, the exact check that hung for 180s on three runs in August —
reached `Done` cleanly, 14/14, and every other acceptance suite passes too
(`verify-all.sh` → ALL ACCEPTANCE: PASS). The most plausible original trigger is
host/podman-version specific: `podman healthcheck run` behavior changed between
the 4.x-era host where the hang was recorded and 5.7, and the stage's bound is
read-only data — its per-attempt timeout and kill-on-drop are pinned by tests
(`poll_health_*` budget tests, `local_executor_kills_child_when_future_is_dropped`).

`scripts/serve-acceptance.sh` check 8 fails: the in-process fallback deploy hangs in the Healthcheck
stage until the script's own `timeout 180` kills it. Three runs, same result. The other thirteen
checks pass.

What is established:

- The deploy's last event is `healthcheck started`; there is no terminal event, the row stays
  `in_progress`, and its lock stays held (freed by `kuadrat reconcile`).
- **The container is healthy the whole time.** `podman ps` during the hang shows
  `Up 3 minutes (healthy)` with the port published, and `systemctl status` shows the unit active
  since two seconds before the healthcheck began.
- The binary running it has the wall-clock bound (`did not become healthy within` present,
  `after N checks` absent) and rendered the `HealthTimeout=5s` in the unit, so it is not a stale
  build.
- `poll_health` is bounded on paper: a 60s budget, each attempt wrapped in a 5s
  `tokio::time::timeout`, `kill_on_drop(true)` on the child.
- `podman healthcheck run` against a healthy container returns in ~0.15s, reproduced rootless.

So every static reading says the stage should give up at 60s with an error and roll back. It does
not, and where the 180 seconds go is unknown.

The observation that would settle it has to be taken **while it is hung**: `/proc/<pid>/wchan` for
the CLI, and `ps -eLf` to see whether a `podman` child outlived its cancelled future. The
possibilities not yet excluded are a starved tokio runtime, a child that cancellation does not
detach from, and something not yet considered.

Not blocking phase 4: the feature works — the same deploy succeeds through the daemon in about a
second, and the fallback's own message prints correctly. What is untrustworthy is the *bound*, which
means a pathological deploy can still stall a CLI invocation indefinitely.

**Hardened 2026-09-20:** `run_stages` now wraps the whole Healthcheck stage in a
90s `tokio::time::timeout` (`HEALTHCHECK_STAGE_CAP`) that is independent of
`poll_health`'s internal budget. The internal bound assumes the runtime can
yield to timers — a child that cancellation does not detach from (one of the
two live hypotheses above) stalls the future without resolving. The stage-level
cap sits outside that machinery, so even then the stage fails with a named
error (`healthcheck stage exceeded its 90s hard cap`) and the deploy rolls
back. Pinned by a regression test with an executor whose healthcheck never
resolves (`a_healthcheck_that_never_resolves_is_cut_by_the_stage_cap_and_rolls_back`).
So the residual risk from this entry is reduced to: a hang on a single-threaded
runtime with no timer thread — the CLI runs `#[tokio::main]` (multi-threaded),
so that case does not apply to the fallback path that this entry is about.

## From phase 4 — a followed stream holds a `journalctl` for up to 30 minutes

Each viewer following a log holds one `journalctl -f` process. Dropping the stream kills it, and a
30-minute ceiling bounds the connection the server never notices dropping — but a host with several
operators watching several apps holds one process each for as long as they watch.

That is the intended cost of live tailing and not a defect. It is recorded because the premise of
this project is a low-memory host, and "how many followers is too many" has never been measured.

The same trade shows up in the browser, not just on the host. The follow view's `<ul>` grows with
`hx-swap="beforeend"` for the life of the connection — nothing ever removes a line — and on the
30-minute reconnect the new stream replays its 100-line backlog straight into the same list, so a
long-watched chatty app accumulates unbounded DOM plus 100 duplicated lines every half hour. This
costs the host nothing, and the design accepted the reconnect explicitly, so it is a recorded cost
of leaving a follow view open indefinitely, not a defect to fix.

## From phase 4 — on SIGTERM, systemd kills the followers, not the daemon

`ChildLines` holds its `Child` so that dropping a stream drops the child and `kill_on_drop` kills the
`journalctl -f`. That closes three of the four ways a followed stream ends: the client disconnects
(the keep-alive write fails within about fifteen seconds and hyper drops the body), the 30-minute
ceiling elapses, or the source errors. On all three the daemon is running and the drop happens.

The fourth is daemon shutdown, and there `kill_on_drop` does **not** fire.
`crates/daemon/src/lib.rs:71` calls `axum::serve(...)` with no `.with_graceful_shutdown`, so SIGTERM
terminates the process without unwinding: nothing is dropped, and a `journalctl -f` on a quiet unit
will not notice its stdout pipe is broken until its next write, which may never come.

What actually reaps them is the deployment shape. `packaging/kuadrat.service` is `Type=simple` and
sets no `KillMode`, so systemd's default of `control-group` applies and the whole cgroup is
SIGTERMed then SIGKILLed on stop or restart. Ctrl-C from a shell reaches the children too — same
process group, no `setsid`. The only leak window is `kill <pid>` against a hand-launched daemon with
a quiet followed unit, which is not a shipped configuration.

Two things follow, and they are the reason this is written down:

- **The code and the packaging are jointly load-bearing, and neither file says so.** The unit file
  never mentions `KillMode` because it is taking the default — so a later change that sets
  `KillMode=process`, for what would look like an unrelated reason, would silently start orphaning a
  `journalctl` per abandoned viewer on every restart.
- **Do not add `with_graceful_shutdown` naively.** It is normally an unambiguous improvement, and
  here it is a hang: graceful shutdown waits for response bodies to end, and these bodies are
  deliberately long-lived, so shutdown would block on every open follower for up to the full
  30-minute ceiling. Adding it means also giving the followers a shutdown signal to end on.

## From phase 5 — the MCP surface trusts loopback exactly as far as the daemon does

`kuadrat mcp` adds no listener: the client spawns it and owns its pipe, and it talks to the
daemon over the same unauthenticated loopback HTTP every other client uses. It therefore
inherits the auth/CSRF trigger recorded above rather than adding to it — but a host where
untrusted local processes can reach 127.0.0.1:7457 was already trusting them with the daemon,
and the MCP surface makes that reach more convenient. Same trigger, same fix, same change.

## From phase 5 — a daemon that dies mid-session becomes a per-call tool error

The startup probe keeps "no daemon" out of the per-call path, but a daemon that exits after the
probe surfaces as a tool error naming `kuadrat serve` on every subsequent call. An agent may
retry a few times before reading it. Deliberate: exiting the MCP process mid-session would take
the agent's whole surface down to report one dead dependency.

## From phase 6 — no hook queue: a push during a deploy is dropped with a receipt

A delivery that arrives while a deploy of the same app is in flight is answered
`200 {"ignored": "deploy in progress"}` and recorded as a failed attempt in the timeline —
never queued. The operator resolves it by pushing again or redelivering from the forge. A queue
is real machinery (ordering, coalescing, retry policy) for a case with a one-click manual fix;
build it when a real workflow hits this more than occasionally.

## From phase 6 — Gitea and Bitbucket are unclaimed

Gitea sends GitHub-shaped deliveries and may work against `/hooks/github/:app` as-is; nothing
tests or promises it. Bitbucket's shape differs and is simply not implemented.

## From phase 6 — task runs have no kuadrat-side history

A scheduled task's result lives where systemd puts it: `systemctl list-timers`, the oneshot
service's status, and the journal (readable via `tail_logs` only if the task logs under the
app's unit, which it does not — it is its own unit). kuadrat's store records deploys, not task
runs. If task observability matters later, the store's event model is where it would go.
