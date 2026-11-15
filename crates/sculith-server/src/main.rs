mod config;
mod plugins;
mod protocol;
mod world;

use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sculith_plugin_api::Event;
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::watch,
    task::JoinSet,
    time::MissedTickBehavior,
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use config::Config;

#[derive(Parser)]
#[command(
    name = "sculith",
    version,
    about = "Bedrock-first Rust server in development (Java discovery; opt-in Bedrock spawn preview)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run both discovery listeners and configured Rust plugins.
    Run {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Validate configuration and plugin paths without launching plugins.
    Check {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Show protocol references and actual gameplay support (currently empty).
    Versions,
    /// Convert explicitly mapped Bedrock PNGs into a Java resource-pack directory.
    Pack {
        #[command(subcommand)]
        command: PackCommand,
    },
}

#[derive(Subcommand)]
enum PackCommand {
    Convert {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        mapping: PathBuf,
        #[arg(long)]
        output: PathBuf,
        /// Exact Java client resource pack major format; not the network protocol.
        #[arg(long)]
        pack_format: u32,
        /// Exact Java resource pack minor format (e.g. 1 for format 97.1).
        #[arg(long, default_value_t = 0)]
        pack_format_minor: u32,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse()
        .command
        .unwrap_or(Command::Run { config: None })
    {
        Command::Run { config } => run(Config::load(config.as_deref())?).await,
        Command::Check { config } => {
            let config = Config::load(config.as_deref())?;
            println!(
                "Configuration valid: Java={}, Bedrock={}, plugins={}. Stage: {}.",
                config.network.java.enabled,
                config.network.bedrock.enabled,
                config.plugins.len(),
                config.status()?.stage,
            );
            Ok(())
        }
        Command::Versions => {
            println!("{}", protocol::REFERENCES_JSON);
            Ok(())
        }
        Command::Pack {
            command:
                PackCommand::Convert {
                    input,
                    mapping,
                    output,
                    pack_format,
                    pack_format_minor,
                },
        } => {
            let report =
                sculith_packs::convert(&input, &mapping, &output, pack_format, pack_format_minor)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
    }
}

async fn run(config: Config) -> Result<()> {
    let authentication = if config.network.bedrock.enabled && config.bedrock.sessions {
        Some(
            protocol::bedrock::session::Authentication::new(config.bedrock.require_authentication)
                .await?,
        )
    } else {
        None
    };
    let java = if config.network.java.enabled {
        Some(
            TcpListener::bind(config.network.java.bind)
                .await
                .context("cannot bind Java TCP listener")?,
        )
    } else {
        None
    };
    let bedrock = if config.network.bedrock.enabled {
        Some(
            UdpSocket::bind(config.network.bedrock.bind)
                .await
                .context("cannot bind Bedrock UDP listener")?,
        )
    } else {
        None
    };
    let mut state = config.status()?;
    let mut plugins = plugins::Plugins::load(&config.plugins, &state).await?;
    plugins
        .dispatch(
            Event::Started {
                server: state.clone(),
            },
            &mut state,
        )
        .await;
    let (status_tx, status_rx) = watch::channel(state.clone());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut services = JoinSet::new();
    let (world, world_task) = world::create(shutdown_rx.clone());
    services.spawn(world_task);
    if let Some(listener) = java {
        info!(address = %listener.local_addr()?, "Java TCP discovery listening");
        services.spawn(protocol::java::serve(listener, status_rx.clone()));
    }
    if let Some(socket) = bedrock {
        info!(address = %socket.local_addr()?, "Bedrock UDP discovery listening");
        // Discovery GUID only, never an authenticated identity or secret.
        let guid = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
            ^ u64::from(std::process::id());
        match authentication {
            Some(authentication) => {
                info!("Bedrock encrypted login and read-only flat-world spawn preview enabled");
                if !config.bedrock.require_authentication {
                    warn!(
                        "Offline test identities enabled on loopback; this is not online account verification"
                    );
                }
                services.spawn(protocol::bedrock::serve_sessions(
                    socket,
                    status_rx,
                    guid,
                    authentication,
                    Duration::from_secs(config.bedrock.login_timeout_secs),
                    shutdown_rx,
                    world.clone(),
                ));
            }
            None => {
                services.spawn(protocol::bedrock::serve(socket, status_rx, guid));
            }
        }
    }
    warn!(stage = %state.stage,
        "Development build: Java login, shared multiplayer simulation and content compatibility are not implemented");
    let mut ticker = tokio::time::interval(Duration::from_secs_f64(
        1.0 / f64::from(config.server.tick_rate),
    ));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut tick = 0u64;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            result = &mut shutdown => { break result; }
            service = services.join_next() => {
                break match service {
                    Some(Ok(Err(error))) => Err(error.context("discovery listener stopped")),
                    Some(Err(error)) => Err(error.into()),
                    _ => Err(anyhow::anyhow!("discovery listener exited unexpectedly")),
                };
            }
            _ = ticker.tick() => {
                tick += 1;
                state.online_players = world.online_players();
                plugins.dispatch(Event::Tick { tick }, &mut state).await;
                status_tx.send_if_modified(|current| {
                    if *current == state { false } else { *current = state.clone(); true }
                });
            }
        }
    };
    shutdown_tx.send_replace(true);
    // The Java/discovery tasks have no background driver and can be cancelled directly.
    // Give the session driver a chance to dispose of all of its child tasks first.
    if tokio::time::timeout(Duration::from_millis(250), async {
        while services.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        services.abort_all();
        while services.join_next().await.is_some() {}
    }
    plugins.stop(&mut state).await;
    info!(ticks = tick, "Sculith stopped");
    result
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
