//! One registered private static model; optional bounded replacement/hide probe.
use sculith_plugin_api::{Action, Event, Hook, Metadata, Plugin, serve};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
struct Models {
    exercise: bool,
    pending: BTreeMap<u64, (Instant, u8)>,
}
impl Plugin for Models {
    fn metadata(&self) -> Metadata {
        let mut hooks = vec![
            Hook::Started,
            Hook::PlayerJoined,
            Hook::PlayerLeft,
            Hook::Stopping,
        ];
        if self.exercise {
            hooks.push(Hook::Tick);
        }
        Metadata {
            name: "model-demo".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            hooks,
        }
    }
    fn on_event(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Started { .. } => vec![Action::Log {
                message: "model plugin started".into(),
            }],
            Event::PlayerJoined { player } => {
                if self.exercise && self.pending.len() < 64 {
                    self.pending.insert(player.id, (Instant::now(), 0));
                }
                vec![Action::ShowModel {
                    player_id: player.id,
                    model: "sculith:demo_statue".into(),
                }]
            }
            Event::PlayerLeft { player_id } => {
                self.pending.remove(&player_id);
                vec![]
            }
            Event::Tick { .. } => {
                let mut actions = vec![];
                for (&id, (joined, stage)) in &mut self.pending {
                    if actions.len() >= 32 {
                        break;
                    }
                    if *stage == 0 && joined.elapsed() >= Duration::from_secs(2) {
                        actions.push(Action::ShowModel {
                            player_id: id,
                            model: "sculith:demo_statue".into(),
                        });
                        *stage = 1;
                    } else if *stage == 1 && joined.elapsed() >= Duration::from_secs(4) {
                        actions.push(Action::HideModel { player_id: id });
                        *stage = 2;
                    }
                }
                self.pending.retain(|_, (_, stage)| *stage < 2);
                actions
            }
            Event::Stopping => vec![Action::Log {
                message: "model plugin stopped".into(),
            }],
            _ => vec![],
        }
    }
}
fn main() -> std::io::Result<()> {
    serve(Models {
        exercise: std::env::args().any(|a| a == "--exercise"),
        pending: BTreeMap::new(),
    })
}
