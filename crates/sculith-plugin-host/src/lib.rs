use std::{
    collections::HashSet,
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use sculith_core::{ServerStatus, validate_motd, validate_system_message};
use sculith_plugin_api::{
    API_VERSION, Action, Event, HostMessage, LEGACY_API_VERSION, MAX_ACTIONS, MAX_MESSAGE_BYTES,
    Metadata, PluginMessage, Request, Response, SUPPORTED_API_VERSIONS,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

mod config;
pub use config::PluginConfig;
use sculith_runtime::{WorldEvent, WorldHandle};

pub const MAX_PLUGINS: usize = 32;

pub struct Plugins {
    processes: Vec<Process>,
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
}

impl Plugins {
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
        Ok(Self { processes })
    }

    pub async fn dispatch(&mut self, event: Event, status: &mut ServerStatus, world: &WorldHandle) {
        self.dispatch_for(event, None, status, world).await;
    }

    pub async fn dispatch_game(
        &mut self,
        event: &WorldEvent,
        status: &mut ServerStatus,
        world: &WorldHandle,
    ) {
        let recipient = match event {
            WorldEvent::PluginInventoryResult { plugin, .. } => Some(plugin.as_str()),
            WorldEvent::FormSubmitted(submission) => Some(submission.plugin.as_str()),
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
        let hook = event.hook();
        let replies = futures_util::future::join_all(self.processes.iter_mut().map(|process| {
            let subscribed = !process.disabled
                && process.metadata.hooks.contains(&hook)
                && recipient.is_none_or(|name| process.metadata.name == name);
            let event = event.clone();
            async move {
                if subscribed {
                    Some(process.request(HostMessage::Event { event }).await)
                } else {
                    None
                }
            }
        }))
        .await;
        for (process, reply) in self.processes.iter_mut().zip(replies) {
            let Some(reply) = reply else { continue };
            let result = async {
                let PluginMessage::Actions { actions } = reply? else {
                    bail!("expected plugin actions");
                };
                validate_actions(&actions)?;
                ensure!(actions.iter().all(|action| action.minimum_api() <= process.api_version), "action exceeds negotiated plugin API");
                let inventory_actions=actions.iter().filter(|a|matches!(a,Action::Inventory {..})).count();
                if inventory_actions!=0 {
                    ensure!(process.metadata.hooks.contains(&sculith_plugin_api::Hook::InventoryResponse),"inventory requests require InventoryResponse subscription");
                    process.charge_inventory(inventory_actions,Instant::now())?;
                }
                let registrations:Vec<_>=actions.iter().filter_map(|action|match action {Action::RegisterCommand {command}=>Some(command.clone()),_=>None}).collect();
                if !registrations.is_empty() {
                    ensure!(hook==sculith_plugin_api::Hook::Started && process.metadata.hooks.contains(&sculith_plugin_api::Hook::CommandSubmitted),"commands require Started registration and CommandSubmitted subscription");
                    world.register_commands(process.metadata.name.clone(),registrations).await?;
                }
                for action in actions {
                    match action {
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
                tracing::warn!(plugin = %process.metadata.name, %error, "plugin disabled");
                process.disabled = true;
                let _ = world.disable_commands(process.metadata.name.clone()).await;
                let _ = process.child.kill().await;
                process.input.take();
            }
        }
    }

    pub async fn disable_game_subscribers(&mut self, world: &WorldHandle) {
        for process in &mut self.processes {
            if !process.disabled
                && process
                    .metadata
                    .hooks
                    .iter()
                    .any(|hook| hook.minimum_api() >= 2)
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
        self.request_id += 1;
        let request = Request {
            id: self.request_id,
            message,
        };
        let mut data = serde_json::to_vec(&request)?;
        data.push(b'\n');
        ensure!(data.len() <= MAX_MESSAGE_BYTES, "host message too large");
        tokio::time::timeout(self.timeout, async {
            let input = self.input.as_mut().context("plugin stdin closed")?;
            input.write_all(&data).await?;
            input.flush().await?;
            let line = read_line_bounded(&mut self.output).await?;
            let response: Response =
                serde_json::from_slice(&line).context("invalid plugin JSON response")?;
            ensure!(response.id == request.id, "plugin response ID mismatch");
            Ok(response.message)
        })
        .await
        .context("plugin response timed out")?
    }
}

async fn read_line_bounded(input: &mut (impl AsyncBufRead + Unpin)) -> Result<Vec<u8>> {
    let mut line = Vec::new();
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
        ensure!(
            line.len() + count <= MAX_MESSAGE_BYTES,
            "plugin response exceeds 64 KiB"
        );
        line.extend_from_slice(&available[..count]);
        input.consume(count);
        if end.is_some() {
            return Ok(line);
        }
    }
}

fn validate_actions(actions: &[Action]) -> Result<()> {
    ensure!(actions.len() <= MAX_ACTIONS, "too many plugin actions");
    for action in actions {
        match action {
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
