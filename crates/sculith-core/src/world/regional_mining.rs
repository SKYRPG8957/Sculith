use std::sync::{Arc, atomic::Ordering::Relaxed};

use dashmap::try_result::TryResult;
use pumpkin_data::{
    BlockDirection, BlockStateId, Enchantment, fluid::Fluid, item_stack::ItemStack,
    statistic::StatisticCategory,
};
use pumpkin_inventory::Inventory;
use sculith_util::math::{position::BlockPos, vector2::Vector2};

use crate::{
    entity::{
        EntityBase,
        player::{BedrockMiningTool, MINE_BLOCK_EXHAUSTION, Player},
    },
    plugin::{
        api::events::{
            block::block_physics::BlockPhysicsEvent,
            entity::entity_exhaustion::EntityExhaustionEvent,
            player::{
                player_item_damage::PlayerItemDamageEvent,
                player_statistic_increment::PlayerStatisticIncrementEvent,
            },
        },
        block::block_break::BlockBreakEvent,
        server::packet::{BedrockConnectionPacketSentEvent, PacketSentEvent},
    },
    server::Server,
};

use super::{World, loot::has_no_drops_for_tool};

impl World {
    /// Finishes a no-drop break that can be committed entirely within one extent.
    /// The caller holds the player's regional tool and movement guards.
    pub(crate) fn try_remove_regional_mining_block(
        &self,
        server: &Server,
        player: &Player,
        position: BlockPos,
        state: BlockStateId,
        expected: BedrockMiningTool,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<()> {
        let block = state.to_block();
        let plugins = &server.plugin_manager;
        let unobserved = || {
            !plugins.has_handlers::<BlockBreakEvent>()
                && !plugins.has_handlers::<BlockPhysicsEvent>()
                && !plugins.has_handlers::<EntityExhaustionEvent>()
                && !plugins.has_handlers::<PlayerStatisticIncrementEvent>()
        };
        if !unobserved()
            || self.is_in_spawn_protection(player, &position)
            || !self.is_in_build_limit(position)
            || !self.is_plain_regional_block(state)
            || !self.is_plain_regional_block(BlockStateId::AIR)
        {
            return None;
        }

        let _save = server.try_entity_save_guard()?;
        let slot = expected.slot;
        let mut inventory = player.inventory.main_inventory.try_write().ok()?;
        let tool = inventory.get_mut(slot)?;
        if tool.uid != expected.stack_uid
            || server
                .item_registry
                .get_pumpkin_item(tool.item.id)
                .is_some()
        {
            return None;
        }
        let harvest = !state.to_state().tool_required() || tool.is_correct_for_drops(block);
        if harvest && block.experience.is_some() {
            return None;
        }
        let abilities = player.abilities.try_lock().ok()?;
        let TryResult::Present(chunk) =
            self.level.loaded_chunks.try_get(&position.chunk_position())
        else {
            return None;
        };
        let chunk = {
            let expected = chunk.value().clone();
            drop(chunk);
            expected
        };
        let commit = |tool: &mut ItemStack| {
            let item_id = tool.item.id;
            let damage = Player::mining_tool_damage(tool, state.to_state());
            self.try_commit_regional_tool_wear(server, player, (slot, tool), damage, || {
                let mut stats = player.stats.try_lock().ok()?;
                let applied = self.level.try_remove_regional_block(
                    &chunk,
                    position,
                    state,
                    |chunk| {
                        owns(chunk) && usize::from(player.inventory.get_selected_slot()) == slot
                    },
                    |read| {
                        unobserved()
                            && BlockDirection::all().into_iter().all(|direction| {
                                let neighbor = position.offset(direction.to_offset());
                                self.is_in_build_limit(neighbor)
                                    && read(neighbor)
                                        .is_some_and(|state| self.is_plain_regional_block(state))
                            })
                    },
                )?;
                applied.then(|| {
                    if harvest && !abilities.invulnerable {
                        player.hunger_manager.add_exhaustion(MINE_BLOCK_EXHAUSTION);
                    }
                    stats.increment(StatisticCategory::Used, i32::from(item_id), 1);
                    stats.increment(StatisticCategory::Mined, i32::from(block.id.as_u16()), 1);
                })
            })
        };
        if harvest && let Some(table) = block.loot_table {
            server
                .datapack_manager
                .try_with_builtin_loot_table(table, |table| {
                    // A random roll here followed by a world reroll would change the loot odds.
                    let loot_tool: &ItemStack = if tool.is_empty() {
                        ItemStack::EMPTY
                    } else {
                        tool
                    };
                    if !table.is_none_or(|table| has_no_drops_for_tool(table, loot_tool)) {
                        return None;
                    }
                    commit(tool)
                })
        } else {
            commit(tool)
        }
    }

    fn try_commit_regional_tool_wear(
        &self,
        server: &Server,
        player: &Player,
        (slot, tool): (usize, &mut ItemStack),
        damage: i32,
        remove: impl FnOnce() -> Option<()>,
    ) -> Option<()> {
        if damage <= 0 || tool.is_empty() || !tool.is_damageable() || tool.is_unbreakable() {
            return remove();
        }
        let next_damage = tool.get_damage().saturating_add(damage);
        let plugins = &server.plugin_manager;
        let unobserved = || {
            !plugins.has_handlers::<PlayerItemDamageEvent>()
                && !plugins.has_handlers::<PacketSentEvent>()
                && !plugins.has_handlers::<BedrockConnectionPacketSentEvent>()
        };
        if next_damage >= tool.get_max_damage()?
            || tool.get_enchantment_level(&Enchantment::UNBREAKING) > 0
            || !unobserved()
        {
            return None;
        }
        let client = player.client.bedrock()?;
        let vehicle = player.get_entity().vehicle.try_lock().ok()?;
        let active = player.living_entity.item_in_use.try_lock().ok()?;
        let advancements = player.advancements.try_lock().ok()?;
        if client.inventory_opened.load(Relaxed)
            || vehicle.is_some()
            || active.is_some()
            || advancements.has_on_foot_durability_listener(tool.item.registry_key)
        {
            return None;
        }
        let current = player.current_screen_handler.try_lock().ok()?;
        if !std::ptr::addr_eq(
            Arc::as_ptr(&current),
            Arc::as_ptr(&player.player_screen_handler),
        ) {
            return None;
        }
        let mut handler = current.try_lock().ok()?;
        // Player saves pin the cursor across the inventory and statistics snapshot.
        let cursor = handler.get_behaviour().cursor_stack.try_lock().ok()?;
        let inventory: Arc<dyn Inventory> = player.inventory.clone();
        let screen_slot = handler.get_slot_index(&inventory, slot)?;
        let mut next = tool.clone();
        // This plan changes only Damage, so the equipped tool keeps its modifiers.
        next.set_damage(next_damage);
        let packet = client.encode_player_inventory_slot(slot, &next)?;
        self.entity_tracker
            .try_publish_regional_main_hand(self, player, &next, || {
                if !unobserved() {
                    return None;
                }
                remove()?;
                *tool = next.clone();
                player.inventory_changed.store(true, Relaxed);
                Some(())
            })?;
        drop(cursor);
        if client.try_enqueue_packet_data_unobserved(packet) {
            handler.set_received_stack(screen_slot, next);
        }
        Some(())
    }

    pub(super) fn is_plain_regional_block(&self, state: BlockStateId) -> bool {
        self.block_registry
            .get_pumpkin_block(state.to_block_id())
            .is_none()
            && Self::get_fluid_from_state_id(state) == &Fluid::EMPTY
    }
}
