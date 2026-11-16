//! Integration-test fixture. Never loaded by a normal server configuration.
use sculith_plugin_api::{
    Action, CommandSpec, Event, Hook, InventoryOperation, InventoryResult, Metadata, Plugin, serve,
};
use std::collections::BTreeMap;

#[derive(Default)]
struct Inventory {
    observer: bool,
    next_request: u32,
    pending: BTreeMap<u32, (u64, bool)>,
}

impl Plugin for Inventory {
    fn metadata(&self) -> Metadata {
        Metadata {
            name: if self.observer {
                "inventory-fixture-observer"
            } else {
                "inventory-fixture"
            }
            .into(),
            version: env!("CARGO_PKG_VERSION").into(),
            hooks: if self.observer {
                vec![Hook::InventoryResponse]
            } else {
                vec![
                    Hook::Started,
                    Hook::CommandSubmitted,
                    Hook::InventoryResponse,
                    Hook::PlayerLeft,
                    Hook::Stopping,
                ]
            },
        }
    }

    fn on_event(&mut self, event: Event) -> Vec<Action> {
        if self.observer {
            return vec![Action::Log {
                message: "unexpected foreign inventory response".into(),
            }];
        }
        match event {
            Event::Started { .. } => [
                ("sculith_test_give", "Test inventory writes"),
                ("sculith_test_inventory", "Test inventory queries"),
            ]
            .into_iter()
            .map(|(name, description)| Action::RegisterCommand {
                command: CommandSpec {
                    name: name.into(),
                    description: description.into(),
                },
            })
            .collect(),
            Event::CommandSubmitted {
                player_id, command, ..
            } => {
                if self.pending.len() >= 128 {
                    return vec![];
                }
                let Some(request_id) = self.next_request.checked_add(1) else {
                    return vec![];
                };
                self.next_request = request_id;
                let give = command == "sculith_test_give";
                self.pending.insert(request_id, (player_id, give));
                vec![Action::Inventory {
                    player_id,
                    request_id,
                    operation: if give {
                        InventoryOperation::Give {
                            item: "minecraft:dirt".into(),
                            count: 5,
                        }
                    } else {
                        InventoryOperation::Get
                    },
                }]
            }
            Event::InventoryResponse {
                player_id,
                request_id,
                result,
            } => {
                let Some((expected, give)) = self.pending.remove(&request_id) else {
                    return vec![];
                };
                assert_eq!(player_id, expected, "inventory response recipient changed");
                let message = match result {
                    InventoryResult::Snapshot { inventory } => {
                        let occupied = inventory.slots.iter().flatten().count();
                        let total: u16 = inventory.slots.iter().flatten().map(|s| s.count).sum();
                        assert_eq!(inventory.slots.len(), 36);
                        if give {
                            "Added 5 Dirt.".into()
                        } else {
                            format!("Inventory: {occupied} stacks, {total} items.")
                        }
                    }
                    InventoryResult::Rejected { reason } => {
                        format!("Inventory request rejected: {reason:?}")
                    }
                };
                vec![
                    Action::Log {
                        message: format!(
                            "inventory response player={player_id} request={request_id} {message}"
                        ),
                    },
                    Action::SendMessage { player_id, message },
                ]
            }
            Event::PlayerLeft { player_id } => {
                self.pending.retain(|_, (id, _)| *id != player_id);
                vec![]
            }
            Event::Stopping => vec![Action::Log {
                message: "inventory plugin stopped".into(),
            }],
            _ => vec![],
        }
    }
}

fn main() -> std::io::Result<()> {
    serve(Inventory {
        observer: std::env::args().any(|a| a == "--observe"),
        ..Inventory::default()
    })
}
