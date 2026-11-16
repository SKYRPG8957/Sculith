mod config;
mod content;
use sculith_plugin_host as plugins;
mod protocol;
use sculith_game as game;
use sculith_runtime as world;

use std::{
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
#[command(name = "sculith", version, about = "A Bedrock-native Minecraft server")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the server.
    Run {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Validate configuration and plugin paths without launching plugins.
    Check {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Show protocol references and the precise scope of verified preview support.
    Versions,
    /// List versioned packet identifiers; registration alone is not game support.
    Packets,
    /// Convert explicitly mapped Bedrock PNGs into a Java resource-pack directory.
    Pack {
        #[command(subcommand)]
        command: PackCommand,
    },
}

#[derive(Subcommand)]
enum PackCommand {
    /// Freeze one Bedrock source into inspectable edition archives (Java 26.3).
    Bundle {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        mapping: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
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
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,sculith=info")),
        )
        .with_target(false)
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
        Command::Packets => {
            println!(
                "{}",
                serde_json::to_string_pretty(&protocol::catalog::all())?
            );
            Ok(())
        }
        Command::Pack {
            command:
                PackCommand::Bundle {
                    input,
                    mapping,
                    output,
                },
        } => {
            let bundle = sculith_packs::bundle::Bundle::prepare(&input, &mapping)?;
            bundle.write(&output)?;
            println!("Edition pack bundle written: {}", output.display());
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
    let started = Instant::now();
    info!("Starting Sculith v{}", env!("CARGO_PKG_VERSION"));
    let delivery = if let Some(settings) = &config.content {
        let source = settings.source.clone();
        let mapping = settings.mapping.clone();
        let bundle = tokio::task::spawn_blocking(move || {
            sculith_packs::bundle::Bundle::prepare(&source, &mapping)
        })
        .await
        .context("pack preparation worker failed")??;
        Some(content::Delivery::prepare(settings.bind, bundle).await?)
    } else {
        None
    };
    let content = delivery.as_ref().map(|delivery| delivery.content.clone());
    let region = if let Some(path) = config.world.snapshot_path.clone() {
        tokio::task::spawn_blocking(move || -> Result<sculith_world::Region> {
            match std::fs::symlink_metadata(&path) {
                Ok(_) => Ok(sculith_world::storage::load(&path)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(sculith_world::Region::flat_origin())
                }
                Err(error) => Err(error.into()),
            }
        })
        .await
        .context("snapshot loading worker failed")??
    } else {
        sculith_world::Region::flat_origin()
    };
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
    let nethernet = if config.nethernet_enabled() {
        Some(
            protocol::bedrock::nethernet::prepare(
                config.network.bedrock.bind,
                &config.bedrock.nethernet_identity_path,
            )
            .await?,
        )
    } else {
        None
    };
    let mut state = config.status()?;
    let (world_stop, world_stop_rx) = watch::channel(false);
    let sounds = content
        .as_ref()
        .map(|content| {
            content
                .bundle
                .report
                .converted_sounds
                .iter()
                .map(|entry| entry.java.clone())
                .collect()
        })
        .unwrap_or_default();
    let models = content
        .as_ref()
        .map(|content| {
            content
                .bundle
                .report
                .converted_models
                .iter()
                .map(|entry| entry.java.clone())
                .collect()
        })
        .unwrap_or_default();
    let (world, world_task) = world::with_region_and_content(
        region,
        config.server.max_players.min(64),
        world_stop_rx,
        sounds,
        models,
    )?;
    let mut world_task = tokio::spawn(world_task);
    let mut world_finished = false;
    let mut game_events = world.subscribe_game();
    let mut plugins = plugins::Plugins::load(&config.plugins, &state).await?;
    world.configure_observations(plugins.observation_flags());
    plugins
        .dispatch(
            Event::Started {
                server: state.clone(),
            },
            &mut state,
            &world,
        )
        .await;
    world.seal_commands().await?;
    world.configure_observations(plugins.observation_flags());
    let (status_tx, status_rx) = watch::channel(state.clone());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut services = JoinSet::new();
    if let Some(delivery) = delivery {
        info!(url = %delivery.content.java_url, "Resource packs available");
        services.spawn(delivery.serve(shutdown_rx.clone()));
    }
    if let Some(listener) = java {
        info!(address = %listener.local_addr()?, "Java listening");
        let preview = config.java.sessions.then(|| protocol::java::Preview {
            content: content.clone(),
            world: world.clone(),
            timeout: Duration::from_secs(config.java.login_timeout_secs),
            shutdown: shutdown_rx.clone(),
        });
        services.spawn(protocol::java::serve(
            listener,
            status_rx.clone(),
            preview,
            shutdown_rx.clone(),
        ));
    }
    if let Some(server) = nethernet {
        info!(address = %server.local_addr()?, "NetherNet listening");
        services.spawn(
            server.serve(protocol::bedrock::nethernet::ContextData {
                settings: protocol::bedrock::session::Settings {
                    authentication: authentication
                        .clone()
                        .context("NetherNet requires Bedrock login initialization")?,
                    timeout: Duration::from_secs(config.bedrock.login_timeout_secs),
                    content: content.clone(),
                },
                world: world.clone(),
                status: status_rx.clone(),
                shutdown: shutdown_rx.clone(),
            }),
        );
    }
    if let Some(socket) = bedrock {
        info!(address = %socket.local_addr()?, "Bedrock listening");
        // Discovery GUID only, never an authenticated identity or secret.
        let guid = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
            ^ u64::from(std::process::id());
        match authentication {
            Some(authentication) => {
                services.spawn(protocol::bedrock::serve_sessions(
                    socket,
                    status_rx,
                    guid,
                    protocol::bedrock::session::Settings {
                        authentication,
                        timeout: Duration::from_secs(config.bedrock.login_timeout_secs),
                        content: content.clone(),
                    },
                    shutdown_rx,
                    world.clone(),
                ));
            }
            None => {
                services.spawn(protocol::bedrock::serve(socket, status_rx, guid));
            }
        }
    }
    if (config.network.java.enabled && config.java.sessions && !config.java.require_authentication)
        || (config.network.bedrock.enabled
            && config.bedrock.sessions
            && !config.bedrock.require_authentication)
    {
        warn!("Offline mode enabled; local connections only");
    }
    info!(
        "Server started in {:.2}s. Press Ctrl+C to stop.",
        started.elapsed().as_secs_f64()
    );
    let mut ticker = tokio::time::interval(Duration::from_secs_f64(
        1.0 / f64::from(config.server.tick_rate),
    ));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut tick = 0u64;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut result = loop {
        tokio::select! {
            result = &mut shutdown => { break result; }
            result = &mut world_task => {
                world_finished = true;
                break match result {
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(error.into()),
                    _ => Err(anyhow::anyhow!("world owner stopped unexpectedly")),
                };
            }
            service = services.join_next() => {
                break match service {
                    Some(Ok(Err(error))) => Err(error.context("discovery listener stopped")),
                    Some(Err(error)) => Err(error.into()),
                    _ => Err(anyhow::anyhow!("discovery listener exited unexpectedly")),
                };
            }
            event = game_events.recv() => {
                match event {
                    Ok(event) => {
                        state.online_players = world.online_players();
                        plugins.dispatch_game(&event, &mut state, &world).await;
                        world.configure_observations(plugins.observation_flags());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        warn!(missed, "game hook backlog; disabling subscribers rather than silently losing events");
                        plugins.disable_game_subscribers(&world).await;
                        world.configure_observations(0);
                    }
                    Err(error) => break Err(error.into()),
                }
            }
            _ = ticker.tick() => {
                tick += 1;
                state.online_players = world.online_players();
                plugins.dispatch(Event::Tick { tick }, &mut state, &world).await;
                world.configure_observations(plugins.observation_flags());
                status_tx.send_if_modified(|current| {
                    if *current == state { false } else { *current = state.clone(); true }
                });
            }
        }
    };
    shutdown_tx.send_replace(true);
    // Java drains its owned connections; discovery has no background driver.
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
    // Network sessions are now gone. Capture the final committed state while
    // the owner is still alive; disk encoding and I/O run on a blocking worker.
    if !world_finished {
        if let Some(path) = config.world.snapshot_path {
            let saved = async {
                let snapshot = world.capture().await?;
                tokio::task::spawn_blocking(move || -> Result<()> {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    sculith_world::storage::save_snapshot(&path, &snapshot)?;
                    Ok(())
                })
                .await
                .context("snapshot saving worker failed")?
            }
            .await;
            if let Err(error) = saved {
                result = Err(error.context("final world snapshot was not saved"));
            }
        }
        world_stop.send_replace(true);
        match world_task.await {
            Ok(Err(error)) => result = Err(error),
            Err(error) => result = Err(error.into()),
            _ => {}
        }
    }
    plugins.stop(&mut state, &world).await;
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
    #[cfg(windows)]
    {
        let mut ctrl_break = tokio::signal::windows::ctrl_break()?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = ctrl_break.recv() => {},
        }
    }
    #[cfg(not(any(unix, windows)))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
