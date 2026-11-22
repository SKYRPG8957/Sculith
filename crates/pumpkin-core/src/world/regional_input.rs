use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use pumpkin_util::math::{position::BlockPos, vector2::Vector2};
use rustc_hash::FxHashMap;

use crate::{
    entity::{EntityBase, player::Player},
    server::{Server, TickDeadline},
};

use super::{RegionChunks, RegionLayout, World};

bitflags::bitflags! {
    #[derive(Clone, Copy)]
    struct RegionalWork: u8 {
        const INPUT = 1;
        const EXPERIENCE_PICKUP = 2;
        const MINING = 4;
    }
}

type ReadyConnections = FxHashMap<u64, (Weak<Player>, RegionalWork)>;

/// Ready connections for an active extent; the ordered packets stay with the player.
pub(super) struct RegionalInput(Mutex<Option<ReadyConnections>>);

impl Default for RegionalInput {
    fn default() -> Self {
        Self(Mutex::new(Some(FxHashMap::default())))
    }
}

impl RegionalInput {
    fn ready(&self) -> MutexGuard<'_, Option<ReadyConnections>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn offer(&self, id: u64, player: &Arc<Player>, work: RegionalWork) -> bool {
        let mut queue = self.ready();
        let Some(ready) = queue.as_mut() else {
            return false;
        };
        ready
            .entry(id)
            .and_modify(|(_, waiting)| *waiting |= work)
            .or_insert_with(|| (Arc::downgrade(player), work));
        true
    }

    fn retire(&self) -> ReadyConnections {
        self.ready().take().unwrap_or_default()
    }

    fn take_ready(&self) -> ReadyConnections {
        self.ready()
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    pub(super) fn drain(
        &self,
        world: &World,
        server: &Server,
        extent: &RegionChunks,
        deadline: TickDeadline,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        let ready = self.take_ready();
        if ready.is_empty() {
            return;
        }
        let Some(_native) = server.plugin_manager.try_without_native_code() else {
            return;
        };
        for (id, (player, work)) in ready {
            let Some(player) = player.upgrade() else {
                continue;
            };
            let Some(client) = player.client.bedrock() else {
                continue;
            };
            let owns = |position| player.can_simulate_in(world) && owns(position);
            let mut pending = RegionalWork::empty();
            if work.contains(RegionalWork::INPUT)
                && client.drain_regional_input(&player, server, extent, owns)
            {
                pending |= RegionalWork::INPUT;
            }
            if work.contains(RegionalWork::EXPERIENCE_PICKUP)
                && client.advance_regional_experience_pickup(&player, deadline, || {
                    owns(player.get_entity().chunk_pos.load())
                })
            {
                pending |= RegionalWork::EXPERIENCE_PICKUP;
            }
            if work.contains(RegionalWork::MINING)
                && client.advance_regional_mining(world, &player, deadline, owns)
            {
                pending |= RegionalWork::MINING;
            }
            if !pending.is_empty() {
                // The owner can change after this job takes its ready list.
                player.world().offer_regional_work(id, &player, pending);
            }
        }
    }
}

impl RegionLayout {
    pub(super) fn transfer_regional_inputs(&self, next: &Self) {
        // Existing jobs can still return ready connections after retirement. They
        // look up the current extent in the same way as a newly arrived packet.
        for retired in self.replaced_inputs(next) {
            for (id, (player, work)) in retired.retire() {
                if let Some(player) = player.upgrade() {
                    player.world().offer_regional_work(id, &player, work);
                }
            }
        }
    }
}

impl World {
    pub(crate) fn offer_regional_input(&self, id: u64, player: &Arc<Player>) {
        self.offer_regional_work(id, player, RegionalWork::INPUT);
    }

    pub(crate) fn offer_regional_experience_pickup(&self, id: u64, player: &Arc<Player>) -> bool {
        self.offer_regional_work(id, player, RegionalWork::EXPERIENCE_PICKUP)
    }

    pub(crate) fn offer_regional_mining(
        &self,
        id: u64,
        player: &Arc<Player>,
        target: BlockPos,
    ) -> bool {
        let layout = self.region_layout.load();
        let target_owner = layout.owner(target.chunk_position());
        target_owner.is_some()
            && target_owner == layout.owner(player.get_entity().chunk_pos.load())
            && self.offer_regional_work(id, player, RegionalWork::MINING)
    }

    fn offer_regional_work(&self, id: u64, player: &Arc<Player>, work: RegionalWork) -> bool {
        if !player.can_simulate_in(self) {
            return false;
        }
        loop {
            let layout = self.region_layout.load();
            let Some(input) = layout
                .owner(player.get_entity().chunk_pos.load())
                .and_then(|owner| layout.input(owner))
            else {
                return false;
            };
            if input.offer(id, player, work) {
                return true;
            }
            // The publisher installed the new layout before closing this queue.
        }
    }
}
