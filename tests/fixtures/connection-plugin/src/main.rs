//! Integration-only lifecycle verifier. No commands or gameplay actions.
use sculith_plugin_api::{Action, ConnectionEvent, Event, Hook, Metadata, Plugin, serve};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct State {
    sequence: u64,
    player: Option<u64>,
}
#[derive(Default)]
struct Connections {
    active: BTreeMap<u64, State>,
    seen: BTreeSet<u64>,
}

impl Plugin for Connections {
    fn metadata(&self) -> Metadata {
        Metadata {
            name: "connection-fixture".into(),
            version: "1".into(),
            hooks: vec![Hook::Connection, Hook::Stopping],
        }
    }
    fn on_event(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Connection { event } => {
                if std::env::args().any(|arg| arg == "--slow") {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                match &event {
                    ConnectionEvent::Opened { connection_id, .. } => {
                        assert!(*connection_id != 0 && self.seen.insert(*connection_id));
                        assert!(
                            self.active
                                .insert(*connection_id, State::default())
                                .is_none()
                        );
                    }
                    ConnectionEvent::PacketReceived {
                        connection_id,
                        packet,
                    } => {
                        packet.validate().unwrap();
                        let state = self
                            .active
                            .get_mut(connection_id)
                            .expect("packet without open");
                        assert_eq!(packet.sequence, state.sequence + 1);
                        state.sequence = packet.sequence;
                    }
                    ConnectionEvent::PlayerAttached {
                        connection_id,
                        player_id,
                    } => {
                        assert!(*player_id != 0);
                        let state = self
                            .active
                            .get_mut(connection_id)
                            .expect("attach without open");
                        assert!(state.player.replace(*player_id).is_none());
                    }
                    ConnectionEvent::Closed { connection_id } => {
                        assert!(self.active.remove(connection_id).is_some());
                    }
                }
                vec![Action::Log {
                    message: format!(
                        "connection-fixture {}",
                        serde_json::to_string(&event).unwrap()
                    ),
                }]
            }
            Event::Stopping => {
                assert!(self.active.is_empty(), "missing close before stopping");
                vec![Action::Log {
                    message: "connection fixture stopped".into(),
                }]
            }
            _ => vec![],
        }
    }
}

fn main() -> std::io::Result<()> {
    serve(Connections::default())
}
