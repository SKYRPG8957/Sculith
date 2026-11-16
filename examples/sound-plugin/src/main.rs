//! A short original pack sound, privately played for each initialized join.
use sculith_plugin_api::{Action, Event, Hook, Metadata, Plugin, serve};
struct Sounds;
impl Plugin for Sounds {
    fn metadata(&self) -> Metadata {
        Metadata {
            name: "sound-demo".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            hooks: vec![Hook::Started, Hook::PlayerJoined, Hook::Stopping],
        }
    }
    fn on_event(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Started { .. } => vec![Action::Log {
                message: "sound plugin started".into(),
            }],
            Event::PlayerJoined { player } => vec![
                Action::PlaySound {
                    player_id: player.id,
                    sound: "sculith:demo_tone".into(),
                    volume: 0.7,
                    pitch: 1.0,
                },
                Action::Log {
                    message: format!("sound requested for player={}", player.id),
                },
            ],
            Event::Stopping => vec![Action::Log {
                message: "sound plugin stopped".into(),
            }],
            _ => vec![],
        }
    }
}
fn main() -> std::io::Result<()> {
    serve(Sounds)
}
