use std::{collections::HashSet, process::Stdio, time::Duration};

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

use crate::config::PluginConfig;
use crate::world::{WorldEvent, WorldHandle};

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
}

impl Plugins {
    pub async fn load(configs: &[PluginConfig], status: &ServerStatus) -> Result<Self> {
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
        let recipient = if let WorldEvent::FormSubmitted(submission) = event {
            Some(submission.plugin.as_str())
        } else {
            None
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
                for action in actions {
                    match action {
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
                let _ = process.child.kill().await;
                process.input.take();
            }
        }
    }

    pub async fn disable_game_subscribers(&mut self) {
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
    async fn start(config: &PluginConfig, status: &ServerStatus) -> Result<Self> {
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
        ensure!(metadata.hooks.len() <= 7, "too many plugin subscriptions");
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
        WorldEvent::Joined(player) => Event::PlayerJoined {
            player: PlayerInfo {
                id: player.id,
                name: player.profile.name.clone(),
                edition: match player.profile.edition {
                    crate::game::Edition::Java => Edition::Java,
                    crate::game::Edition::Bedrock => Edition::Bedrock,
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
mod tests {
    use super::*;

    #[test]
    fn actions_validate_as_a_batch_before_any_state_mutation() {
        assert!(
            validate_actions(&[
                Action::SetMotd {
                    motd: "valid".into()
                },
                Action::SetMotd {
                    motd: "injected;field".into()
                },
            ])
            .is_err()
        );
        assert!(
            validate_actions(&[Action::Log {
                message: "injected\nline".into()
            }])
            .is_err()
        );
        assert!(
            validate_actions(&vec![
                Action::Log {
                    message: "x".into()
                };
                MAX_ACTIONS + 1
            ])
            .is_err()
        );
    }

    #[tokio::test]
    async fn bounded_plugin_reader_rejects_large_and_incomplete_responses() {
        let data = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        assert!(read_line_bounded(&mut &data[..]).await.is_err());
        assert!(read_line_bounded(&mut &b"partial"[..]).await.is_err());
        assert_eq!(read_line_bounded(&mut &b"{}\n"[..]).await.unwrap(), b"{}\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v1_callbacks_wait_concurrently_but_apply_actions_in_configuration_order() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let script = r#"read -r hello
printf '%s\n' '{"id":1,"message":{"type":"ready","api_version":1,"metadata":{"name":"'"$3"'","version":"1","hooks":["started"]}}}'
read -r event
: > "$1"
while [ ! -e "$2" ]; do sleep 0.01; done
printf '%s\n' '{"id":2,"message":{"type":"actions","actions":[{"type":"set_motd","motd":"'"$3"'"}]}}'
"#;
        let configs = [(&first, &second, "first"), (&second, &first, "second")]
            .into_iter()
            .map(|(own, peer, name)| PluginConfig {
                path: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    script.into(),
                    "fixture".into(),
                    own.display().to_string(),
                    peer.display().to_string(),
                    name.into(),
                ],
                timeout_ms: 2000,
            })
            .collect::<Vec<_>>();
        let mut status = ServerStatus::new("original".into(), 20).unwrap();
        let mut plugins = Plugins::load(&configs, &status).await.unwrap();
        let (shutdown, stop) = tokio::sync::watch::channel(false);
        let (world, task) = crate::world::create(stop);
        let task = tokio::spawn(task);
        plugins
            .dispatch(
                Event::Started {
                    server: status.clone(),
                },
                &mut status,
                &world,
            )
            .await;
        assert!(
            plugins
                .processes
                .iter()
                .all(|p| !p.disabled && p.api_version == 1)
        );
        assert_eq!(status.motd, "second");
        plugins.stop(&mut status, &world).await;
        shutdown.send_replace(true);
        task.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_incompatible_plugin_and_times_out_unresponsive_plugin() {
        let status = ServerStatus::new("test".into(), 20).unwrap();
        let config = PluginConfig {
            path: "/bin/sh".into(),
            args: vec!["-c".into(), "read -r line; printf '%s\\n' '{\"id\":1,\"message\":{\"type\":\"ready\",\"api_version\":999,\"metadata\":{\"name\":\"wrong\",\"version\":\"1\",\"hooks\":[]}}}'".into()],
            timeout_ms: 1000,
        };
        let error = match Process::start(&config, &status).await {
            Ok(_) => panic!("wrong API accepted"),
            Err(e) => e,
        };
        assert!(error.to_string().contains("incompatible"));
        let config = PluginConfig {
            path: "/bin/sh".into(),
            args: vec!["-c".into(), "exec sleep 10".into()],
            timeout_ms: 50,
        };
        let error = match Process::start(&config, &status).await {
            Ok(_) => panic!("stalled plugin accepted"),
            Err(e) => e,
        };
        assert!(error.to_string().contains("timed out"));
    }
}
