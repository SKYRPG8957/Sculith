//! Integration-only command source, recipient and legacy compatibility probe.
use sculith_plugin_api::{Action, CommandSpec, Event, Hook, Metadata, Plugin, serve};
struct Commands {
    mode: String,
}
impl Commands {
    fn command(&self) -> String {
        format!("{}_probe", self.mode)
    }
}
impl Plugin for Commands {
    fn metadata(&self) -> Metadata {
        let mut hooks = vec![Hook::Started, Hook::Stopping, Hook::WorldResponse];
        if self.mode != "legacy" {
            hooks.push(Hook::ConsoleCommand);
        }
        if matches!(self.mode.as_str(), "legacy" | "dual" | "observer") {
            hooks.push(Hook::CommandSubmitted);
        }
        Metadata {
            name: format!("console-{}", self.mode),
            version: "1".into(),
            hooks,
        }
    }
    fn on_event(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Started { .. } => vec![Action::RegisterCommand {
                command: CommandSpec {
                    name: self.command(),
                    description: "command source probe".into(),
                },
            }],
            Event::ConsoleCommand { command, arguments } => {
                assert!(
                    self.mode != "legacy" && command == self.command(),
                    "foreign console event"
                );
                if self.mode == "slow" {
                    std::thread::sleep(std::time::Duration::from_millis(2000));
                }
                if self.mode == "console" && arguments == "edit" {
                    use sculith_plugin_api::{WorldBlock, WorldOperation, WorldPosition};
                    return vec![Action::World {
                        request_id: 7,
                        operation: WorldOperation::Edit {
                            region_id: 0,
                            expected_revision: 0,
                            blocks: vec![WorldBlock {
                                position: WorldPosition { x: 1, y: 4, z: 1 },
                                block: "minecraft:stone".into(),
                            }],
                        },
                    }];
                }
                vec![Action::Log {
                    message: format!(
                        "console-reply {} {}",
                        self.mode,
                        serde_json::to_string(&arguments).unwrap()
                    ),
                }]
            }
            Event::WorldResponse { request_id, result } => {
                assert!(
                    self.mode == "console" && request_id == 7,
                    "foreign world reply"
                );
                vec![Action::Log {
                    message: format!("console-world {}", serde_json::to_string(&result).unwrap()),
                }]
            }
            Event::CommandSubmitted {
                player_id,
                command,
                arguments,
                ..
            } => {
                assert!(
                    player_id > 0 && command == self.command(),
                    "foreign player command"
                );
                vec![
                    Action::Log {
                        message: format!(
                            "player-reply {} {}",
                            self.mode,
                            serde_json::to_string(&arguments).unwrap()
                        ),
                    },
                    Action::SendMessage {
                        player_id,
                        message: "command acknowledged".into(),
                    },
                ]
            }
            _ => vec![],
        }
    }
}
fn legacy(mut plugin: Commands) -> std::io::Result<()> {
    use sculith_plugin_api::{HostMessage, MAX_MESSAGE_BYTES, PluginMessage, Request, Response};
    use std::io::{BufRead, Read, Write};
    let input = std::io::stdin();
    let mut input = input.lock();
    let output = std::io::stdout();
    let mut output = output.lock();
    loop {
        let mut bytes = Vec::new();
        let length = (&mut input)
            .take(MAX_MESSAGE_BYTES as u64)
            .read_until(b'\n', &mut bytes)?;
        if length == 0 {
            return Ok(());
        }
        if !bytes.ends_with(b"\n") {
            return Err(std::io::Error::other("oversized host frame"));
        }
        let request: Request = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
        let message = match request.message {
            HostMessage::Hello { .. } => PluginMessage::Ready {
                api_version: 11,
                metadata: plugin.metadata(),
            },
            HostMessage::Event { event } => PluginMessage::Actions {
                actions: plugin.on_event(event),
            },
        };
        serde_json::to_writer(
            &mut output,
            &Response {
                id: request.id,
                message,
            },
        )
        .map_err(std::io::Error::other)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}
fn main() -> std::io::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or("console".into());
    if mode == "legacy" {
        legacy(Commands { mode })
    } else {
        serve(Commands { mode })
    }
}
