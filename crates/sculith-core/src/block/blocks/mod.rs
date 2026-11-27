// Portals & dimension blocks
pub mod end_gateway;
pub mod end_portal;
pub mod end_portal_frame;
pub mod end_rod;
pub mod nether_portal;

// Storage & containers
pub mod barrel;
pub mod chests;
pub mod ender_chest;
pub mod hopper;
pub mod shulker_box;

// Workstations & processing
pub mod blast_furnace;
pub mod brewing_stand;
pub mod crafting_table;
pub mod enchanting_table;
pub mod furnace;
pub mod grindstone;
pub mod smoker;
pub mod stonecutter;

// Redstone & mechanisms
pub mod command; // command block / redstone control
pub mod jigsaw;
pub mod note;
pub mod piston;
pub mod redstone;
pub mod test_block;

// Doors, gates & openings
pub mod doors;
pub mod fence_gates;
pub mod fences;
pub mod trapdoor;

// Building & structural blocks
pub mod barrier;
pub mod chain;
pub mod glass_panes;
pub mod glazed_terracotta;
pub mod honey;
pub mod iron_bars;
pub mod scaffolding;
pub mod slabs;
pub mod stairs;
pub mod structure_void;
pub mod tinted_glass;
pub mod walls;
pub mod weathering_copper;

// Decorative & furnishings
pub mod banners;
pub mod bed;
pub mod carpet;
pub mod chiseled_bookshelf;
pub mod conduit;
pub mod flower_pots;
pub mod jukebox;
pub mod lectern;
pub mod shelf;
pub mod signs;
pub mod skull_block;
pub mod straw_bed;
pub mod wither_skull;

// Lighting, heat & fire
pub mod campfire;
pub mod candle_cakes;
pub mod candles;
pub mod fire;
pub mod lanterns;
pub mod light;
pub mod magma;
pub mod torches;

// Plants, crops & natural blocks
pub mod amethyst;
pub mod carved_pumpkin;
pub mod dripstone;
pub mod farmland;
pub mod hay;
pub mod huge_mushroom;
pub mod leaves;
pub mod logs;
pub mod mangrove_roots;
pub mod plant;
pub mod pumpkin;
pub mod sculk;
pub mod vine;

// Terrain / environment / physics
pub mod bonemealable_feature_placer;
pub mod bubble_column;
pub mod cobweb;
pub mod dirt_path;
pub mod dragon_egg;
pub mod falling;
pub mod grass_block;
pub mod ice;
pub mod infested;
pub mod mud;
pub mod netherrack;
pub mod nylium;
pub mod potent_sulfur;
pub mod powder_snow;
pub mod rooted_dirt;
pub mod snow;
pub mod soul_sand;
pub mod sponge;
pub mod spreading_snowy_block;

// Entities, hazards & misc gameplay blocks
pub mod anvil;
pub mod beehive;
pub mod cake;
pub mod cauldron;
pub mod composter;
pub mod dried_ghast;
pub mod frogspawn;
pub mod ladder;
pub mod respawn_anchor;
pub mod slime;
pub mod spawner;
pub mod tnt;

pub mod coral;

// Misc / abstract
pub mod abstract_wall_mounting;

pub mod beacon;
pub mod brushable_block;
pub mod cartography_table;
pub mod creaking_heart;
pub mod decorated_pot;
pub mod fletching_table;
pub mod loom;
pub mod smithing_table;
pub mod sniffer_egg;
pub mod structure_block;
pub mod trial_spawner;
pub mod turtle_egg;
pub mod vault;

use std::{cell::Cell, sync::Arc};

use pumpkin_data::{
    BlockState, BlockStateId,
    game_event::GameEvent,
    sound::{Sound, SoundCategory},
};
use sculith_util::math::position::BlockPos;
use sculith_world::world::BlockFlags;

use crate::{
    entity::player::Player,
    plugin::{
        server::packet::{BedrockConnectionPacketSentEvent, PacketSentEvent},
        world::generic_game::GenericGameEvent,
    },
    server::Server,
    world::World,
};

/// A block opening's state change and the effects at the clicked position.
pub(crate) struct BlockToggle {
    pub state: BlockStateId,
    pub java_sound: Sound,
    pub bedrock_sound: &'static str,
    pub event: GameEvent,
    pub other_half: Option<(BlockPos, BlockStateId, BlockStateId)>,
}

impl BlockToggle {
    const fn new(
        state: BlockStateId,
        open: bool,
        java_sound: Sound,
        (opening, closing): (&'static str, &'static str),
    ) -> Self {
        Self {
            state,
            java_sound,
            bedrock_sound: if open { opening } else { closing },
            event: if open {
                GameEvent::BlockOpen
            } else {
                GameEvent::BlockClose
            },
            other_half: None,
        }
    }

    const fn with_other_half(
        mut self,
        position: BlockPos,
        old: BlockStateId,
        new: BlockStateId,
    ) -> Self {
        self.other_half = Some((position, old, new));
        self
    }

    pub(crate) fn changes(
        &self,
        position: BlockPos,
        expected: BlockStateId,
    ) -> impl Iterator<Item = (BlockPos, BlockStateId, BlockStateId)> + use<> {
        std::iter::once((position, expected, self.state)).chain(self.other_half)
    }

    pub(crate) fn vertical_pair(
        &self,
        position: BlockPos,
        expected: BlockStateId,
    ) -> Option<(BlockPos, [(BlockStateId, BlockStateId); 2])> {
        let (other, before, next) = self.other_half?;
        let clicked = (expected, self.state);
        let companion = (before, next);
        Some(if other.0.y < position.0.y {
            (other, [companion, clicked])
        } else {
            (position, [clicked, companion])
        })
    }

    pub(crate) fn effects_observed(server: &Server) -> bool {
        let plugins = &server.plugin_manager;
        plugins.has_handlers::<GenericGameEvent>()
            || plugins.has_handlers::<PacketSentEvent>()
            || plugins.has_handlers::<BedrockConnectionPacketSentEvent>()
    }

    fn apply_in_world(
        player: Option<&Player>,
        world: &Arc<World>,
        position: BlockPos,
        plan: impl Fn(BlockStateId) -> Option<Self>,
    ) -> Option<BlockStateId> {
        Self::apply_world_update(
            player,
            world,
            position,
            BlockFlags::NOTIFY_LISTENERS,
            |state| plan(state).map(|toggle| (toggle, true)),
        )
    }

    fn apply_world_update(
        player: Option<&Player>,
        world: &Arc<World>,
        position: BlockPos,
        flags: BlockFlags,
        plan: impl Fn(BlockStateId) -> Option<(Self, bool)>,
    ) -> Option<BlockStateId> {
        if !world.is_in_build_limit(position) {
            return None;
        }
        loop {
            let expected = world.get_block_state_id(&position);
            let (toggle, play_effects) = plan(expected)?;
            let observed = play_effects
                && (world
                    .server
                    .upgrade()
                    .is_some_and(|server| Self::effects_observed(&server))
                    || world.block_toggle_has_listeners(position, toggle.event));
            if observed {
                // Keep the callback order for observers that can inspect the old state.
                toggle.publish_world_effects(player, world, position);
            }
            if !toggle.write_in_world(world, position, expected)? {
                if observed {
                    return None;
                }
                continue;
            }
            if play_effects && !observed {
                toggle.publish_world_effects(player, world, position);
            }
            for (position, old, new) in toggle.changes(position, expected) {
                world.on_block_state_set(&position, old, new, flags);
            }
            return Some(toggle.state);
        }
    }

    fn write_in_world(
        &self,
        world: &World,
        position: BlockPos,
        expected: BlockStateId,
    ) -> Option<bool> {
        if let Some((lower, changes)) = self.vertical_pair(position, expected) {
            if !world.is_in_build_limit(lower) || !world.is_in_build_limit(lower.up()) {
                return None;
            }
            return world
                .level
                .read_chunk_sync(&lower.chunk_position(), |chunk| {
                    chunk.set_vertical_block_pair_if(lower, changes)
                })?;
        }
        let compared = Cell::new(false);
        let replaced = world.write_block_state_if(&position, self.state, |_, live| {
            compared.set(true);
            live == expected
        });
        compared.get().then_some(replaced.is_some())
    }

    fn publish_world_effects(&self, player: Option<&Player>, world: &World, position: BlockPos) {
        if let Some(player) = player {
            world.play_block_sound_expect(player, self.java_sound, SoundCategory::Blocks, position);
        } else {
            world.play_block_sound(self.java_sound, SoundCategory::Blocks, position);
        }
        let center = position.to_centered_f64();
        world.play_bedrock_level_sound(
            self.bedrock_sound,
            &center,
            BlockState::to_be_network_id(self.state) as i32,
        );
        world.emit_game_event(self.event.name(), center);
    }
}
