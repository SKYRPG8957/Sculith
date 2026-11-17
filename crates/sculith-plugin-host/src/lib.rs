use std::{
    collections::HashSet,
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use sculith_core::{ServerStatus, validate_motd, validate_system_message};
use sculith_plugin_api::{
    API_VERSION, Action, Event, HostMessage, LEGACY_API_VERSION, MAX_ACTIONS, Metadata,
    PluginMessage, Response, SUPPORTED_API_VERSIONS,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

pub mod profiling;
use profiling::{Collector, Mode, Sample};
mod config;
pub use config::PluginConfig;
use sculith_runtime::{WorldEvent, WorldHandle};

mod framing;
use framing::{Buffer, EventMessage};

pub const MAX_PLUGINS: usize = 32;

pub struct Plugins {
    processes: Vec<Process>,
    dispatch_metrics: Collector,
    event_buffer: Buffer,
}

struct Process {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    metadata: Metadata,
    request_id: u64,
    timeout: Duration,
    disabled: bool,
    api_version: u32,
    inventory_tokens: f64,
    inventory_clock: Instant,
    world_budget: Option<(f64, Instant)>,
    metrics: Collector,
    request_buffer: Buffer,
    reply_buffer: Buffer,
}

impl Plugins {
    /// Minimal counters remain active in Off; clocks and history are opt-in.
    pub fn monitor(&mut self, enabled: bool) {
        let now = Instant::now();
        let mode = if enabled { Mode::Monitor } else { Mode::Off };
        self.dispatch_metrics.configure(mode, now, None);
        for process in &mut self.processes {
            process.metrics.configure(mode, now, None);
        }
    }

    pub fn start_profile(&mut self, duration: Duration) -> Result<()> {
        ensure!(
            (Duration::from_secs(1)..=Duration::from_secs(300)).contains(&duration),
            "plugin profile duration must be 1..300 seconds"
        );
        let now = Instant::now();
        let end = Some(now + duration);
        self.dispatch_metrics.configure(Mode::Profile, now, end);
        for process in &mut self.processes {
            process.metrics.configure(Mode::Profile, now, end);
        }
        Ok(())
    }

    pub fn profile_snapshot(&self) -> profiling::Snapshot {
        let now = Instant::now();
        profiling::Snapshot {
            schema_version: 1,
            scope: "plugin_ipc_wall_time",
            dispatcher: self.dispatch_metrics.snapshot(now),
            shared_event_buffer_capacity: self.event_buffer.capacity(),
            plugins: self
                .processes
                .iter()
                .map(|process| profiling::PluginProfile {
                    name: process.metadata.name.clone(),
                    api_version: process.api_version,
                    worker_pid: process.child.id(),
                    disabled: process.disabled,
                    request_buffer_capacity: process.request_buffer.capacity(),
                    reply_buffer_capacity: process.reply_buffer.capacity(),
                    ipc: process.metrics.snapshot(now),
                })
                .collect(),
        }
    }

    pub fn observes_connections(&self) -> bool {
        self.processes.iter().any(|p| {
            !p.disabled
                && p.metadata
                    .hooks
                    .contains(&sculith_plugin_api::Hook::Connection)
        })
    }

    pub fn observes_outgoing(&self) -> bool {
        self.processes.iter().any(|p| {
            !p.disabled
                && p.metadata
                    .hooks
                    .contains(&sculith_plugin_api::Hook::PacketSent)
        })
    }

    pub async fn disable_connection_subscribers(&mut self, world: &WorldHandle) {
        for process in &mut self.processes {
            if !process.disabled
                && process
                    .metadata
                    .hooks
                    .contains(&sculith_plugin_api::Hook::Connection)
            {
                tracing::warn!(plugin = %process.metadata.name, "plugin disabled after missing connection events");
                process.disabled = true;
                let _ = world.disable_commands(process.metadata.name.clone()).await;
                let _ = process.child.kill().await;
                process.input.take();
            }
        }
    }

    pub fn observation_flags(&self) -> u8 {
        use sculith_plugin_api::Hook;
        use sculith_runtime::observations;
        self.processes
            .iter()
            .filter(|p| !p.disabled)
            .flat_map(|p| &p.metadata.hooks)
            .fold(0, |flags, hook| {
                flags
                    | match hook {
                        Hook::PacketReceived => observations::PACKETS,
                        Hook::PlayerMoved => observations::MOVEMENT,
                        Hook::PlayerEquipped => observations::EQUIPMENT,
                        Hook::PlayerInventoryChanged => observations::INVENTORY,
                        _ => 0,
                    }
            })
    }
    pub async fn load(configs: &[PluginConfig], status: &ServerStatus) -> Result<Self> {
        ensure!(
            configs.len() <= MAX_PLUGINS,
            "at most 32 plugins may be configured"
        );
        for config in configs {
            config.validate()?;
        }
        let mut processes = Vec::new();
        let mut names = HashSet::new();
        for config in configs {
            let process = Process::start(config, status)
                .await
                .with_context(|| format!("could not load plugin {}", config.path.display()))?;
            ensure!(
                names.insert(process.metadata.name.clone()),
                "duplicate plugin name: {}",
                process.metadata.name
            );
            tracing::info!(plugin = %process.metadata.name, version = %process.metadata.version, api_version = process.api_version, "Rust plugin loaded");
            processes.push(process);
        }
        Ok(Self {
            processes,
            dispatch_metrics: Collector::default(),
            event_buffer: Buffer::default(),
        })
    }

    pub async fn dispatch(&mut self, event: Event, status: &mut ServerStatus, world: &WorldHandle) {
        self.dispatch_for(event, None, status, world).await;
    }

    /// Local operator input is delivered only to its registered console subscriber.
    pub async fn dispatch_console(
        &mut self,
        input: String,
        status: &mut ServerStatus,
        world: &WorldHandle,
    ) -> Result<Option<String>> {
        match world.route_console(input).await? {
            sculith_game::commands::Route::Feedback(message) => Ok(Some(message)),
            sculith_game::commands::Route::Plugin {
                owner,
                command,
                arguments,
            } => {
                if !self.processes.iter().any(|p| {
                    !p.disabled
                        && p.metadata.name == owner
                        && p.metadata
                            .hooks
                            .contains(&sculith_plugin_api::Hook::ConsoleCommand)
                }) {
                    return Ok(Some("This command is temporarily unavailable.".into()));
                }
                self.dispatch_for(
                    Event::ConsoleCommand { command, arguments },
                    Some(&owner),
                    status,
                    world,
                )
                .await;
                Ok(self
                    .processes
                    .iter()
                    .find(|p| p.metadata.name == owner)
                    .filter(|p| p.disabled)
                    .map(|_| "Command failed; the plugin was disabled.".into()))
            }
        }
    }

    pub async fn dispatch_game(
        &mut self,
        event: &WorldEvent,
        status: &mut ServerStatus,
        world: &WorldHandle,
    ) {
        let recipient = match event {
            WorldEvent::PluginWorldResult { plugin, .. }
            | WorldEvent::PluginInventoryResult { plugin, .. } => Some(plugin.as_str()),
            WorldEvent::FormSubmitted(submission) => Some(submission.plugin.as_str()),
            WorldEvent::CustomFormSubmitted(submission) => Some(submission.plugin.as_str()),
            WorldEvent::CommandSubmitted(submission) => {
                if !self
                    .processes
                    .iter()
                    .any(|p| !p.disabled && p.metadata.name == submission.plugin)
                {
                    let _ = world
                        .send_message(
                            submission.player_id,
                            "This command is temporarily unavailable.".into(),
                        )
                        .await;
                    return;
                }
                Some(submission.plugin.as_str())
            }
            _ => None,
        };
        if self.processes.iter().any(|p| {
            !p.disabled
                && p.metadata
                    .hooks
                    .contains(&sculith_plugin_api::Hook::WorldBlocksChanged)
        }) && let Some(world_event) = world_block_event(event)
        {
            self.dispatch_for(world_event, None, status, world).await;
        }
        if let Some(event) = game_event(event) {
            self.dispatch_for(event, recipient, status, world).await;
        }
    }

    async fn dispatch_for(
        &mut self,
        event: Event,
        recipient: Option<&str>,
        status: &mut ServerStatus,
        world: &WorldHandle,
    ) {
        // Requests wait concurrently; actions still apply in configured order.
        // One slow worker must not multiply the round's delay by plugin count.
        let timing = self.dispatch_metrics.begin();
        let mut dispatch_failed = false;
        let hook = event.hook();
        let mut selected = [false; MAX_PLUGINS];
        let mut selected_count = 0;
        for (process, selected) in self.processes.iter().zip(&mut selected) {
            *selected = !process.disabled
                && process.metadata.hooks.contains(&hook)
                && recipient.is_none_or(|name| process.metadata.name == name);
            selected_count += usize::from(*selected);
        }
        // A private reply selects its owner before any serialization. Public
        // fan-out encodes the body once; envelope IDs remain per worker.
        let shared = selected_count > 1;
        let encode_timing = shared.then(|| self.dispatch_metrics.begin()).flatten();
        let body = shared.then(|| framing::event(&mut self.event_buffer, &event));
        let encode_ns = Collector::elapsed(&encode_timing);
        let prepared = body
            .as_ref()
            .map(|result| result.as_ref().map(|()| self.event_buffer.bytes()));
        let replies = if selected_count == 0 {
            Vec::new()
        } else if selected_count == 1 {
            let index = selected
                .iter()
                .position(|selected| *selected)
                .expect("selected worker");
            let reply = self.processes[index].request_event(&event).await;
            vec![(index, reply)]
        } else {
            futures_util::future::join_all(
                self.processes
                    .iter_mut()
                    .enumerate()
                    .filter(|(index, _)| selected[*index])
                    .map(|(index, process)| {
                        let body = prepared.expect("shared event");
                        let event = &event;
                        async move {
                            (
                                index,
                                process.request_shared_event(event.hook(), body).await,
                            )
                        }
                    }),
            )
            .await
        };
        self.event_buffer.clear();
        for (index, reply) in replies {
            let process = &mut self.processes[index];
            let result = async {
                let PluginMessage::Actions { actions } = reply? else {
                    bail!("expected plugin actions");
                };
                validate_actions(&actions)?;
                ensure!(actions.iter().all(|action| action.minimum_api() <= process.api_version), "action exceeds negotiated plugin API");
                let world_units: usize = actions.iter().filter_map(|a| match a { Action::World { operation, .. } => Some(operation.units()), _ => None }).sum();
                if world_units != 0 {
                    ensure!(process.metadata.hooks.contains(&sculith_plugin_api::Hook::WorldResponse), "world requests require WorldResponse subscription");
                    process.charge_world(world_units, Instant::now())?;
                }
                let inventory_actions=actions.iter().filter(|a|matches!(a,Action::Inventory {..})).count();
                if inventory_actions!=0 {
                    ensure!(process.metadata.hooks.contains(&sculith_plugin_api::Hook::InventoryResponse),"inventory requests require InventoryResponse subscription");
                    process.charge_inventory(inventory_actions,Instant::now())?;
                }
                let registrations:Vec<_>=actions.iter().filter_map(|action|match action {Action::RegisterCommand {command}=>Some(command.clone()),_=>None}).collect();
                if !registrations.is_empty() {
                    let sources = sculith_game::commands::Sources {
                        player: process.metadata.hooks.contains(&sculith_plugin_api::Hook::CommandSubmitted),
                        console: process.metadata.hooks.contains(&sculith_plugin_api::Hook::ConsoleCommand),
                    };
                    ensure!(hook==sculith_plugin_api::Hook::Started && (sources.player || sources.console),"commands require Started registration and a command subscription");
                    world.register_commands_from(process.metadata.name.clone(),registrations,sources).await?;
                }
                for action in actions {
                    match action {
                        Action::World { request_id, operation } => {
                            world.plugin_world(process.metadata.name.clone(), request_id, operation).await?;
                        }
                        Action::Inventory {player_id,request_id,operation}=>{
                            world.plugin_inventory(process.metadata.name.clone(),player_id,request_id,operation).await?;
                        }
                        Action::RegisterCommand {..}=>{},
                        Action::SetMotd { motd } => {
                            status.set_motd(motd).map_err(anyhow::Error::msg)?
                        }
                        Action::Log { message } => {
                            tracing::info!(plugin = %process.metadata.name, %message)
                        }
                        Action::SendMessage { player_id, message } => {
                            if let Err(error) = world.send_message(player_id, message).await {
                                // Disconnects and owner rate limits are ordinary rejections.
                                tracing::warn!(plugin = %process.metadata.name, player_id, %error, "plugin message rejected");
                            }
                        }
                        Action::ShowModel { player_id, model } => {
                            if let Err(error)=world.show_model(player_id,model).await { tracing::warn!(plugin=%process.metadata.name,player_id,%error,"plugin model rejected"); }
                        }
                        Action::HideModel { player_id } => {
                            if let Err(error)=world.hide_model(player_id).await { tracing::warn!(plugin=%process.metadata.name,player_id,%error,"plugin model rejected"); }
                        }
                        Action::PlaySound { player_id, sound, volume, pitch } => {
                            if let Err(error) = world.play_sound(player_id, sound, volume, pitch).await {
                                tracing::warn!(plugin=%process.metadata.name, player_id, %error, "plugin sound rejected");
                            }
                        }
                        Action::ShowCustomForm {player_id,request_id,form}=>{
                            if let Err(error)=world.show_custom_form(player_id,process.metadata.name.clone(),request_id,form).await {
                                tracing::warn!(plugin=%process.metadata.name,player_id,%error,"plugin custom form rejected");
                            }
                        }
                        Action::ShowForm { player_id, request_id, form } => {
                            if let Err(error) = world.show_form(player_id, process.metadata.name.clone(), request_id, form).await {
                                tracing::warn!(plugin=%process.metadata.name, player_id, %error, "plugin form rejected");
                            }
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                dispatch_failed = true;
                tracing::warn!(plugin = %process.metadata.name, %error, "plugin disabled");
                process.disabled = true;
                let _ = world.disable_commands(process.metadata.name.clone()).await;
                let _ = process.child.kill().await;
                process.input.take();
            }
        }
        self.dispatch_metrics.finish(
            timing,
            Sample {
                request_id: None,
                hook: Some(hook),
                start_ns: 0,
                encode_ns,
                message_encodes: u64::from(shared),
                wall_ns: 0,
                sent_bytes: 0,
                received_bytes: 0,
                failed: dispatch_failed,
                timed_out: false,
            },
        );
    }

    pub async fn disable_game_subscribers(&mut self, world: &WorldHandle) {
        for process in &mut self.processes {
            if !process.disabled
                && process.metadata.hooks.iter().any(|hook| {
                    hook.minimum_api() >= 2
                        && !matches!(
                            hook,
                            sculith_plugin_api::Hook::Connection
                                | sculith_plugin_api::Hook::PacketSent
                        )
                })
            {
                tracing::warn!(plugin = %process.metadata.name, "plugin disabled after missing game events");
                process.disabled = true;
                let _ = world.disable_commands(process.metadata.name.clone()).await;
                let _ = process.child.kill().await;
                process.input.take();
            }
        }
    }

    pub async fn stop(&mut self, status: &mut ServerStatus, world: &WorldHandle) {
        self.dispatch(Event::Stopping, status, world).await;
        for process in &mut self.processes {
            process.input.take();
            if tokio::time::timeout(Duration::from_millis(500), process.child.wait())
                .await
                .is_err()
            {
                let _ = process.child.kill().await;
            }
        }
    }
}

impl Process {
    fn charge_world(&mut self, units: usize, now: Instant) -> Result<()> {
        let available = match self.world_budget {
            Some((tokens, updated)) => (tokens
                + now
                    .checked_duration_since(updated)
                    .context("world budget clock moved backwards")?
                    .as_secs_f64()
                    * 1024.0)
                .min(2048.0),
            None => 2048.0,
        };
        ensure!(
            units as f64 <= available,
            "plugin world request limit exceeded"
        );
        self.world_budget = Some((available - units as f64, now));
        Ok(())
    }

    fn charge_inventory(&mut self, count: usize, now: Instant) -> Result<()> {
        let elapsed = now
            .saturating_duration_since(self.inventory_clock)
            .as_secs_f64();
        let available = (self.inventory_tokens + elapsed * 128.0).min(256.0);
        ensure!(
            count as f64 <= available,
            "plugin inventory request limit exceeded"
        );
        self.inventory_tokens = available - count as f64;
        self.inventory_clock = now;
        Ok(())
    }

    async fn start(config: &PluginConfig, status: &ServerStatus) -> Result<Self> {
        config.validate()?;
        let mut command = Command::new(&config.path);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        // Console Ctrl+C/Break belongs to the host. Keep Windows JSON-lines
        // workers alive until they receive the ordered Stopping event and EOF.
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = command.spawn().context("cannot spawn plugin executable")?;
        let input = child.stdin.take().context("missing plugin stdin")?;
        let output = BufReader::new(child.stdout.take().context("missing plugin stdout")?);
        let mut process = Self {
            child,
            input: Some(input),
            output,
            metadata: Metadata {
                name: "loading".into(),
                version: String::new(),
                hooks: vec![],
            },
            request_id: 0,
            timeout: Duration::from_millis(config.timeout_ms),
            disabled: false,
            api_version: LEGACY_API_VERSION,
            inventory_tokens: 256.0,
            inventory_clock: Instant::now(),
            world_budget: None,
            metrics: Collector::default(),
            request_buffer: Buffer::default(),
            reply_buffer: Buffer::default(),
        };
        let reply = process
            .request(HostMessage::Hello {
                api_version: LEGACY_API_VERSION,
                supported_versions: SUPPORTED_API_VERSIONS.to_vec(),
                server: status.clone(),
            })
            .await?;
        let PluginMessage::Ready {
            api_version,
            metadata,
        } = reply
        else {
            bail!("expected plugin ready handshake");
        };
        ensure!(
            SUPPORTED_API_VERSIONS.contains(&api_version),
            "plugin API version {api_version} is incompatible with {API_VERSION}"
        );
        ensure!(
            !metadata.name.is_empty()
                && metadata.name.len() <= 64
                && metadata
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
            "invalid plugin name"
        );
        ensure!(
            !metadata.version.is_empty()
                && metadata.version.len() <= 32
                && !metadata.version.chars().any(char::is_control),
            "invalid plugin version"
        );
        ensure!(
            !metadata
                .hooks
                .contains(&sculith_plugin_api::Hook::PacketSent)
                || metadata
                    .hooks
                    .contains(&sculith_plugin_api::Hook::Connection),
            "PacketSent requires Connection lifecycle subscription"
        );
        ensure!(
            metadata.hooks.len() <= sculith_plugin_api::Hook::ALL.len(),
            "too many plugin subscriptions"
        );
        for (index, hook) in metadata.hooks.iter().enumerate() {
            ensure!(
                hook.minimum_api() <= api_version,
                "hook exceeds negotiated plugin API"
            );
            ensure!(
                !metadata.hooks[..index].contains(hook),
                "duplicate plugin subscription"
            );
        }
        process.metadata = metadata;
        process.api_version = api_version;
        Ok(process)
    }

    async fn request(&mut self, message: HostMessage) -> Result<PluginMessage> {
        let hook = match &message {
            HostMessage::Event { event } => Some(event.hook()),
            HostMessage::Hello { .. } => None,
        };
        self.request_message(&message, hook).await
    }

    async fn request_event(&mut self, event: &Event) -> Result<PluginMessage> {
        self.request_message(&EventMessage::Event { event }, Some(event.hook()))
            .await
    }

    async fn request_message<T: serde::Serialize + Sync>(
        &mut self,
        message: &T,
        hook: Option<sculith_plugin_api::Hook>,
    ) -> Result<PluginMessage> {
        self.request_frame(hook, 1, |id, buffer| framing::request(buffer, id, message))
            .await
    }

    async fn request_shared_event(
        &mut self,
        hook: sculith_plugin_api::Hook,
        body: std::result::Result<&[u8], &anyhow::Error>,
    ) -> Result<PluginMessage> {
        self.request_frame(Some(hook), 0, |id, buffer| {
            let body = body
                .map_err(|error| anyhow::anyhow!("shared plugin event encoding failed: {error}"))?;
            framing::shared_request(buffer, id, body)
        })
        .await
    }

    async fn request_frame(
        &mut self,
        hook: Option<sculith_plugin_api::Hook>,
        encodes: u64,
        encode: impl FnOnce(u64, &mut Buffer) -> Result<()> + Send,
    ) -> Result<PluginMessage> {
        let timing = self.metrics.begin();
        let (mut encode_ns, mut message_encodes, mut sent_bytes, mut received_bytes, mut timed_out) =
            (0, 0, 0, 0, false);
        let result = async {
            self.request_id = self
                .request_id
                .checked_add(1)
                .context("plugin request identifiers exhausted")?;
            message_encodes = encodes;
            let encoded = encode(self.request_id, &mut self.request_buffer);
            encode_ns = Collector::elapsed(&timing);
            encoded?;
            let outcome = tokio::time::timeout(self.timeout, async {
                let input = self.input.as_mut().context("plugin stdin closed")?;
                input.write_all(self.request_buffer.bytes()).await?;
                sent_bytes = self.request_buffer.bytes().len() as u64;
                input.flush().await?;
                read_line_bounded(&mut self.output, &mut self.reply_buffer).await?;
                received_bytes = self.reply_buffer.bytes().len() as u64;
                let response: Response = serde_json::from_slice(self.reply_buffer.bytes())
                    .context("invalid plugin JSON response")?;
                ensure!(
                    response.id == self.request_id,
                    "plugin response ID mismatch"
                );
                Ok(response.message)
            })
            .await;
            timed_out = outcome.is_err();
            outcome.context("plugin response timed out")?
        }
        .await;
        // Responses own their fields. Retain bounded capacity, never stale bytes.
        self.request_buffer.clear();
        self.reply_buffer.clear();
        self.metrics.finish(
            timing,
            Sample {
                request_id: Some(self.request_id),
                hook,
                start_ns: 0,
                encode_ns,
                message_encodes,
                wall_ns: 0,
                sent_bytes,
                received_bytes,
                failed: result.is_err(),
                timed_out,
            },
        );
        result
    }
}

async fn read_line_bounded(
    input: &mut (impl AsyncBufRead + Unpin),
    line: &mut Buffer,
) -> Result<()> {
    line.clear();
    loop {
        let available = input.fill_buf().await?;
        ensure!(
            !available.is_empty(),
            "plugin closed stdout before a complete response"
        );
        let end = available
            .iter()
            .position(|&b| b == b'\n')
            .map(|index| index + 1);
        let count = end.unwrap_or(available.len());
        line.append(&available[..count])
            .context("plugin response exceeds 64 KiB")?;
        input.consume(count);
        if end.is_some() {
            return Ok(());
        }
    }
}

fn validate_actions(actions: &[Action]) -> Result<()> {
    ensure!(actions.len() <= MAX_ACTIONS, "too many plugin actions");
    for action in actions {
        match action {
            Action::World { operation, .. } => operation.validate().map_err(anyhow::Error::msg)?,
            Action::Inventory {
                player_id,
                operation,
                ..
            } => {
                ensure!(*player_id != 0, "invalid inventory recipient");
                operation.validate().map_err(anyhow::Error::msg)?;
            }
            Action::RegisterCommand { command } => {
                command.validate().map_err(anyhow::Error::msg)?
            }
            Action::SetMotd { motd } => validate_motd(motd).map_err(anyhow::Error::msg)?,
            Action::Log { message } => {
                ensure!(
                    message.len() <= 2048 && !message.chars().any(char::is_control),
                    "invalid plugin log message"
                );
            }
            Action::SendMessage { player_id, message } => {
                ensure!(*player_id != 0, "invalid message recipient");
                validate_system_message(message).map_err(anyhow::Error::msg)?;
            }
            Action::ShowModel { player_id, model } => {
                ensure!(*player_id != 0, "invalid model recipient");
                sculith_core::validate_resource_id(model).map_err(anyhow::Error::msg)?;
            }
            Action::HideModel { player_id } => {
                ensure!(*player_id != 0, "invalid model recipient");
            }
            Action::PlaySound {
                player_id,
                sound,
                volume,
                pitch,
            } => {
                ensure!(*player_id != 0, "invalid sound recipient");
                sculith_core::validate_sound_id(sound).map_err(anyhow::Error::msg)?;
                sculith_core::validate_sound_parameters(*volume, *pitch)
                    .map_err(anyhow::Error::msg)?;
            }
            Action::ShowCustomForm {
                player_id, form, ..
            } => {
                ensure!(*player_id != 0, "invalid form recipient");
                form.validate().map_err(anyhow::Error::msg)?;
            }
            Action::ShowForm {
                player_id, form, ..
            } => {
                ensure!(*player_id != 0, "invalid form recipient");
                form.validate().map_err(anyhow::Error::msg)?;
            }
        }
    }
    Ok(())
}

fn world_block_event(event: &WorldEvent) -> Option<Event> {
    use sculith_plugin_api::WorldActor;
    let (batch, actor) = match event {
        WorldEvent::Blocks {
            player_id, batch, ..
        } => (
            batch,
            WorldActor::Player {
                player_id: *player_id,
            },
        ),
        WorldEvent::PluginBlocks { plugin, batch, .. } => (
            batch,
            WorldActor::Plugin {
                name: plugin.clone(),
            },
        ),
        _ => return None,
    };
    Some(Event::WorldBlocksChanged {
        region_id: batch.region.0,
        previous_revision: batch.previous_revision,
        revision: batch.revision,
        actor,
        changes: sculith_game::world_api::describe_changes(batch),
    })
}

pub fn game_event(event: &WorldEvent) -> Option<Event> {
    use sculith_plugin_api::{BlockChange, Edition, PlayerInfo};
    use sculith_world::BlockKind;
    fn name(block: BlockKind) -> String {
        match block {
            BlockKind::Air => "minecraft:air",
            BlockKind::Bedrock => "minecraft:bedrock",
            BlockKind::Stone => "minecraft:stone",
            BlockKind::Dirt => "minecraft:dirt",
            BlockKind::Grass => "minecraft:grass_block",
        }
        .into()
    }
    Some(match event {
        WorldEvent::PluginWorldResult {
            request_id, result, ..
        } => Event::WorldResponse {
            request_id: *request_id,
            result: result.clone(),
        },
        WorldEvent::PluginInventoryResult {
            player_id,
            request_id,
            result,
            ..
        } => Event::InventoryResponse {
            player_id: *player_id,
            request_id: *request_id,
            result: result.clone(),
        },
        WorldEvent::CommandSubmitted(submission) => Event::CommandSubmitted {
            player_id: submission.player_id,
            name: submission.name.clone(),
            command: submission.command.clone(),
            arguments: submission.arguments.clone(),
        },
        WorldEvent::Chat(chat) => Event::PlayerChat {
            player_id: chat.player_id,
            name: chat.name.clone(),
            message: chat.message.clone(),
        },
        WorldEvent::PacketReceived { player_id, packet } => Event::PacketReceived {
            player_id: *player_id,
            packet: packet.clone(),
        },
        WorldEvent::Moved { player, tick } => Event::PlayerMoved {
            player_id: player.id,
            tick: *tick,
            x: player.feet.x,
            y: player.feet.y,
            z: player.feet.z,
            yaw: player.rotation.yaw,
            pitch: player.rotation.pitch,
            head_yaw: player.rotation.head_yaw,
        },
        WorldEvent::InventoryChanged { player_id, changes } => Event::PlayerInventoryChanged {
            player_id: *player_id,
            changes: changes
                .iter()
                .map(|change| {
                    use sculith_game::inventory::Slot;
                    use sculith_plugin_api::{InventoryChange, InventoryContainer};
                    let (container, slot) = match change.slot {
                        Slot::Inventory(slot) => (InventoryContainer::Inventory, slot),
                        Slot::Cursor => (InventoryContainer::Cursor, 0),
                        Slot::CreatedOutput => (InventoryContainer::CreatedOutput, 50),
                    };
                    InventoryChange {
                        container,
                        slot,
                        item: change.stack.map(|stack| name(stack.kind)),
                        count: change.stack.map_or(0, |stack| u16::from(stack.count)),
                    }
                })
                .collect(),
        },
        WorldEvent::Equipped(player) => {
            let selected = player.held();
            Event::PlayerEquipped {
                player_id: player.id,
                slot: player.selected_slot,
                item: selected.map(|stack| name(stack.kind)),
                count: selected.map_or(0, |stack| u16::from(stack.count)),
            }
        }
        WorldEvent::Joined(player) => Event::PlayerJoined {
            player: PlayerInfo {
                id: player.id,
                name: player.profile.name.clone(),
                edition: match player.profile.edition {
                    sculith_game::Edition::Java => Edition::Java,
                    sculith_game::Edition::Bedrock => Edition::Bedrock,
                },
            },
        },
        WorldEvent::Left { id } => Event::PlayerLeft { player_id: *id },
        WorldEvent::CustomFormSubmitted(submission) => Event::CustomFormSubmitted {
            player_id: submission.player_id,
            request_id: submission.request_id,
            values: submission.values.clone(),
        },
        WorldEvent::FormSubmitted(submission) => Event::FormSubmitted {
            player_id: submission.player_id,
            request_id: submission.request_id,
            selection: submission.selection,
        },
        WorldEvent::Blocks {
            player_id, batch, ..
        } => Event::BlocksChanged {
            player_id: *player_id,
            revision: batch.revision,
            changes: batch
                .changes
                .iter()
                .map(|c| BlockChange {
                    x: c.position.x,
                    y: c.position.y,
                    z: c.position.z,
                    before: name(c.before),
                    after: name(c.after),
                })
                .collect(),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
