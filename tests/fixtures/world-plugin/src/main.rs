//! Integration-only world requests and observer isolation. Never a default plugin.
use sculith_plugin_api::{
    Action, CommandSpec, Event, Hook, Metadata, Plugin, RegionInfo, WorldBlock, WorldOperation,
    WorldPosition, WorldResult, serve,
};
use std::collections::BTreeMap;

#[derive(Default)]
struct World {
    mode: String,
    next_id: u32,
    pending: BTreeMap<u32, Option<u64>>,
    region: Option<RegionInfo>,
    notifications: usize,
}
fn position(x: i32) -> WorldPosition {
    WorldPosition { x, y: 4, z: 2 }
}
fn block(x: i32, name: &str) -> WorldBlock {
    WorldBlock {
        position: position(x),
        block: name.into(),
    }
}
impl World {
    fn request(&mut self, player: Option<u64>, operation: WorldOperation) -> Vec<Action> {
        let Some(id) = self.next_id.checked_add(1) else {
            return vec![];
        };
        if self.pending.len() >= 128 {
            return vec![];
        }
        self.next_id = id;
        self.pending.insert(id, player);
        vec![Action::World {
            request_id: id,
            operation,
        }]
    }
}
impl Plugin for World {
    fn metadata(&self) -> Metadata {
        let (name, hooks) = match self.mode.as_str() {
            "observe" => (
                "world-audit",
                vec![Hook::WorldBlocksChanged, Hook::Stopping],
            ),
            "guard" => (
                "world-private-guard",
                vec![Hook::WorldResponse, Hook::Stopping],
            ),
            "legacy" => (
                "world-legacy-observer",
                vec![Hook::BlocksChanged, Hook::Stopping],
            ),
            "flood" => ("world-flood-fixture", vec![Hook::Tick, Hook::WorldResponse]),
            _ => (
                "world-fixture",
                vec![
                    Hook::Started,
                    Hook::CommandSubmitted,
                    Hook::WorldResponse,
                    Hook::Stopping,
                ],
            ),
        };
        Metadata {
            name: name.into(),
            version: "1".into(),
            hooks,
        }
    }
    fn on_event(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Started { .. } => {
                let mut actions = vec![Action::RegisterCommand {
                    command: CommandSpec {
                        name: "sculith_test_world".into(),
                        description: "Test owner world operations".into(),
                    },
                }];
                actions.extend(self.request(None, WorldOperation::Read { positions: vec![] }));
                actions
            }
            Event::CommandSubmitted {
                player_id,
                arguments,
                ..
            } => {
                let region = self
                    .region
                    .as_ref()
                    .expect("world discovery did not finish");
                let operation = match arguments.as_str() {
                    "read" => WorldOperation::Read {
                        positions: vec![position(1), position(17), position(2)],
                    },
                    command => {
                        let blocks = match command {
                            "build" | "noop" | "stale" => {
                                vec![block(1, "minecraft:stone"), block(17, "minecraft:dirt")]
                            }
                            "erase" => vec![block(1, "minecraft:air"), block(17, "minecraft:air")],
                            "bounds" => vec![
                                block(1, "minecraft:bedrock"),
                                block(i32::MAX, "minecraft:stone"),
                            ],
                            "duplicate" => {
                                vec![block(1, "minecraft:bedrock"), block(1, "minecraft:dirt")]
                            }
                            "unsupported" => {
                                vec![block(1, "minecraft:bedrock"), block(17, "minecraft:glass")]
                            }
                            _ => return vec![],
                        };
                        WorldOperation::Edit {
                            region_id: region.id,
                            expected_revision: if command == "stale" {
                                0
                            } else {
                                region.revision
                            },
                            blocks,
                        }
                    }
                };
                self.request(Some(player_id), operation)
            }
            Event::WorldResponse { request_id, result } => {
                assert!(
                    self.mode != "guard",
                    "foreign world response reached observer"
                );
                if self.mode == "flood" {
                    return vec![];
                }
                let player = self
                    .pending
                    .remove(&request_id)
                    .expect("unknown world request");
                match &result {
                    WorldResult::Snapshot { region, .. } | WorldResult::Updated { region, .. } => {
                        self.region = Some(region.clone())
                    }
                    _ => {}
                }
                if let Some(player_id) = player {
                    vec![Action::SendMessage {
                        player_id,
                        message: format!(
                            "world-result {}",
                            serde_json::to_string(&result).unwrap()
                        ),
                    }]
                } else {
                    assert!(
                        matches!(result,WorldResult::Snapshot {blocks,..} if blocks.is_empty())
                    );
                    vec![Action::Log {
                        message: "world discovery completed before player requests".into(),
                    }]
                }
            }
            Event::WorldBlocksChanged {
                region_id,
                previous_revision,
                revision,
                actor,
                changes,
            } => {
                assert_eq!(region_id, 0);
                assert_eq!(previous_revision + 1, revision);
                self.notifications += 1;
                vec![Action::Log {
                    message: format!(
                        "world-audit {}",
                        serde_json::json!({"previous_revision":previous_revision,"revision":revision,"actor":actor,"changes":changes})
                    ),
                }]
            }
            Event::BlocksChanged {
                player_id,
                revision,
                changes,
            } => {
                assert!(player_id != 0);
                self.notifications += 1;
                vec![Action::Log {
                    message: format!(
                        "legacy-world-change player={player_id} revision={revision} count={}",
                        changes.len()
                    ),
                }]
            }
            Event::Tick { .. } if self.mode == "flood" => (0..32)
                .filter_map(|_| {
                    self.next_id = self.next_id.checked_add(1)?;
                    Some(Action::World {
                        request_id: self.next_id,
                        operation: WorldOperation::Read {
                            positions: vec![position(1); 64],
                        },
                    })
                })
                .collect(),
            Event::Stopping => {
                assert!(self.pending.is_empty(), "undelivered world response");
                vec![Action::Log {
                    message: format!(
                        "world fixture stopped mode={} notifications={}",
                        self.mode, self.notifications
                    ),
                }]
            }
            _ => vec![],
        }
    }
}
fn main() -> std::io::Result<()> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_default()
        .trim_start_matches("--")
        .to_owned();
    serve(World {
        mode,
        ..World::default()
    })
}
