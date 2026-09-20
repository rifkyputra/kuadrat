# kuadrat

**Podman Quadlet deployment daemon for a single host.**

Take a git repo to a running service with TLS — on systemd, without a container daemon.

> Status: **pre-alpha**. Phases 1 through 7 are merged — the CLI builds, deploys, routes, holds
> secrets, rolls back, and recovers from a crash; `kuadrat serve` runs a daemon with a designed
> operator console (dark/light, live deploy progress, live log following), signed GitHub/GitLab
> push-to-deploy hooks, and an outbound webhook; specs can declare scheduled tasks on systemd
> timers; and `kuadrat mcp` gives an agent six tools over stdio. `packaging/kuadrat.service`
> runs the daemon under systemd. It binds loopback only, with no authentication — reach it
> remotely over an SSH tunnel or a VPN. See the [Guide](#guide) to use it today.

## Why

Deploying an app to one Linux server means choosing between a Kubernetes-shaped stack that
costs more RAM than the app, or a Docker-based PaaS that runs a daemon and a second supervisor
alongside systemd.

Podman Quadlet removes the daemon — containers become native systemd units — but nothing wraps
it in a deployment loop. There is no "repo to running service with TLS" path on Quadlet, no
single place to see status and logs, and no agent-operable interface.

kuadrat is that layer. systemd stays the supervisor; kuadrat is the thing that puts workloads
in front of it.

### Measured

The daemon claim above is testable, so [`examples/`](examples/) ships the same app twice — once
as a kuadrat deployment, once as a Docker Compose service, with a **byte-identical** `app.py` and
base image — plus [`examples/bench.py`](examples/bench.py) to measure both.

Supervisor processes only, not the app itself. Ubuntu 24.04, 2 cores, Docker 29.1.3 vs
Podman 4.9.3:

| | Docker | kuadrat |
|---|---|---|
| **Fixed** — before any container exists | `dockerd` 88.8 MB + `containerd` 51.5 MB = **140.3 MB** | **0** — systemd is already running |
| **Marginal** — per container | `containerd-shim` 10.4 + 2× `docker-proxy` 8.9 = **19.3 MB** | `conmon` **2.5 MB** |
| **Ten containers** | **334 MB** | **25 MB** |

**7.8× per container**, and Docker pays the 140 MB before the first one starts. On a 1 GB VPS
that fixed cost alone is 14% of the machine. Docker runs *two* `docker-proxy` processes per
published port — one per address family — while `conmon` is the only per-container process on the
Quadlet side, because systemd is the supervisor and it is running regardless.

**Throughput showed no difference, and that is the honest result.** One run suggested kuadrat was
faster; it did not reproduce, and bypassing `docker-proxy` by hitting the container IP directly was
no faster than going through it — ruling out the obvious explanation. Both runtimes start the same
process under the same kernel. Anyone quoting a throughput win here is quoting noise.

Two caveats before citing these figures: memory is **RSS, not PSS** (`smaps_rollup` is unreadable
for root-owned processes without `sudo`), which double-counts shared pages and therefore *inflates*
the multi-process side — the direction is not in doubt but the ratio would shrink. And the host was
already running ~16 other Docker containers, so `dockerd`'s working set is fatter than a clean
host's; the marginal figures are matched by container id and port and are unaffected. Full method
and caveats in [`examples/hello-py-docker/README.md`](examples/hello-py-docker/README.md).

## What it does

```
kuadrat deploy pbrain
  Detect → Build → Secrets → Apply → Route → Healthcheck → Done
                                                   └── failure → RolledBack
```

- **Deploy loop** — detect stack, build, render a `.container` unit, route through Caddy with
  automatic TLS, healthcheck, roll back on failure
- **Secrets** — `podman secret` management; specs carry names, never values
- **Logs** — journald reads scoped to a unit, streamable live as JSON for an API client
- **Web UI** — an operator console: fleet status at a glance, live deploy timelines, a real log
  console with live following, dark and light, keyboard-accessible — one plain stylesheet, no
  build step, no framework
- **Push to deploy** — a `git push` to GitHub or GitLab redeploys the app through a signed
  webhook; a failed hook lands in the deploy timeline, not a void
- **Scheduled tasks** — spec-declared commands on systemd timers, each run a fresh container
  from the app's image; results live in `systemctl list-timers` and the journal
- **MCP surface** — `kuadrat mcp` speaks MCP over stdio: an agent can list apps, deploy, watch a
  deploy's stages, tail a journal, and reconcile after a crash. Deliberately no `remove` and no
  secrets — it proposes, a human approves the irreversible
- **Events** — typed and subscribable; kuadrat emits, subscribers deliver

## Guide

Everything below works against the current code. Commands that write units, secrets, or Caddy
fragments touch `/etc` and `/var/lib`, so they need **root**.

### Install

Two ways in, one way out — `scripts/install.sh` is the single entry point for
install, upgrade, and uninstall, and it is idempotent by construction
(re-running with a newer binary in place is the upgrade). It places the
binary at `/usr/local/bin`, writes the shipped `packaging/kuadrat.service`, and
enables + starts the daemon.

**From source:**

```bash
git clone git@github.com:rifkyputra/kuadrat.git && cd kuadrat
sudo make install        # builds the release binary, then installs and starts the service
```

**From a release artifact** (binary + unit + SHA256 manifest are attached to
every `v*` GitHub release):

```bash
# download kuadrat-vX.Y.Z and kuadrat.service from the release page
sudo bash scripts/install.sh ./kuadrat-vX.Y.Z ./kuadrat.service
```

**Upgrade:** get the newer binary (pull + `make` or download the newer
release) and re-run the install command — the unit is rewritten and the
service restarts onto the new binary. `kuadrat --version` prints the running
version.

**Uninstall:**

```bash
sudo make uninstall      # or: sudo bash scripts/install.sh --uninstall
```

Needs Podman 4.4+, systemd on cgroups v2, and `git`. Caddy is only required if
you route an app (see [Routing](#routing-an-app)).

### Your first deploy

An app is a **local repo with a Containerfile** (or Dockerfile) plus a `kuadrat.json` describing
how it should run. kuadrat never clones — you or CI put the code on the host.

```jsonc
// ~/apps/worker/kuadrat.json
{
  "name": "worker",              // overwritten by the app argument; keep them the same
  "image": "",                   // ignored on deploy — the build fills it in
  "command": null,
  "env": [["LOG_LEVEL", "info"]],
  "ports": [],
  "volumes": [],
  "secrets": [],
  "memory_max": "256M",
  "health_cmd": null,
  "restart_policy": "Always",
  "route": null
}
```

```bash
sudo kuadrat deploy worker ~/apps/worker
```

That runs Detect → Build → Secrets → Apply → Route → Healthcheck. It prints the outcome and
**exits non-zero on anything but `Done`**, so CI can gate on it. With `route: null` the Route stage
is a no-op and Caddy is never called; the healthcheck falls back to `systemctl is-active`.

Then:

```bash
kuadrat list                      # kuadrat-managed workloads
kuadrat status worker             # Running / Stopped / Failed / Not installed / Unknown
journalctl -u kuadrat-worker -f   # logs — it's a normal systemd unit
sudo kuadrat remove worker
```

Every artefact carries the `kuadrat-` prefix, so `worker` becomes the unit `kuadrat-worker` and
can never collide with a hand-written `worker.container` or the host's own `worker.service`.

**Where the spec comes from**, in order: `kuadrat.json` in the repo → the spec stored from the app's
last deploy → error. The `app` argument always wins over the spec's `name` field, and `--route`
always wins over the spec's `route`. So a redeploy after an edit is just `kuadrat deploy worker
~/apps/worker` again; the image is rebuilt and tagged `localhost/kuadrat-worker:<git-sha>`.

### Routing an app

A route is a domain reverse-proxied to a container port, served by Caddy with automatic TLS.

```bash
sudo kuadrat deploy web ~/apps/web --route example.com:3000
```

Two prerequisites, both one-time:

1. **Caddy is installed and running.** kuadrat writes `/etc/caddy/kuadrat.d/<slug>.caddy` and runs
   `systemctl reload caddy`; it does not manage Caddy's lifecycle.
2. **Your Caddyfile imports the fragments** — add `import kuadrat.d/*.caddy`. Without that line the
   fragment lands on disk and serves nothing.

A routed spec **must** set `health_cmd`; `validate()` rejects a route without one. Public traffic
must not reach something with no readiness signal:

```jsonc
"health_cmd": "curl -fsS http://localhost:3000/health",
"route": { "domain": "example.com", "port": 3000 }
```

### Secrets

Specs carry secret **names**; values live in `podman secret` and never appear in a spec, a unit
file, or argv. Values are read from stdin only — argv is world-readable via `ps`.

```bash
printf '%s' "$TOKEN" | sudo kuadrat secret set api-token
kuadrat secret ls
sudo kuadrat secret rm api-token
```

Reference it by name in the spec (`"secrets": ["api-token"]`) and the Secrets stage fails the
deploy up front if a named secret is missing — so a deploy fails safe rather than serving
half-configured.

### When a deploy fails

Failure triggers compensation in reverse from the stage that failed, and the outcome is
`RolledBack`. A failure at Detect, Build, or Secrets touched nothing on the host — the old version
is still serving. A failure at Apply re-applies the previously deployed spec, or removes the unit
if this was the app's first deploy. A failure at Route or Healthcheck unwinds the route first, then
the unit. The outcome is `Failed` only when compensation *itself* fails — that one wants a look.

If the host dies mid-deploy, the app is left locked and `in_progress` in the store. Recover it:

```bash
sudo kuadrat reconcile
```

It rolls back anything still in flight and releases the lock. Idempotent — safe to run on every
boot, and the natural thing to wire into a `kuadrat-reconcile.service` with
`After=network-online.target`.

### Where things live

| Path | What | Override |
|---|---|---|
| `/etc/containers/systemd/kuadrat-<slug>.container` | the generated Quadlet unit | `--root <dir>` |
| `/var/lib/kuadrat/kuadrat.db` | SQLite: specs, deploy history, stage, locks, events | `--root <dir>` |
| `/etc/caddy/kuadrat.d/<slug>.caddy` | the Caddy fragment | `--root <dir>` |

`--root <dir>` relocates all three under one directory — for dry runs and testing without touching
the real host. kuadrat only ever overwrites files it owns: units carry a `# kuadrat-managed: true`
marker, and a foreign file at a target path is refused, never clobbered.

### Spec reference

| Field | Type | Notes |
|---|---|---|
| `name` | string | overwritten by the `app` argument on deploy |
| `image` | string | ignored on deploy (the build sets it); used by `kuadrat apply` |
| `command` | string[] \| null | argv; each element is one argument |
| `env` | [string, string][] | rendered as `Environment=` |
| `ports` | string[] | `"host:container"` |
| `volumes` | string[] | `"host:container"` |
| `secrets` | string[] | podman secret **names** only |
| `memory_max` | string \| null | e.g. `"256M"` |
| `health_cmd` | string \| null | **required** when `route` is set |
| `restart_policy` | `"Always"` \| `"OnFailure"` \| `"No"` | |
| `route` | `{domain, port}` \| null | needs Caddy |
| `tasks` | `{name, schedule, command}[]` | systemd timers; `schedule` is an `OnCalendar` expression (`daily`, `*-*-* 03:00:00`), `command` runs in a fresh container from the app's image with its env and secrets |

Newlines and carriage returns are rejected in every rendered field — a `\n` in an env value would
otherwise inject directives (`Secret=`, `User=`) nobody wrote. `%` is escaped so systemd does not
expand it as a specifier.

### Commands

| Command | What |
|---|---|
| `deploy <app> <path> [--route domain:port]` | the full loop: build from a local repo and run it |
| `build <path>` | build and tag the image only; prints the reference |
| `apply <file.json>` | apply a spec directly — no build, no route |
| `remove <name>` / `status <name>` / `list` | manage applied workloads |
| `secret set\|ls\|rm <name>` | podman secrets; values via stdin |
| `reconcile` | roll back deploys left in flight by a crash |
| `serve [--listen addr]` | run the HTTP daemon: API, web UI, event stream. Loopback only |
| `mcp [--listen addr]` | speak MCP over stdio for an agent; requires a running daemon |

### The MCP surface

`kuadrat mcp` is how an agent operates this host: an MCP client spawns it and gets six tools —
`list_apps`, `get_app`, `deploy`, `get_deploy`, `tail_logs`, `reconcile`. It talks to the daemon
over loopback and refuses to start without one, so an agent can never run a deploy the daemon's
timeline does not contain. `deploy` returns its `deploy_id` immediately; the agent polls
`get_deploy`. Register it with Claude Code:

```bash
claude mcp add kuadrat -- kuadrat mcp
```

Deliberately absent: `remove` (the one irreversible operation — a human runs it), the secret
commands (values are stdin-only by construction, a property a JSON tool call cannot provide), and
live log following (a tool call is request/response; `tail_logs` is the bounded snapshot an agent
can read in one turn). It answers both current MCP clients (per-request versioning,
`server/discover`) and older handshake-based ones (`initialize`).

### Push to deploy

A `git push` can redeploy an app: GitHub and GitLab POST to the daemon, kuadrat verifies the
delivery, resets the host repo to the pushed commit, and runs the same deploy the button runs.

1. **Configure the shared secret** (the routes answer 404 until one exists). Same file-over-env
   reasoning as the outbound webhook:

| Variable | Value |
|---|---|
| `KUADRAT_HOOK_SECRET` | the secret directly |
| `KUADRAT_HOOK_SECRET_FILE` | path to a file containing it |

2. **Expose the hook routes.** The daemon stays loopback-only; the signature is the
   authentication, so exposure is one Caddy block (or an SSH/Cloudflare tunnel):

```caddy
hooks.example.com {
    reverse_proxy 127.0.0.1:7457
}
```

3. **Point the forge at the app's route** with the same secret:
   GitHub → `https://hooks.example.com/hooks/github/<app>` (secret in the webhook's Secret
   field, `application/json` payload); GitLab → `https://hooks.example.com/hooks/gitlab/<app>`
   (Secret token field).

Only a push to the branch the host repo has checked out deploys; anything else — another
branch, a tag, a delivery during an in-flight deploy — is answered `200 {"ignored": <why>}` so
the forge never marks the hook broken. The checkout is `reset --hard` to the pushed commit:
a push-to-deploy working copy is a deployment surface, not a workspace. Failures (bad branch
read, fetch, reset) are recorded in the app's deploy timeline where an operator will look.

### The webhook

`kuadrat serve` can POST a JSON message to a webhook whenever a deploy reaches a terminal outcome
(`Done`, `RolledBack`, `Failed`) or a stage fails — not every event, just the ones worth a line in
chat.

Configure it with one of:

| Variable | Value |
|---|---|
| `KUADRAT_WEBHOOK_URL` | the URL directly |
| `KUADRAT_WEBHOOK_URL_FILE` | path to a file containing the URL |

A webhook URL carries its token in its path, so prefer `KUADRAT_WEBHOOK_URL_FILE`: a systemd
`Environment=` line is readable by anyone who can run `systemctl show`, but a file loaded through
`LoadCredential=`, or via `EnvironmentFile=` as in `packaging/kuadrat.service`, is not. Neither
variable, nor the URL itself, ever reaches argv — see `crates/daemon/src/webhook.rs`.

## Design principles

- **systemd is the orchestrator.** kuadrat renders restart policies and timers, but runs no
  supervisor or scheduler of its own — systemd already does. Adding a second one is the problem,
  not the solution.
- **The spec is the source of truth.** Unit files are derived artifacts kuadrat owns and may
  overwrite. Rollback is re-applying a previous spec, not diffing files.
- **The core never touches the network.** `kuadrat-core` manipulates the local filesystem,
  systemd, and podman, and knows nothing about hosts. Everything network-facing lives in the
  daemon. See [ADR-0002](docs/adr/0002-transport-agnostic-core.md).
- **Emit events, don't deliver them.** No notification providers, no chat integrations.
  Dedupe and delivery belong to the subscriber.

## Scope

**v1:** deploy loop, secrets, logs, web UI, MCP surface, event stream, signed GitHub/GitLab
push-to-deploy hooks, and spec-declared scheduled tasks on systemd timers.

**Not in v1:** multi-host orchestration, blue/green deploys, notification delivery, metrics,
backups, preview deployments, or autonomous agent action.

## Requirements

**Podman 4.4+** (when Quadlet landed), systemd with **cgroups v2**, and Caddy. Validated on
Podman 4.9.3 / Ubuntu 24.04. Podman 6 removed cgroups v1, CNI, `slirp4netns`, and BoltDB — kuadrat
targets the modern stack, so hosts still on cgroups v1 defaults are unsupported.

The daemon binds loopback only (`kuadrat serve --listen 127.0.0.1:7457` by default) — reaching the
UI from elsewhere is the operator's job via SSH tunnel or VPN.

## Documentation

| Document | What |
|---|---|
| [Phase 1 design](docs/design/2026-08-10-design.md) | Architecture, components, data flow, error handling, testing |
| [Phase 2 design](docs/design/2026-08-10-phase-2-deploy-loop.md) | The deploy loop: state machine, gateway, secrets, store, events |
| [Phase 3 design](docs/design/2026-08-11-phase-3-daemon-and-surfaces.md) | The daemon: HTTP API, SSE, htmx UI, logs, webhook |
| [Phase 4 design](docs/design/2026-08-11-phase-4-live-logs.md) | The streaming seam and live log tailing |
| [Phase 5 design](docs/design/2026-08-13-phase-5-mcp-surface.md) | The daemon-backed MCP agent surface |
| [Phase 6 design](docs/design/2026-08-18-phase-6-push-and-timers.md) | Signed push-to-deploy hooks and systemd-native scheduled tasks |
| [Examples](examples/) | A runnable app, its Docker equivalent, and the runtime benchmark |
| [Plans](docs/plans/) | Per-gate implementation plans, task by task |
| [Known gaps](docs/known-gaps.md) | Deferred findings, acceptance records, what to re-read before which phase |
| [ADRs](docs/adr/) | Decisions and their reasoning |

## Prior art

[quadit](https://crates.io/crates/quadit) (GitOps for Quadlet),
[quadletman](https://github.com/mikkovihonen/quadletman) (web UI),
[podlet](https://github.com/containers/podlet) (one-shot compose conversion),
Cockpit (Quadlet management in a web console), and the Docker-only PaaS family
(Dokku, Kamal, Coolify, CapRover).

The unoccupied combination is deploy loop + gateway/TLS + agent surface on Quadlet.

## License

Apache-2.0
