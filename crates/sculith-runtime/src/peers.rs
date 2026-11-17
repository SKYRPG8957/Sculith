//! Per-viewer projection of owner-committed initialized players. This is not a
//! second simulation: it tracks visibility in the viewer's loaded chunks only.
use crate::WorldEvent;
use anyhow::{Result, bail, ensure};
use sculith_game::PlayerSnapshot;
use sculith_world::{BlockPos, ChunkPos};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub enum Change {
    InfoAdd(PlayerSnapshot),
    Spawn(PlayerSnapshot),
    Equipment(PlayerSnapshot),
    Move { player: PlayerSnapshot, tick: u64 },
    Despawn { id: u64 },
    InfoRemove { uuid: uuid::Uuid },
}

pub struct Peers {
    viewer: u64,
    known: BTreeMap<u64, PlayerSnapshot>,
    visible: BTreeSet<u64>,
}

fn interested(player: &PlayerSnapshot, interest: &BTreeMap<ChunkPos, u64>) -> bool {
    let p = player.feet;
    interest.contains_key(
        &BlockPos {
            x: p.x.floor() as i32,
            y: 0,
            z: p.z.floor() as i32,
        }
        .chunk(),
    )
}

impl Peers {
    pub fn new(viewer: u64, players: Vec<PlayerSnapshot>) -> Result<Self> {
        ensure!(players.len() <= 63, "peer snapshot exceeds player bound");
        let mut known = BTreeMap::new();
        for player in players {
            ensure!(
                player.initialized && player.id != viewer,
                "peer snapshot contains self or pending player"
            );
            ensure!(
                known.insert(player.id, player).is_none(),
                "duplicate peer snapshot entry"
            );
        }
        Ok(Self {
            viewer,
            known,
            visible: BTreeSet::new(),
        })
    }

    pub fn initial(&mut self, interest: &BTreeMap<ChunkPos, u64>) -> Vec<Change> {
        let mut out: Vec<_> = self.known.values().cloned().map(Change::InfoAdd).collect();
        out.extend(self.refresh(interest));
        out
    }

    /// Recover a lagged presentation stream from a new atomic owner snapshot.
    /// Remove old identities before adding replacements, refresh current poses
    /// and equipment, then continue exclusively on the snapshot's new receiver.
    pub fn synchronize(
        &mut self,
        players: Vec<PlayerSnapshot>,
        interest: &BTreeMap<ChunkPos, u64>,
    ) -> Result<Vec<Change>> {
        let mut next = Self::new(self.viewer, players)?;
        for (&id, player) in &next.known {
            if let Some(previous) = self.known.get(&id) {
                ensure!(
                    previous.profile == player.profile,
                    "peer identity changed during resynchronization"
                );
            }
        }
        let mut out = Vec::new();
        for (&id, player) in &self.known {
            if !next.known.contains_key(&id) {
                if self.visible.contains(&id) {
                    out.push(Change::Despawn { id });
                }
                out.push(Change::InfoRemove {
                    uuid: player.profile.uuid,
                });
            }
        }
        for (&id, player) in &next.known {
            let previous = self.known.get(&id);
            if previous.is_none() {
                out.push(Change::InfoAdd(player.clone()));
            }
            if interested(player, interest) {
                next.visible.insert(id);
                if previous.is_none() || !self.visible.contains(&id) {
                    out.push(Change::Spawn(player.clone()));
                } else {
                    if previous.is_some_and(|old| {
                        old.selected_slot != player.selected_slot || old.held() != player.held()
                    }) {
                        out.push(Change::Equipment(player.clone()));
                    }
                    // An older pose may still have been waiting for transport;
                    // always refresh visible actors, even if our cache matches.
                    out.push(Change::Move {
                        player: player.clone(),
                        tick: player.movement_tick.unwrap_or(0),
                    });
                }
            } else if self.visible.contains(&id) {
                out.push(Change::Despawn { id });
            }
        }
        *self = next;
        Ok(out)
    }

    pub fn refresh(&mut self, interest: &BTreeMap<ChunkPos, u64>) -> Vec<Change> {
        let mut out = vec![];
        for (&id, player) in &self.known {
            let show = interested(player, interest);
            if show && self.visible.insert(id) {
                out.push(Change::Spawn(player.clone()));
            } else if !show && self.visible.remove(&id) {
                out.push(Change::Despawn { id });
            }
        }
        out
    }

    pub fn event(
        &mut self,
        event: &WorldEvent,
        interest: &BTreeMap<ChunkPos, u64>,
    ) -> Result<Vec<Change>> {
        let mut out = vec![];
        match event {
            WorldEvent::Joined(player) if player.id != self.viewer => {
                ensure!(
                    player.initialized
                        && self.known.len() < 63
                        && !self.known.contains_key(&player.id),
                    "invalid or duplicate peer join"
                );
                self.known.insert(player.id, player.clone());
                out.push(Change::InfoAdd(player.clone()));
                if interested(player, interest) {
                    self.visible.insert(player.id);
                    out.push(Change::Spawn(player.clone()));
                }
            }
            WorldEvent::Moved { player, tick } if player.id != self.viewer => {
                let previous = self
                    .known
                    .get(&player.id)
                    .ok_or_else(|| anyhow::anyhow!("peer moved without membership"))?;
                ensure!(
                    player.initialized && previous.profile == player.profile,
                    "peer identity changed during movement"
                );
                self.known.insert(player.id, player.clone());
                if interested(player, interest) {
                    if self.visible.insert(player.id) {
                        out.push(Change::Spawn(player.clone()));
                    } else {
                        out.push(Change::Move {
                            player: player.clone(),
                            tick: *tick,
                        });
                    }
                } else if self.visible.remove(&player.id) {
                    out.push(Change::Despawn { id: player.id });
                }
            }
            WorldEvent::Equipped(player) if player.id != self.viewer => {
                let previous = self
                    .known
                    .get(&player.id)
                    .ok_or_else(|| anyhow::anyhow!("peer equipped without membership"))?;
                ensure!(
                    player.initialized && previous.profile == player.profile,
                    "peer identity changed during equipment update"
                );
                self.known.insert(player.id, player.clone());
                if self.visible.contains(&player.id) {
                    out.push(Change::Equipment(player.clone()));
                }
            }
            WorldEvent::Left { id } if *id != self.viewer => {
                let player = self
                    .known
                    .remove(id)
                    .ok_or_else(|| anyhow::anyhow!("peer left without membership"))?;
                if self.visible.remove(id) {
                    out.push(Change::Despawn { id: *id });
                }
                out.push(Change::InfoRemove {
                    uuid: player.profile.uuid,
                });
            }
            WorldEvent::Joined(_)
            | WorldEvent::Equipped(_)
            | WorldEvent::Moved { .. }
            | WorldEvent::Left { .. } => {}
            _ => bail!("non-membership event in peer stream"),
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sculith_game::{Edition, GameState, Permissions, Position, Profile};
    use sculith_world::Region;
    fn player() -> PlayerSnapshot {
        let mut game = GameState::new(Region::flat_origin(), 20).unwrap();
        let p = game
            .reserve(
                Profile {
                    subject: "test:peer".into(),
                    name: "Peer".into(),
                    uuid: uuid::Uuid::new_v4(),
                    edition: Edition::Bedrock,
                },
                Permissions::default(),
                std::time::Instant::now(),
            )
            .unwrap();
        game.activate(p.id).unwrap()
    }
    #[test]
    fn entering_and_leaving_loaded_chunks_changes_entities_without_losing_membership() {
        let p = player();
        let id = p.id;
        let mut interest = [(ChunkPos { x: 0, z: 0 }, 0)].into();
        let mut peers = Peers::new(u64::MAX, vec![p.clone()]).unwrap();
        assert!(matches!(
            peers.initial(&interest).as_slice(),
            [Change::InfoAdd(_), Change::Spawn(_)]
        ));
        let mut moved = p.clone();
        moved.feet = Position {
            x: 16.5,
            y: 4.0,
            z: 0.5,
        };
        assert!(
            matches!(peers.event(&WorldEvent::Moved {player:moved.clone(),tick:1},&interest).unwrap().as_slice(),[Change::Despawn {id:current}] if *current==id)
        );
        interest.insert(ChunkPos { x: 1, z: 0 }, 0);
        assert!(matches!(
            peers.refresh(&interest).as_slice(),
            [Change::Spawn(_)]
        ));
        assert!(matches!(
            peers
                .event(
                    &WorldEvent::Moved {
                        player: moved,
                        tick: 2
                    },
                    &interest
                )
                .unwrap()
                .as_slice(),
            [Change::Move { tick: 2, .. }]
        ));
        assert!(matches!(
            peers
                .event(&WorldEvent::Left { id }, &interest)
                .unwrap()
                .as_slice(),
            [Change::Despawn { .. }, Change::InfoRemove { .. }]
        ));
        assert!(peers.refresh(&interest).is_empty());
        assert!(
            peers
                .event(&WorldEvent::Moved { player: p, tick: 3 }, &interest)
                .is_err()
        );
    }
    #[test]
    fn hidden_equipment_updates_the_next_spawn_without_creating_an_entity() {
        let p = player();
        let id = p.id;
        let mut peers = Peers::new(u64::MAX, vec![p.clone()]).unwrap();
        let mut interest = BTreeMap::new();
        assert!(matches!(
            peers.initial(&interest).as_slice(),
            [Change::InfoAdd(_)]
        ));
        let mut selected = p;
        selected.selected_slot = 1;
        assert!(
            peers
                .event(&WorldEvent::Equipped(selected.clone()), &interest)
                .unwrap()
                .is_empty()
        );
        interest.insert(ChunkPos { x: 0, z: 0 }, 0);
        assert!(
            matches!(peers.refresh(&interest).as_slice(),[Change::Spawn(p)] if p.selected_slot==1)
        );
        selected.selected_slot = 8;
        assert!(
            matches!(peers.event(&WorldEvent::Equipped(selected.clone()),&interest).unwrap().as_slice(),[Change::Equipment(p)] if p.hotbar[p.selected_slot as usize].is_none())
        );
        peers.event(&WorldEvent::Left { id }, &interest).unwrap();
        assert!(
            peers
                .event(&WorldEvent::Equipped(selected), &interest)
                .is_err()
        );
    }

    #[test]
    fn pending_self_duplicate_and_private_events_do_not_enter_the_projection() {
        let mut p = player();
        assert!(Peers::new(p.id, vec![p.clone()]).is_err());
        assert!(Peers::new(u64::MAX, vec![p.clone(), p.clone()]).is_err());
        p.initialized = false;
        assert!(Peers::new(u64::MAX, vec![p]).is_err());
        let mut peers = Peers::new(1, vec![]).unwrap();
        assert!(
            peers
                .event(
                    &WorldEvent::Message {
                        player_id: 2,
                        message: "private".into()
                    },
                    &BTreeMap::new()
                )
                .is_err()
        );
    }
}
