//! Bounded opt-in public event observer. No packet bodies or credentials are read.
use sculith_plugin_api::{Action, Event, Hook, Metadata, Plugin, serve};
use std::collections::BTreeMap;

#[derive(Default)]
struct Counts {
    sequence: u64,
    packets: u64,
    moved: u64,
    equipped: u64,
}
struct Events {
    observer: bool,
    players: BTreeMap<u64, Counts>,
    packets: u64,
    moved: u64,
    equipped: u64,
    trace: bool,
}
fn log(message: String) -> Vec<Action> {
    vec![Action::Log { message }]
}
impl Plugin for Events {
    fn metadata(&self) -> Metadata {
        if self.observer {
            return Metadata {
                name: "events-observer".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                hooks: vec![Hook::CommandSubmitted],
            };
        }
        Metadata {
            name: "events-demo".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            hooks: vec![
                Hook::Started,
                Hook::PlayerJoined,
                Hook::PlayerLeft,
                Hook::PlayerMoved,
                Hook::PlayerEquipped,
                Hook::PlayerInventoryChanged,
                Hook::PacketReceived,
                Hook::PlayerChat,
                Hook::CommandSubmitted,
                Hook::Stopping,
            ],
        }
    }
    fn on_event(&mut self, event: Event) -> Vec<Action> {
        if self.observer {
            return log("unexpected foreign command invocation".into());
        }
        match event {
            Event::Started { .. } => vec![
                Action::RegisterCommand {
                    command: sculith_plugin_api::CommandSpec {
                        name: "hello".into(),
                        description: "Say hello".into(),
                    },
                },
                Action::Log {
                    message: "events plugin started".into(),
                },
            ],
            Event::CommandSubmitted {
                player_id,
                name,
                command,
                ..
            } => {
                assert_eq!(command, "hello");
                assert!(self.players.contains_key(&player_id));
                vec![
                    Action::SendMessage {
                        player_id,
                        message: format!("Hello {name} 👋"),
                    },
                    Action::Log {
                        message: format!("events command player={player_id} command={command}"),
                    },
                ]
            }
            Event::PlayerJoined { player } => {
                assert!(self.players.len() < 64 && !self.players.contains_key(&player.id));
                self.players.insert(player.id, Counts::default());
                vec![]
            }
            Event::PlayerLeft { player_id } => {
                self.players.remove(&player_id);
                vec![]
            }
            Event::PlayerInventoryChanged { player_id, changes } => {
                assert!(self.players.contains_key(&player_id));
                log(format!(
                    "events inventory player={player_id} slots={}",
                    changes.len()
                ))
            }
            Event::PlayerChat {
                player_id,
                name,
                message,
            } => {
                assert!(self.players.contains_key(&player_id));
                log(format!(
                    "events chat player={player_id} name={name} bytes={}",
                    message.len()
                ))
            }
            Event::PacketReceived { player_id, packet } => {
                let entry = self
                    .players
                    .get_mut(&player_id)
                    .expect("packet after initialized join");
                assert_eq!(
                    packet.sequence,
                    entry.sequence + 1,
                    "packet receipt sequence gap"
                );
                packet.validate().expect("valid metadata only");
                entry.sequence = packet.sequence;
                entry.packets += 1;
                self.packets += 1;
                if self.trace || entry.packets == 1 {
                    log(format!(
                        "events packet player={player_id} edition={:?} protocol={} id={} name={} dispatch={:?} sequence={}",
                        packet.edition,
                        packet.protocol,
                        packet.id,
                        packet.name,
                        packet.dispatch,
                        packet.sequence
                    ))
                } else {
                    vec![]
                }
            }
            Event::PlayerMoved {
                player_id,
                tick,
                x,
                y,
                z,
                yaw,
                pitch,
                head_yaw,
            } => {
                let entry = self
                    .players
                    .get_mut(&player_id)
                    .expect("move after initialized join");
                entry.moved += 1;
                self.moved += 1;
                if self.trace || entry.moved == 1 {
                    log(format!(
                        "events move player={player_id} tick={tick} feet={x},{y},{z} rotation={yaw},{pitch},{head_yaw}"
                    ))
                } else {
                    vec![]
                }
            }
            Event::PlayerEquipped {
                player_id,
                slot,
                item,
                count,
            } => {
                let entry = self
                    .players
                    .get_mut(&player_id)
                    .expect("equipment after initialized join");
                entry.equipped += 1;
                self.equipped += 1;
                log(format!(
                    "events equipment player={player_id} slot={slot} item={} count={count}",
                    item.as_deref().unwrap_or("empty")
                ))
            }
            Event::Stopping => log(format!(
                "events plugin stopped packets={} moved={} equipped={}",
                self.packets, self.moved, self.equipped
            )),
            _ => vec![],
        }
    }
}
fn main() -> std::io::Result<()> {
    serve(Events {
        observer: std::env::args().any(|arg| arg == "--observe"),
        players: BTreeMap::new(),
        packets: 0,
        moved: 0,
        equipped: 0,
        trace: std::env::args().any(|arg| arg == "--trace"),
    })
}
