use std::{collections::HashSet, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use sculith_core::{ServerStatus, validate_motd};
use sculith_plugin_api::{
    API_VERSION, Action, Event, HostMessage, MAX_ACTIONS, MAX_MESSAGE_BYTES, Metadata,
    PluginMessage, Request, Response,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

use crate::config::PluginConfig;

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
            tracing::info!(plugin = %process.metadata.name, version = %process.metadata.version, "Rust plugin loaded");
            processes.push(process);
        }
        Ok(Self { processes })
    }

    pub async fn dispatch(&mut self, event: Event, status: &mut ServerStatus) {
        for process in &mut self.processes {
            if process.disabled || !process.metadata.hooks.contains(&event.hook()) {
                continue;
            }
            let result = async {
                let reply = process
                    .request(HostMessage::Event {
                        event: event.clone(),
                    })
                    .await?;
                let PluginMessage::Actions { actions } = reply else {
                    bail!("expected plugin actions");
                };
                validate_actions(&actions)?;
                for action in actions {
                    match action {
                        Action::SetMotd { motd } => {
                            status.set_motd(motd).map_err(anyhow::Error::msg)?
                        }
                        Action::Log { message } => {
                            tracing::info!(plugin = %process.metadata.name, %message)
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

    pub async fn stop(&mut self, status: &mut ServerStatus) {
        self.dispatch(Event::Stopping, status).await;
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
        };
        let reply = process
            .request(HostMessage::Hello {
                api_version: API_VERSION,
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
            api_version == API_VERSION,
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
        ensure!(metadata.hooks.len() <= 3, "too many plugin subscriptions");
        for (index, hook) in metadata.hooks.iter().enumerate() {
            ensure!(
                !metadata.hooks[..index].contains(hook),
                "duplicate plugin subscription"
            );
        }
        process.metadata = metadata;
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
        }
    }
    Ok(())
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
