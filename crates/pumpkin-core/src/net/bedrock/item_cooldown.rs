use std::{collections::HashMap, sync::PoisonError, time::Instant};

use crate::{
    entity::player::{ItemCooldown, Player},
    server::{Server, TickDeadline},
};

use super::BedrockClient;

#[derive(Clone, Copy)]
pub(super) struct ItemCooldownClock {
    tick: i32,
    started: Instant,
    last_step: Option<Instant>,
}

impl BedrockClient {
    // The player's item_cooldowns mutex serializes the clock and its entries.
    pub(crate) fn start_item_cooldown_clock(&self, world_tick: i32) -> i32 {
        if let Some(clock) = self.activity.item_cooldown.load() {
            return clock.tick;
        }
        self.activity.item_cooldown.store(Some(ItemCooldownClock {
            tick: world_tick,
            started: Instant::now(),
            last_step: None,
        }));
        world_tick
    }

    pub(crate) fn item_cooldown_tick(&self, world_tick: i32) -> i32 {
        self.activity
            .item_cooldown
            .load()
            .map_or(world_tick, |clock| clock.tick)
    }

    pub(crate) fn offer_item_cooldowns(&self, player: &Player) -> bool {
        let world = player.world();
        world
            .get_player_by_uuid(player.gameprofile.id)
            .is_some_and(|active| {
                std::ptr::eq(active.as_ref(), player)
                    && world.offer_regional_item_cooldowns(self.id, &active)
            })
    }

    pub(crate) fn tick_item_cooldowns(
        &self,
        server: &Server,
        player: &Player,
        deadline: Option<TickDeadline>,
    ) {
        if self.activity.item_cooldown.load().is_none() {
            return;
        }
        if deadline.is_some()
            && let Some(_native) = server.plugin_manager.try_without_native_code()
            && self.offer_item_cooldowns(player)
        {
            return;
        }
        let mut entries = player
            .item_cooldowns
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.advance_item_cooldowns(&mut entries, deadline, false);
    }

    /// Returns true while the connection still has work for its current region.
    pub(crate) fn advance_regional_item_cooldowns(
        &self,
        player: &Player,
        deadline: TickDeadline,
        owns: impl FnOnce() -> bool,
    ) -> bool {
        if self.activity.item_cooldown.load().is_none() {
            return false;
        }
        let Ok(mut entries) = player.item_cooldowns.try_lock() else {
            return true;
        };
        !owns() || self.advance_item_cooldowns(&mut entries, Some(deadline), true)
    }

    fn advance_item_cooldowns(
        &self,
        entries: &mut HashMap<String, ItemCooldown>,
        deadline: Option<TickDeadline>,
        regional: bool,
    ) -> bool {
        let Some(mut clock) = self.activity.item_cooldown.load() else {
            return false;
        };
        if deadline.is_some_and(|step| {
            clock
                .last_step
                .map_or(regional && step.at <= clock.started, |last| {
                    !step.follows(last)
                })
        }) {
            return true;
        }
        clock.tick = clock.tick.wrapping_add(1);
        clock.last_step = deadline.map(|step| step.at);
        entries.retain(|_, cooldown| cooldown.remaining_at(clock.tick).is_some());
        let active = !entries.is_empty();
        self.activity.item_cooldown.store(active.then_some(clock));
        active
    }
}
