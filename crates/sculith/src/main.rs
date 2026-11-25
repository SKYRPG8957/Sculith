#![deny(clippy::unwrap_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
// Don't warn on event sending macros
#![recursion_limit = "512"]

#[cfg(target_os = "wasi")]
compile_error!("Compiling for WASI targets is not supported!");

use pumpkin_data::packet::CURRENT_MC_VERSION;
use pumpkin_world::{
    CURRENT_BEDROCK_MC_PROTOCOL, CURRENT_BEDROCK_MC_VERSION,
    chunk::io::bedrock_import::{
        GeneratorPolicy, ImportOptions, ImportStats, export_storage, import_world, inspect_world,
    },
};
use std::{
    backtrace::{Backtrace, BacktraceStatus},
    ffi::{OsStr, OsString},
    io::{self},
    panic::PanicHookInfo,
    path::Path,
    process::exit,
    sync::{OnceLock, atomic::Ordering},
    thread::{self, ThreadId},
};
#[cfg(not(unix))]
use tokio::signal::ctrl_c;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

use sculith_core::{
    CRASH_REPORT, SERVER_EXIT_CODE, SERVER_IS_STOPPING,
    crash::{CrashReport, FullBacktrace},
    data::VanillaData,
    stop_or_exit_server,
};
use sculith_core::{SculithServer, stop_server};

use pumpkin_wasm_host::WasmPluginLoader;
use sculith_config::{LoadConfiguration, SculithConfig};
use sculith_core::plugin::loader::PluginLoader;
use sculith_util::text::{
    TextComponent,
    color::{Color, NamedColor},
};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

mod properties;

const CARGO_PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

static MAIN_THREAD: OnceLock<ThreadId> = OnceLock::new();

// WARNING: All rayon calls from the tokio runtime must be non-blocking! This includes things
// like `par_iter`. These should be spawned in the the rayon pool and then passed to the tokio
// runtime with a channel! See `Level::fetch_chunks` as an example!
#[allow(clippy::too_many_lines)]
#[expect(
    clippy::print_stderr,
    reason = "configuration errors are reported before logging starts"
)]
#[tokio::main]
async fn main() {
    let _ = MAIN_THREAD.set(thread::current().id());

    match std::env::args_os()
        .nth(1)
        .as_deref()
        .and_then(OsStr::to_str)
    {
        Some("--import-bedrock-terrain") => {
            exit(import_bedrock_command(std::env::args_os().skip(2)))
        }
        Some("--inspect-bedrock-world") => {
            exit(inspect_bedrock_command(std::env::args_os().skip(2)))
        }
        Some("--export-bedrock-storage") => {
            exit(export_bedrock_command(std::env::args_os().skip(2)))
        }
        _ => {}
    }

    let exec_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let offline = std::env::args_os().skip(1).any(|arg| arg == "--offline");
    let check = std::env::args_os().nth(1).is_some_and(|arg| arg == "check")
        || std::env::args_os().skip(1).any(|arg| arg == "--check");
    let initialize_properties = std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--init-server-properties");
    if check || initialize_properties || has_init_settings_arg(std::env::args().skip(1)) {
        let mode = if initialize_properties {
            properties::LoadMode::InitializeProperties
        } else {
            properties::LoadMode::Check
        };
        exit(check_settings(&exec_dir, offline, mode));
    }

    // reqwest is built with `rustls-no-provider`, so pick the ring provider (the one
    // wasmtime-wasi-http/rtc already force) before any client can be constructed.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Initialize global Rayon thread pool with named worker threads
    let _ = rayon::ThreadPoolBuilder::new()
        .thread_name(|i| format!("Rayon-Worker-{i}"))
        .build_global();

    // Set the panic handler.
    std::panic::set_hook(Box::new(handle_panic));

    #[cfg(feature = "console-subscriber")]
    console_subscriber::init();
    let time = Instant::now();

    let conversion = parse_convert_world_arg(std::env::args().skip(1));
    let mode = if conversion.is_some() {
        properties::LoadMode::Check
    } else {
        properties::LoadMode::Run
    };
    let mut config = match properties::load(&exec_dir, mode) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Sculith settings load failed: {error}");
            exit(1);
        }
    };
    if offline {
        configure_local_session(&mut config);
    }

    let vanilla_data = VanillaData::load();

    sculith_core::init_logger(&config.advanced);

    let world_dir = exec_dir.join(config.basic.get_world_path());
    if world_dir.join("db/CURRENT").is_file() {
        tracing::error!(
            "This is a Bedrock LevelDB world. Use --import-bedrock-terrain SOURCE DESTINATION before opening it in Sculith: {}",
            world_dir.display()
        );
        exit(1);
    }

    if let Some(target_arg) = conversion {
        let anvil_default = match &config.advanced.world.chunk {
            sculith_config::chunk::ChunkConfig::Anvil(cfg) => cfg.clone(),
            _ => sculith_config::chunk::AnvilChunkConfig::default(),
        };
        let Some(target_config) =
            pumpkin_world::chunk::io::converter::parse_chunk_config(&target_arg, anvil_default)
        else {
            tracing::error!(
                "Unknown chunk format '{target_arg}'. Expected 'pump', 'anvil', or 'linear'."
            );
            exit(1);
        };
        let target_format =
            pumpkin_world::chunk::io::converter::RegionFormat::from_config(&target_config);
        let world_dir = exec_dir.join(config.basic.get_world_path());
        info!(
            "Converting world at {} to {} format...",
            world_dir.display(),
            target_format.name()
        );
        match pumpkin_world::chunk::io::converter::convert_world(&world_dir, &target_config, true) {
            Ok(stats) => {
                info!(
                    "Converted {} region(s) ({} chunk(s)) to {} format in {}ms",
                    stats.regions_converted,
                    stats.chunks_converted,
                    target_format.name(),
                    time.elapsed().as_millis()
                );
                exit(0);
            }
            Err(err) => {
                tracing::error!("World conversion failed: {err}");
                exit(1);
            }
        }
    }

    info!(
        "Starting Sculith {CARGO_PKG_VERSION} | Bedrock {CURRENT_BEDROCK_MC_VERSION} (protocol {CURRENT_BEDROCK_MC_PROTOCOL})"
    );
    if config.advanced.networking.java.enabled {
        info!(
            "Java compatibility enabled (protocol {})",
            CURRENT_MC_VERSION.protocol_version()
        );
    }

    debug!(
        "Build info: FAMILY: \"{}\", OS: \"{}\", ARCH: \"{}\", BUILD: \"{}\"",
        std::env::consts::FAMILY,
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(debug_assertions) {
            "Debug"
        } else {
            "Release"
        }
    );
    if cfg!(debug_assertions) {
        warn!("Debug build; use a release build for performance testing.");
    }

    tokio::spawn(async {
        if let Err(err) = setup_sighandler().await {
            tracing::error!("Unable to setup signal handlers: {err}");
        }
    });

    let plugin_loaders: Vec<Arc<dyn PluginLoader>> = vec![Arc::new(WasmPluginLoader::new(
        config.advanced.plugins.verify_signatures,
    ))];

    let sculith_server = SculithServer::new(
        config.basic,
        config.advanced,
        config.telemetry,
        vanilla_data,
        plugin_loaders,
        true,
    )
    .await
    .unwrap_or_else(|error| {
        tracing::error!("{error}");
        exit(1);
    });
    let plugin_wait_time = sculith_server.init_plugins().await;

    let time_elapsed = time.elapsed().saturating_sub(plugin_wait_time);

    info!(
        "Started server; took {}",
        TextComponent::text(format!("{}ms", time_elapsed.as_millis()))
            .color_named(NamedColor::Gold)
            .to_pretty_console()
    );
    let advanced_config = &sculith_server.server.advanced_config;
    info!(
        "Server is now running. Connect using port: {}{}{}",
        if advanced_config.networking.java.enabled {
            format!(
                "{} {}",
                TextComponent::text("Java Edition:")
                    .color_named(NamedColor::Yellow)
                    .to_pretty_console(),
                TextComponent::text(format!("{}", advanced_config.networking.java.address))
                    .color_named(NamedColor::DarkBlue)
                    .to_pretty_console()
            )
        } else {
            TextComponent::text(String::new()).to_pretty_console()
        },
        if advanced_config.networking.java.enabled && advanced_config.networking.bedrock.enabled {
            " | " // Separator if both are enabled
        } else {
            ""
        },
        if advanced_config.networking.bedrock.enabled {
            format!(
                "{} {}",
                TextComponent::text("Bedrock Edition:")
                    .color_named(NamedColor::Gold)
                    .to_pretty_console(),
                TextComponent::text(format!(
                    "{}",
                    advanced_config.networking.bedrock.nethernet.address
                ))
                .color_named(NamedColor::DarkBlue)
                .to_pretty_console()
            )
        } else {
            TextComponent::text(String::new()).to_pretty_console()
        }
    );

    let distant_terrain =
        sculith_core::net::bedrock::distant_terrain::DistantTerrain::start(&sculith_server.server)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "Could not start distant terrain");
                None
            });
    sculith_server.start().await;
    if let Some(worker) = distant_terrain {
        worker.stop();
    }

    info!(
        "{}",
        TextComponent::text("The server has stopped.")
            .color_named(NamedColor::Red)
            .to_pretty_console()
    );

    exit(SERVER_EXIT_CODE.load(Ordering::Acquire));
}
fn handle_interrupt() {
    warn!(
        "{}",
        TextComponent::text("Received interrupt signal; stopping server...")
            .color_named(NamedColor::Red)
            .to_pretty_console()
    );
    stop_or_exit_server();
}

fn handle_panic(panic_info: &PanicHookInfo<'_>) {
    // Generate a crash report.
    let crash_report = {
        // We capture the backtraces here, and not in the
        // crash report, so that the backtrace doesn't show
        // the CrashReport's `new` function.
        let captured_backtrace = Backtrace::capture();
        let full_backtrace = if captured_backtrace.status() == BacktraceStatus::Captured {
            FullBacktrace::Captured
        } else {
            FullBacktrace::ForceCaptured(Backtrace::force_capture())
        };

        CrashReport::new(panic_info, captured_backtrace, full_backtrace)
    };

    let payload = panic_info.payload();

    if is_main_thread() {
        // It's the first panic;
        // We cannot gracefully shut down as the main thread
        // has panicked. However, we can still generate the crash report.

        if let Some(crash_report) = try_set_crash_report(crash_report) {
            crash_report.print_to_console();
            crash_report.save_and_log();

            tracing::error!(
                "{}",
                TextComponent::text("Aborting due to the main thread panicking.")
                    .color(Color::Named(NamedColor::Red))
                    .to_pretty_console()
            );
        } else {
            // It's a subsequent panic.
            tracing::error!(
                "{}: {}",
                TextComponent::text(
                    "The main thread panicked while stopping the server; aborting."
                )
                .color(Color::Named(NamedColor::Red))
                .bold()
                .to_pretty_console(),
                payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<unknown>")
            );
        }

        exit(1);
    }

    if try_set_crash_report(crash_report).is_some() {
        // It's the first panic; let's stop the server.
        stop_server();
    } else {
        // It's a subsequent panic; let's just alert about it.
        tracing::error!(
            "{}: {}",
            TextComponent::text("Encountered panic while shutting down")
                .color(Color::Named(NamedColor::Red))
                .bold()
                .to_pretty_console(),
            payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<unknown>")
        );
    }
}

fn is_main_thread() -> bool {
    Some(&thread::current().id()) == MAIN_THREAD.get()
}

/// Returns `Some` if the crash report was successfully set. That
/// means it is the first panic, and it must be logged and saved later.
///
/// Returns `None` otherwise as the panic is subsequent.
fn try_set_crash_report(crash_report: CrashReport) -> Option<&'static CrashReport> {
    if !SERVER_IS_STOPPING.load(Ordering::Acquire) && CRASH_REPORT.set(crash_report).is_ok() {
        CRASH_REPORT.get()
    } else {
        None
    }
}

// Non-UNIX Ctrl-C handling
#[cfg(not(unix))]
async fn setup_sighandler() -> io::Result<()> {
    if ctrl_c().await.is_ok() {
        handle_interrupt();
    }

    Ok(())
}

// Unix signal handling
#[cfg(unix)]
async fn setup_sighandler() -> io::Result<()> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut terminate = signal(SignalKind::terminate())?;

    let received = tokio::select! {
        received = interrupt.recv() => received,
        received = hangup.recv() => received,
        received = terminate.recv() => received,
    };

    if received.is_some() {
        handle_interrupt();
    }

    Ok(())
}

fn has_init_settings_arg(mut args: impl Iterator<Item = String>) -> bool {
    args.any(|arg| arg == "--init-settings" || arg == "--initSettings")
}

fn configure_local_session(config: &mut SculithConfig) {
    let net = &mut config.advanced.networking;
    let local = std::net::Ipv4Addr::LOCALHOST.into();
    net.bedrock.nethernet.address.set_ip(local);
    net.bedrock.nethernet.external_ip = Some(local);
    net.bedrock.nethernet.stun_servers.clear();
    net.bedrock.online_mode = false;
    net.bedrock.authentication.enabled = false;
    net.java.address.set_ip(local);
    net.java.online_mode = false;
    net.java.authentication.enabled = false;
    net.lan_broadcast.enabled = false;
    net.management.enabled = false;
    net.query.enabled = false;
    net.proxy.enabled = false;
}

#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "launcher output before server startup"
)]
fn check_settings(dir: &Path, offline: bool, mode: properties::LoadMode) -> i32 {
    let mut config = match properties::load(dir, mode) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("Sculith settings check failed: {err}");
            return 1;
        }
    };
    if offline {
        configure_local_session(&mut config);
    }
    let bedrock = &config.advanced.networking.bedrock;
    println!(
        "Settings: {}",
        dir.join(SculithConfig::get_path()).display()
    );
    let basic_path = dir.join(properties::FILE_NAME);
    if basic_path.is_file() {
        println!("Basic settings: {}", basic_path.display());
    }
    println!(
        "Bedrock enabled={} address={} online_mode={}",
        bedrock.enabled && bedrock.nethernet.enabled,
        bedrock.nethernet.address,
        bedrock.online_mode
    );
    let java = &config.advanced.networking.java;
    println!(
        "Java enabled={} address={} online_mode={}",
        java.enabled, java.address, java.online_mode
    );
    println!(
        "World folder={:?} level-type={}",
        config.basic.default_level_name,
        properties::level_type(config.advanced.world.generator)
    );
    0
}

#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the import command runs before server logging is initialized"
)]
fn import_bedrock_command(mut args: impl Iterator<Item = OsString>) -> i32 {
    let (Some(source), Some(destination), Some((generator, player_map))) =
        (args.next(), args.next(), bedrock_import_flags(args))
    else {
        eprintln!(
            "Run sculith --import-bedrock-terrain SOURCE DESTINATION [--new-chunks=normal] [--player-map FILE]"
        );
        return 2;
    };
    let options = ImportOptions {
        generator,
        player_map: player_map.as_deref().map(Path::new),
    };
    match import_world(Path::new(&source), Path::new(&destination), options) {
        Ok(stats) => {
            println!(
                "Imported {} Overworld chunks in {} regions to {}.",
                stats.chunks,
                stats.regions,
                Path::new(&destination).display()
            );
            print_bedrock_import_stats(&stats, "Imported");
            0
        }
        Err(error) => {
            eprintln!("Bedrock terrain import failed: {error}");
            1
        }
    }
}

#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the inspection command runs before server logging is initialized"
)]
fn inspect_bedrock_command(mut args: impl Iterator<Item = OsString>) -> i32 {
    let (Some(source), Some((generator, player_map))) = (args.next(), bedrock_import_flags(args))
    else {
        eprintln!(
            "Run sculith --inspect-bedrock-world SOURCE [--new-chunks=normal] [--player-map FILE]"
        );
        return 2;
    };
    let options = ImportOptions {
        generator,
        player_map: player_map.as_deref().map(Path::new),
    };
    let report = match inspect_world(Path::new(&source), options) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("Bedrock world inspection failed ({error}).");
            return 1;
        }
    };
    println!(
        "Read {} Overworld chunks in {} regions.",
        report.stats.chunks, report.stats.regions
    );
    print_bedrock_import_stats(&report.stats, "Read");
    if report.blockers == 0 {
        println!("Bedrock import check passed.");
        return 0;
    }
    eprintln!("Bedrock import blocked by {} issues.", report.blockers);
    for issue in &report.blocker_examples {
        eprintln!("{issue}");
    }
    if report.blockers > report.blocker_examples.len() {
        eprintln!(
            "{} further issues.",
            report.blockers - report.blocker_examples.len()
        );
    }
    1
}

fn bedrock_import_flags(
    mut args: impl Iterator<Item = OsString>,
) -> Option<(GeneratorPolicy, Option<OsString>)> {
    let mut generator = GeneratorPolicy::RequireSource;
    let mut player_map = None;
    while let Some(arg) = args.next() {
        if arg == "--new-chunks=normal" && matches!(generator, GeneratorPolicy::RequireSource) {
            generator = GeneratorPolicy::NormalIfUnspecified;
        } else if arg == "--player-map" && player_map.is_none() {
            player_map = Some(args.next()?);
        } else {
            return None;
        }
    }
    Some((generator, player_map))
}

#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the export command runs before server logging is initialized"
)]
fn export_bedrock_command(mut args: impl Iterator<Item = OsString>) -> i32 {
    let (Some(template), Some(shared), Some(destination), None) =
        (args.next(), args.next(), args.next(), args.next())
    else {
        eprintln!(
            "Run sculith --export-bedrock-storage BEDROCK_TEMPLATE SCULITH_WORLD DESTINATION"
        );
        return 2;
    };
    let destination = Path::new(&destination);
    match export_storage(Path::new(&template), Path::new(&shared), destination) {
        Ok(stats) => {
            println!(
                "Updated {} of {} supported storage inventories in {}.",
                stats.inventories_updated,
                stats.containers_checked,
                destination.display()
            );
            print_block_entity_examples(&stats.preserved, "Preserved");
            0
        }
        Err(error) => {
            eprintln!("Bedrock storage export failed ({error}).");
            1
        }
    }
}

#[expect(
    clippy::print_stdout,
    reason = "the CLI prints migration results before logging starts"
)]
fn print_bedrock_import_stats(stats: &ImportStats, verb: &str) {
    println!("{verb} {} storage containers.", stats.containers_imported);
    println!("{verb} {} livestock actors.", stats.entities_imported);
    if stats.players_imported > 0 {
        println!("{verb} {} selected players.", stats.players_imported);
    }
    if !stats.player_source_examples.is_empty() {
        println!(
            "Available Bedrock player keys include {}.",
            stats.player_source_examples.join(", ")
        );
    }
    println!(
        "Excluded data in {} block-entity records, {} entity records and {} other LevelDB records.",
        stats.block_entity_records_excluded,
        stats.entity_records_excluded,
        stats.other_records_excluded
    );
    if stats.default_generator_selected {
        println!(
            "New chunks use Sculith's normal generator because the source layers are unspecified."
        );
    }
    print_block_entity_examples(stats, "Skipped");
    for skipped in &stats.skipped_actor_examples {
        println!(
            "Skipped Bedrock actor {:?} in chunk {}, {} ({}).",
            skipped.kind, skipped.chunk.x, skipped.chunk.y, skipped.reason
        );
    }
}

#[expect(
    clippy::print_stdout,
    reason = "the CLI prints migration results before logging starts"
)]
fn print_block_entity_examples(stats: &ImportStats, verb: &str) {
    for skipped in &stats.skipped_block_entity_examples {
        let location = skipped
            .position
            .map_or_else(|| "unknown position".to_owned(), |pos| pos.to_string());
        println!(
            "{verb} Bedrock {:?} at {location} ({}).",
            skipped.kind, skipped.reason
        );
    }
    if stats.block_entities_skipped > stats.skipped_block_entity_examples.len() {
        println!(
            "{} further block entities in this category.",
            stats.block_entities_skipped - stats.skipped_block_entity_examples.len()
        );
    }
}

fn parse_convert_world_arg(mut args: impl Iterator<Item = String>) -> Option<String> {
    while let Some(arg) = args.next() {
        if arg == "--convert-world" {
            return Some(args.next().unwrap_or_default());
        }
        if let Some(value) = arg.strip_prefix("--convert-world=") {
            return Some(value.to_string());
        }
    }
    None
}
