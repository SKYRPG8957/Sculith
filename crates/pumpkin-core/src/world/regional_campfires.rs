use std::{
    any::Any,
    sync::{Arc, Mutex, PoisonError},
};

use dashmap::try_result::TryResult;
use pumpkin_data::{item::Item, item_stack::ItemStack, recipes::RecipeResultStruct};
use pumpkin_util::math::{position::BlockPos, vector2::Vector2};
use pumpkin_world::chunk::io::Dirtiable;
use rustc_hash::FxHashMap;

use crate::{
    block::entities::{BlockEntity, campfire::CampfireBlockEntity},
    entity::EntityBase,
    server::Server,
};

use super::World;

struct ReadyRecipe {
    position: BlockPos,
    fire: Arc<dyn BlockEntity>,
    slot: usize,
    ingredient: &'static Item,
    result: &'static RecipeResultStruct,
}

struct CookedDrop {
    position: BlockPos,
    fire: Arc<dyn BlockEntity>,
    item: Arc<dyn EntityBase>,
}

#[derive(Default)]
pub(super) struct RegionalCampfireOutputs(Mutex<Vec<CookedDrop>>);

impl World {
    pub(super) fn advance_regional_campfires(
        self: &Arc<Self>,
        server: &Server,
        pos: Vector2<i32>,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        let TryResult::Present(entities) = self.block_entities.try_get(&pos) else {
            return;
        };
        let can_finish = CampfireBlockEntity::outputs_unobserved(server);
        let mut ready = Vec::new();
        for entity in entities.values() {
            let Some(fire) = (entity.as_ref() as &dyn Any).downcast_ref::<CampfireBlockEntity>()
            else {
                continue;
            };
            let position = fire.position;
            if position.chunk_position() != pos {
                continue;
            }
            self.level.try_with_block_state(position, |chunk, state| {
                if owns(pos)
                    && fire.advance_regional_timers(state, |slot, ingredient, result| {
                        if can_finish {
                            ready.push(ReadyRecipe {
                                position,
                                fire: entity.clone(),
                                slot,
                                ingredient,
                                result,
                            });
                        }
                    })
                {
                    chunk.mark_dirty(true);
                }
            });
        }
        drop(entities);

        for recipe in ready {
            if !owns(pos) {
                break;
            }
            self.try_commit_campfire_output(server, pos, recipe, owns);
        }
    }

    fn try_commit_campfire_output(
        self: &Arc<Self>,
        server: &Server,
        pos: Vector2<i32>,
        recipe: ReadyRecipe,
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) {
        let Some(result) = Item::from_registry_key(recipe.result.id) else {
            return;
        };
        // Entity construction samples the biome. Release the chunk pins before
        // preparing it and validate the original actor again at commit.
        let item = CampfireBlockEntity::cooked_item(
            self,
            recipe.position,
            ItemStack::new(recipe.result.count, result),
        );
        item.init_data_tracker();

        let Some(_checkpoint) = server.try_regional_entity_save_guard() else {
            return;
        };
        let Ok(_snapshot) = self.entity_chunk_changes.snapshot.try_lock() else {
            return;
        };
        let Ok(mut pending) = self.regional_campfire_outputs.0.try_lock() else {
            return;
        };
        let TryResult::Present(entities) = self.block_entities.try_get(&pos) else {
            return;
        };
        let Some(entity) = entities
            .get(&recipe.position)
            .filter(|current| Arc::ptr_eq(current, &recipe.fire))
        else {
            return;
        };
        let Some(fire) = (entity.as_ref() as &dyn Any).downcast_ref::<CampfireBlockEntity>() else {
            return;
        };
        self.level
            .try_with_block_state(recipe.position, |chunk, state| {
                if owns(pos)
                    && CampfireBlockEntity::timers_unobserved(server)
                    && CampfireBlockEntity::outputs_unobserved(server)
                    && fire.finish_regional_slot(state, recipe.slot, recipe.ingredient, || {
                        self.append_live_entity(&item);
                        pending.push(CookedDrop {
                            position: recipe.position,
                            fire: recipe.fire,
                            item,
                        });
                    })
                {
                    chunk.mark_dirty(true);
                }
            });
    }

    pub(crate) fn flush_regional_campfire_outputs(&self) {
        let pending = std::mem::take(
            &mut *self
                .regional_campfire_outputs
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let mut changed_fires = FxHashMap::default();
        for drop in pending {
            if !drop.item.get_entity().is_removed() {
                self.track_new_entity(&drop.item);
                self.finish_entity_registration(&drop.item);
            }
            changed_fires.insert(drop.position, drop.fire);
        }
        for (position, fire) in changed_fires {
            // The position can have been broken, replaced or unloaded since
            // the region produced the item. Publish only the current actor.
            let active = self
                .block_entities
                .get(&position.chunk_position())
                .and_then(|actors| actors.get(&position).cloned());
            if active.is_some_and(|current| Arc::ptr_eq(&current, &fire)) {
                self.update_block_entity(&fire);
            }
        }
    }
}
