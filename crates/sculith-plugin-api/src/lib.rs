//! Versioned JSON-lines API for trusted Rust plugin executables.
//! stdout is reserved for this protocol; diagnostics belong on stderr.

use std::io::{self, BufRead, Write};

use sculith_core::ServerStatus;
use serde::{Deserialize, Serialize};

pub const API_VERSION: u32 = 2;
pub const LEGACY_API_VERSION: u32 = 1;
pub const GAME_API_VERSION: u32 = 2;
pub const MAX_MESSAGE_BYTES: usize = 65_536;
pub const MAX_ACTIONS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hook {
    Started,
    Tick,
    Stopping,
    PlayerJoined,
    PlayerLeft,
    BlocksChanged,
}

impl Hook {
    pub fn minimum_api(&self) -> u32 {
        match self {
            Self::Started | Self::Tick | Self::Stopping => LEGACY_API_VERSION,
            _ => GAME_API_VERSION,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub name: String,
    pub version: String,
    pub hooks: Vec<Hook>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub message: HostMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    Hello {
        api_version: u32,
        /// Old v1 SDKs ignore this additive field and still select api_version.
        #[serde(default)]
        supported_versions: Vec<u32>,
        server: ServerStatus,
    },
    Event {
        event: Event,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Started {
        server: ServerStatus,
    },
    Tick {
        tick: u64,
    },
    Stopping,
    PlayerJoined {
        player: PlayerInfo,
    },
    /// Runtime disconnect notification; use Stopping for whole-server shutdown.
    PlayerLeft {
        player_id: u64,
    },
    /// Post-commit notification. This event cannot cancel or rewrite a commit.
    BlocksChanged {
        player_id: u64,
        revision: u64,
        changes: Vec<BlockChange>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edition {
    Java,
    Bedrock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerInfo {
    pub id: u64,
    pub name: String,
    pub edition: Edition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockChange {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    /// Edition-independent names, never a wire runtime ID.
    pub before: String,
    pub after: String,
}

impl Event {
    pub fn hook(&self) -> Hook {
        match self {
            Self::Started { .. } => Hook::Started,
            Self::Tick { .. } => Hook::Tick,
            Self::Stopping => Hook::Stopping,
            Self::PlayerJoined { .. } => Hook::PlayerJoined,
            Self::PlayerLeft { .. } => Hook::PlayerLeft,
            Self::BlocksChanged { .. } => Hook::BlocksChanged,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub message: PluginMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginMessage {
    Ready {
        api_version: u32,
        metadata: Metadata,
    },
    Actions {
        actions: Vec<Action>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    SetMotd {
        motd: String,
    },
    Log {
        message: String,
    },
    /// Plain system text to one initialized player; the owner applies a rate limit.
    SendMessage {
        player_id: u64,
        message: String,
    },
}

impl Action {
    pub fn minimum_api(&self) -> u32 {
        match self {
            Self::SetMotd { .. } | Self::Log { .. } => LEGACY_API_VERSION,
            Self::SendMessage { .. } => GAME_API_VERSION,
        }
    }
}

pub trait Plugin {
    fn metadata(&self) -> Metadata;
    fn on_event(&mut self, event: Event) -> Vec<Action>;
}

/// Run a plugin on stdin/stdout until the server closes the pipe.
pub fn serve(plugin: impl Plugin) -> io::Result<()> {
    serve_io(plugin, io::stdin().lock(), io::stdout().lock())
}

fn serve_io(
    mut plugin: impl Plugin,
    mut input: impl BufRead,
    mut output: impl Write,
) -> io::Result<()> {
    let mut negotiated = None;
    while let Some(line) = read_line_bounded(&mut input)? {
        let request: Request = serde_json::from_slice(&line).map_err(io::Error::other)?;
        let message = match request.message {
            HostMessage::Hello {
                api_version,
                supported_versions,
                ..
            } if negotiated.is_none() => {
                let version =
                    if api_version == API_VERSION || supported_versions.contains(&API_VERSION) {
                        API_VERSION
                    } else if api_version == LEGACY_API_VERSION {
                        LEGACY_API_VERSION
                    } else {
                        return Err(io::Error::other("incompatible plugin API version"));
                    };
                let metadata = plugin.metadata();
                if metadata
                    .hooks
                    .iter()
                    .any(|hook| hook.minimum_api() > version)
                {
                    return Err(io::Error::other(
                        "host does not support the plugin's game hooks",
                    ));
                }
                negotiated = Some(version);
                PluginMessage::Ready {
                    api_version: version,
                    metadata,
                }
            }
            HostMessage::Event { event } if negotiated.is_some() => {
                let version = negotiated.expect("guarded above");
                if event.hook().minimum_api() > version {
                    return Err(io::Error::other("event exceeds negotiated API version"));
                }
                let actions = plugin.on_event(event);
                if actions.len() > MAX_ACTIONS {
                    return Err(io::Error::other("too many plugin actions"));
                }
                if actions.iter().any(|action| action.minimum_api() > version) {
                    return Err(io::Error::other("action exceeds negotiated API version"));
                }
                PluginMessage::Actions { actions }
            }
            _ => return Err(io::Error::other("invalid plugin handshake or API version")),
        };
        let response = serde_json::to_vec(&Response {
            id: request.id,
            message,
        })
        .map_err(io::Error::other)?;
        if response.len() + 1 > MAX_MESSAGE_BYTES {
            return Err(io::Error::other("plugin response too large"));
        }
        output.write_all(&response)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}

fn read_line_bounded(input: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::other("unterminated plugin message"))
            };
        }
        let end = available.iter().position(|&b| b == b'\n').map(|i| i + 1);
        let count = end.unwrap_or(available.len());
        if line.len() + count > MAX_MESSAGE_BYTES {
            return Err(io::Error::other("plugin request too large"));
        }
        line.extend_from_slice(&available[..count]);
        input.consume(count);
        if end.is_some() {
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPlugin;
    impl Plugin for TestPlugin {
        fn metadata(&self) -> Metadata {
            Metadata {
                name: "test".into(),
                version: "1".into(),
                hooks: vec![Hook::Started],
            }
        }
        fn on_event(&mut self, _: Event) -> Vec<Action> {
            vec![Action::SetMotd {
                motd: "Hello both editions".into(),
            }]
        }
    }

    #[test]
    fn plugin_handshake_and_event_keep_request_ids() {
        let requests = [
            Request {
                id: 7,
                message: HostMessage::Hello {
                    api_version: API_VERSION,
                    supported_versions: vec![LEGACY_API_VERSION, API_VERSION],
                    server: ServerStatus::new("test".into(), 20).unwrap(),
                },
            },
            Request {
                id: 8,
                message: HostMessage::Event {
                    event: Event::Tick { tick: 1 },
                },
            },
        ];
        let mut input = Vec::new();
        for request in requests {
            serde_json::to_writer(&mut input, &request).unwrap();
            input.push(b'\n');
        }
        let mut output = Vec::new();
        serve_io(TestPlugin, &input[..], &mut output).unwrap();
        let replies: Vec<Response> = output
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(replies[0].id, 7);
        assert!(matches!(
            replies[0].message,
            PluginMessage::Ready {
                api_version: API_VERSION,
                ..
            }
        ));
        assert_eq!(replies[1].id, 8);
        assert!(
            matches!(&replies[1].message, PluginMessage::Actions { actions } if actions.len() == 1)
        );
    }

    #[test]
    fn sdk_rejects_wrong_api_event_before_hello_and_oversize() {
        let mut output = Vec::new();
        for message in [
            HostMessage::Hello {
                api_version: API_VERSION + 1,
                supported_versions: vec![],
                server: ServerStatus::new("test".into(), 20).unwrap(),
            },
            HostMessage::Event {
                event: Event::Stopping,
            },
        ] {
            let mut input = serde_json::to_vec(&Request { id: 0, message }).unwrap();
            input.push(b'\n');
            assert!(serve_io(TestPlugin, &input[..], &mut output).is_err());
        }
        assert!(read_line_bounded(&mut &vec![b'x'; MAX_MESSAGE_BYTES + 1][..]).is_err());
    }

    #[test]
    fn negotiates_v2_with_a_new_host_and_retains_v1_for_legacy_hooks() {
        for (supported, expected) in [(vec![1, 2], 2), (vec![], 1)] {
            let mut input = serde_json::to_vec(&Request {
                id: 1,
                message: HostMessage::Hello {
                    api_version: 1,
                    supported_versions: supported,
                    server: ServerStatus::new("test".into(), 20).unwrap(),
                },
            })
            .unwrap();
            input.push(b'\n');
            let mut output = Vec::new();
            serve_io(TestPlugin, &input[..], &mut output).unwrap();
            let response: Response = serde_json::from_slice(&output).unwrap();
            assert!(
                matches!(response.message, PluginMessage::Ready { api_version, .. } if api_version == expected)
            );
        }
    }

    struct GamePlugin;
    impl Plugin for GamePlugin {
        fn metadata(&self) -> Metadata {
            Metadata {
                name: "game".into(),
                version: "1".into(),
                hooks: vec![Hook::PlayerJoined],
            }
        }
        fn on_event(&mut self, _: Event) -> Vec<Action> {
            vec![]
        }
    }

    #[test]
    fn legacy_host_cannot_silently_accept_a_game_hook_plugin() {
        let mut input = serde_json::to_vec(&Request {
            id: 1,
            message: HostMessage::Hello {
                api_version: 1,
                supported_versions: vec![],
                server: ServerStatus::new("test".into(), 20).unwrap(),
            },
        })
        .unwrap();
        input.push(b'\n');
        assert!(serve_io(GamePlugin, &input[..], &mut Vec::new()).is_err());
    }
}
