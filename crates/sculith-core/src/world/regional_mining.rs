use dashmap::try_result::TryResult;
use pumpkin_data::{BlockDirection, BlockStateId, fluid::Fluid, statistic::StatisticCategory};
use sculith_util::math::{position::BlockPos, vector2::Vector2};

use crate::{
    entity::player::{MINE_BLOCK_EXHAUSTION, Player},
    plugin::{
        api::events::{
            block::block_physics::BlockPhysicsEvent,
            entity::entity_exhaustion::EntityExhaustionEvent,
            player::player_statistic_increment::PlayerStatisticIncrementEvent,
        },
        block::block_break::BlockBreakEvent,
    },
    server::Server,
};

use super::{World, loot::has_no_bare_hand_drops};

impl World {
    /// Finishes a bare-hand break that can be committed entirely within one extent.
    /// The caller holds the player's regional tool and movement guards.
    pub(crate) fn try_remove_regional_mining_block(
        &self,
        server: &Server,
        player: &Player,
        position: BlockPos,
        state: BlockStateId,
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
            || !self.has_no_mining_behavior(state)
            || !self.has_no_mining_behavior(BlockStateId::AIR)
        {
            return None;
        }

        let _save = server.try_entity_save_guard()?;
        let inventory = player.inventory.main_inventory.try_read().ok()?;
        let tool = inventory.get(usize::from(player.inventory.get_selected_slot()))?;
        if !tool.is_empty() {
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
        let commit = || {
            let mut stats = player.stats.try_lock().ok()?;
            let applied = self.level.try_apply_regional_block_edit(
                &chunk,
                position,
                state,
                BlockStateId::AIR,
                owns,
                |read| {
                    unobserved()
                        && BlockDirection::all().into_iter().all(|direction| {
                            let neighbor = position.offset(direction.to_offset());
                            self.is_in_build_limit(neighbor)
                                && read(neighbor)
                                    .is_some_and(|state| self.has_no_mining_behavior(state))
                        })
                },
            )?;
            applied.then(|| {
                if harvest && !abilities.invulnerable {
                    player.hunger_manager.add_exhaustion(MINE_BLOCK_EXHAUSTION);
                }
                stats.increment(StatisticCategory::Used, i32::from(tool.item.id), 1);
                stats.increment(StatisticCategory::Mined, i32::from(block.id.as_u16()), 1);
            })
        };
        if harvest && let Some(table) = block.loot_table {
            server
                .datapack_manager
                .try_with_builtin_loot_table(table, |table| {
                    // A random roll here followed by a world reroll would change the loot odds.
                    table
                        .is_none_or(has_no_bare_hand_drops)
                        .then(commit)
                        .flatten()
                })
        } else {
            commit()
        }
    }

    fn has_no_mining_behavior(&self, state: BlockStateId) -> bool {
        self.block_registry
            .get_pumpkin_block(state.to_block_id())
            .is_none()
            && Self::get_fluid_from_state_id(state) == &Fluid::EMPTY
    }
}
