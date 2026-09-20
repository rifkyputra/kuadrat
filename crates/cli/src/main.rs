use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use kuadrat_core::events::null::NullSink;
use kuadrat_core::exec::local::LocalExecutor;
use kuadrat_core::fs::local::LocalFileSystem;
use kuadrat_core::spec::WorkloadSpec;
use kuadrat_core::workloads::apply::{apply, remove, Paths};
use kuadrat_core::workloads::query::{list, status};

mod args;
mod daemon_client;
mod resolve;

#[derive(Parser)]
#[command(
    name = "kuadrat",
    version,
    about = "Podman Quadlet deployment for a single host"
)]
struct Cli {
    /// Treat all paths as relative to this root (for testing without touching /etc)
    #[arg(long)]
    root: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply a workload spec from a JSON file
    Apply { file: std::path::PathBuf },
    /// Remove a workload by name
    Remove { name: String },
    /// Show a workload's state
    Status { name: String },
    /// List kuadrat-managed workloads
    List,
    /// Build a repo's image without deploying it
    Build { path: std::path::PathBuf },
    /// Build and deploy an app from a local repo
    Deploy {
        app: String,
        path: std::path::PathBuf,
        /// Route this app: domain:port (e.g. example.com:3000)
        #[arg(long)]
        route: Option<String>,
        /// Where a running daemon would be listening. Tried first; only a
        /// daemon that doesn't answer here falls back to an in-process
        /// deploy. Must match the address `kuadrat serve --listen` was given
        /// — the two default the same way so this only needs setting when
        /// `serve` was told to use a non-default one.
        #[arg(long, default_value_t = args::default_listen())]
        listen: std::net::SocketAddr,
    },
    /// Manage podman secrets (values read from stdin, never argv)
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// Recover from crashed deploys: roll back anything left in progress
    Reconcile,
    /// Run the HTTP daemon: the API, the pages, and the event stream
    Serve {
        /// Address to listen on. Loopback only — the daemon has no
        /// authentication, so it refuses to bind anything else. Reach it
        /// remotely with an SSH tunnel or a VPN.
        #[arg(long, default_value_t = args::default_listen())]
        listen: std::net::SocketAddr,
    },
    /// Speak MCP over stdio for an agent, operating the daemon at --listen.
    /// Refuses to start when no daemon answers: an agent cannot see a
    /// fallback message, so a silent second code path would let it report
    /// deploys the daemon's timeline does not contain.
    Mcp {
        /// The daemon's address — the same default `kuadrat serve` binds.
        #[arg(long, default_value_t = args::default_listen())]
        listen: std::net::SocketAddr,
    },
}

#[derive(Subcommand)]
enum SecretAction {
    /// Create or replace a secret; the value is read verbatim from stdin
    Set { name: String },
    /// List secret names
    Ls,
    /// Remove a secret
    Rm { name: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = match &cli.root {
        Some(root) => Paths::rooted(root),
        None => Paths::default(),
    };
    let exec = LocalExecutor;
    let fsys = LocalFileSystem;

    match cli.command {
        Command::Apply { file } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let spec: WorkloadSpec = serde_json::from_str(&text).context("parsing spec JSON")?;
            apply(&exec, &fsys, &paths, &spec).await?;
            println!("applied {}", spec.name);
        }
        Command::Remove { name } => {
            remove(&exec, &fsys, &paths, &name).await?;
            println!("removed {name}");
        }
        Command::Status { name } => {
            let state = status(&exec, &fsys, &paths, &name).await?;
            println!("{}", state.label());
        }
        Command::List => {
            for name in list(&fsys, &paths).await? {
                println!("{name}");
            }
        }
        Command::Build { path } => {
            use kuadrat_core::deploy::{build::build, detect::detect};
            use kuadrat_core::spec::slug;

            let abs = path
                .canonicalize()
                .with_context(|| format!("no such path: {}", path.display()))?;
            let name = args::app_name(&abs)?;
            let plan = detect(&exec, &fsys, &abs).await?;
            let image = build(&exec, &plan, &slug(name)).await?;
            println!("{image}");
        }
        Command::Deploy {
            app,
            path,
            route,
            listen,
        } => {
            use kuadrat_core::deploy::{run, Ctx, DeployOutcome};
            use kuadrat_core::store::Store;

            let route_override = route.map(|s| args::parse_route(&s)).transpose()?;

            let store = Store::open(&paths.db_path)?;

            // Back-fill the registration the daemon handoff below depends on
            // — see `resolve::backfill_registration` for why, and
            // docs/known-gaps.md's now-closed "CLI-deployed apps have no
            // registration" entry.
            resolve::backfill_registration(&store, &app, &path, route_override.clone())?;

            // Prefer a running daemon: it queues behind the global
            // one-at-a-time semaphore and gives this deploy an addressable
            // `/deploy/:id` page. Only an unreachable daemon falls back to
            // running in-process — a refusal is the daemon's answer and is
            // reported, never retried locally. When `--root` is set the
            // handoff is skipped outright: see
            // `daemon_client::try_deploy_unless_rooted` for why.
            if cli.root.is_some() {
                println!("--root is set; deploying locally instead of handing off to a daemon");
            }
            match daemon_client::try_deploy_unless_rooted(&exec, listen, &app, cli.root.as_deref())
                .await
            {
                daemon_client::Handoff::Accepted { deploy_id } => {
                    println!("queued as deploy {deploy_id} on the running daemon");
                    println!("http://{listen}/deploy/{deploy_id}");
                    return Ok(());
                }
                daemon_client::Handoff::Refused { status, message } => {
                    match status {
                        Some(status) => {
                            eprintln!("daemon refused the deploy ({status}): {message}")
                        }
                        None => {
                            eprintln!("daemon refused the deploy (status unknown): {message}")
                        }
                    }
                    std::process::exit(1);
                }
                daemon_client::Handoff::Unreachable => {
                    if cli.root.is_none() {
                        println!("no daemon running; deploying locally");
                    }
                }
            }

            let spec = resolve::resolve_spec(&app, &path, &store, route_override)?;
            let ctx = Ctx {
                exec: &exec,
                fsys: &fsys,
                store: &store,
                paths: &paths,
                sink: &NullSink,
            };
            let outcome = run(&ctx, spec, &path).await?;
            println!("{outcome:?}");
            // A rolled-back or failed deploy exits non-zero (CI-friendly); only
            // `Done` is success.
            if !matches!(outcome, DeployOutcome::Done { .. }) {
                std::process::exit(1);
            }
        }
        Command::Secret { action } => {
            use kuadrat_core::secrets;
            match action {
                SecretAction::Set { name } => {
                    use std::io::Read;
                    let mut value = String::new();
                    std::io::stdin()
                        .read_to_string(&mut value)
                        .context("reading the secret value from stdin")?;
                    secrets::set(&exec, &name, &value).await?;
                    println!("set secret {name}");
                }
                SecretAction::Ls => {
                    for n in secrets::list(&exec).await? {
                        println!("{n}");
                    }
                }
                SecretAction::Rm { name } => {
                    secrets::remove(&exec, &name).await?;
                    println!("removed secret {name}");
                }
            }
        }
        Command::Serve { listen } => {
            kuadrat_daemon::serve(kuadrat_daemon::Config {
                listen,
                root: cli.root,
            })
            .await?;
        }
        Command::Mcp { listen } => {
            let daemon = kuadrat_mcp::CurlDaemon { listen };
            if let Err(e) = kuadrat_mcp::probe(&daemon).await {
                // stderr, never stdout: on stdio, stdout carries only MCP
                // messages.
                eprintln!("{e:#}");
                std::process::exit(1);
            }
            kuadrat_mcp::serve(
                &daemon,
                tokio::io::BufReader::new(tokio::io::stdin()),
                tokio::io::stdout(),
            )
            .await?;
        }
        Command::Reconcile => {
            use kuadrat_core::deploy::{reconcile, Ctx};
            use kuadrat_core::store::Store;

            let store = Store::open(&paths.db_path)?;
            let ctx = Ctx {
                exec: &exec,
                fsys: &fsys,
                store: &store,
                paths: &paths,
                sink: &NullSink,
            };
            let outcomes = reconcile(&ctx).await?;
            if outcomes.is_empty() {
                println!("nothing to reconcile");
            } else {
                for outcome in &outcomes {
                    println!("{outcome:?}");
                }
            }
        }
    }

    Ok(())
}
