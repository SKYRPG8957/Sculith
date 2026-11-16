//! Integration-only lifecycle verifier. No commands or gameplay actions.
use sculith_plugin_api::{Action, ConnectionEvent, Event, Hook, Metadata, Plugin, serve};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct State {
    sequence: u64,
    sent_sequence: u64,
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
            name: if std::env::args().any(|arg| arg == "--api8") {
                "connection-fixture-v8"
            } else {
                "connection-fixture"
            }
            .into(),
            version: "1".into(),
            hooks: if std::env::args().any(|arg| arg == "--api8") {
                vec![Hook::Connection, Hook::Stopping]
            } else {
                vec![Hook::Connection, Hook::PacketSent, Hook::Stopping]
            },
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
                    ConnectionEvent::PacketSent {
                        connection_id,
                        packet,
                    } => {
                        assert!(
                            !std::env::args().any(|arg| arg == "--api8"),
                            "v8 received v9 event"
                        );
                        packet.validate().unwrap();
                        let state = self
                            .active
                            .get_mut(connection_id)
                            .expect("send without open");
                        assert_eq!(packet.sequence, state.sent_sequence + 1);
                        state.sent_sequence = packet.sequence;
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
                        "{} {}",
                        if std::env::args().any(|arg| arg == "--api8") {
                            "connection-v8-fixture"
                        } else {
                            "connection-fixture"
                        },
                        serde_json::to_string(&event).unwrap()
                    ),
                }]
            }
            Event::Stopping => {
                assert!(self.active.is_empty(), "missing close before stopping");
                vec![Action::Log {
                    message: if std::env::args().any(|arg| arg == "--api8") {
                        "connection v8 fixture stopped"
                    } else {
                        "connection fixture stopped"
                    }
                    .into(),
                }]
            }
            _ => vec![],
        }
    }
}

fn main() -> std::io::Result<()> {
    if std::env::args().any(|arg| arg == "--api8") {
        // Retained wire contract: select v8 and accept only its original events.
        use sculith_plugin_api::{HostMessage, PluginMessage, Request, Response};
        use std::io::{BufRead, Write};
        let mut state = Connections::default();
        let mut output = std::io::stdout().lock();
        for line in std::io::stdin().lock().lines() {
            let request: Request = serde_json::from_str(&line?)?;
            let message = match request.message {
                HostMessage::Hello {
                    supported_versions, ..
                } => {
                    assert!(supported_versions.contains(&8));
                    PluginMessage::Ready {
                        api_version: 8,
                        metadata: state.metadata(),
                    }
                }
                HostMessage::Event { event } => PluginMessage::Actions {
                    actions: state.on_event(event),
                },
            };
            serde_json::to_writer(
                &mut output,
                &Response {
                    id: request.id,
                    message,
                },
            )?;
            writeln!(output)?;
            output.flush()?;
        }
        Ok(())
    } else {
        serve(Connections::default())
    }
}
