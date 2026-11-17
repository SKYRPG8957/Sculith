//! Integration-only message and committed-game-hook probe.
use sculith_plugin_api::{Action, Event, Hook, Metadata, Plugin, serve};

struct Gameplay;

impl Plugin for Gameplay {
    fn metadata(&self) -> Metadata {
        Metadata {
            name: "gameplay-fixture".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            hooks: vec![
                Hook::Started,
                Hook::PlayerJoined,
                Hook::PlayerLeft,
                Hook::BlocksChanged,
                Hook::Stopping,
            ],
        }
    }

    fn on_event(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Started { .. } => vec![Action::Log {
                message: "gameplay plugin started".into(),
            }],
            Event::PlayerJoined { player } => vec![Action::SendMessage {
                player_id: player.id,
                message: format!("plugin message: {} / 테스트 😀", player.name),
            }],
            Event::PlayerLeft { player_id } => vec![Action::Log {
                message: format!("gameplay player left id={player_id}"),
            }],
            Event::BlocksChanged {
                player_id,
                revision,
                changes,
            } => vec![Action::Log {
                message: format!(
                    "gameplay block commit revision={revision} actor={player_id} changes={}",
                    changes.len()
                ),
            }],
            Event::Stopping => vec![Action::Log {
                message: "gameplay plugin stopped".into(),
            }],
            _ => vec![],
        }
    }
}

fn main() -> std::io::Result<()> {
    serve(Gameplay)
}
