use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use pumpkin_data::block_properties::{RespawnAnchorLikeProperties, WhiteBedLikeProperties};
use pumpkin_data::dimension::Dimension;
use pumpkin_data::tag::Taggable;
use pumpkin_data::{Block, BlockStateId, tag};
use pumpkin_inventory::Clearable;
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::java::client::play::{
    CGameEvent, CPlayerSpawnPosition, CRespawn, GameEvent, PlayerSpawnData,
};
use pumpkin_util::math::{position::BlockPos, vector2::Vector2, vector3::Vector3};
use pumpkin_world::world::BlockFlags;
use tokio::task::JoinHandle;

use crate::entity::EntityBase;
use crate::plugin::player::{
    player_change_world::PlayerChangeWorldEvent, player_respawn::PlayerRespawnEvent,
    player_spawn_location::PlayerSpawnLocationEvent,
};
use crate::server::Server;
use crate::world::World;

use super::world_transfer::{PendingWorldChange, WorldPosition, can_wait_for_world_change};
use super::{CalculatedRespawnPoint, GameMode, Player, RespawnPoint};

pub fn bedrock_dimension_id(dimension: &Dimension) -> i32 {
    match dimension.minecraft_name {
        "minecraft:the_nether" => 1,
        "minecraft:the_end" => 2,
        _ => 0,
    }
}

pub(super) struct RespawnAnchor {
    position: BlockPos,
    charged: BlockStateId,
    spent: BlockStateId,
}

impl RespawnAnchor {
    pub(super) fn consume(&self, world: &World) -> bool {
        world
            .write_block_state_if(&self.position, self.spent, |_, current| {
                current == self.charged
            })
            .is_some()
    }

    pub(super) fn notify(self, world: &Arc<World>) {
        world.on_block_state_set(
            &self.position,
            self.charged,
            self.spent,
            BlockFlags::NOTIFY_ALL,
        );
    }
}

struct SavedRespawn {
    point: RespawnPoint,
    location: WorldPosition,
}

impl SavedRespawn {
    async fn load(source: &Arc<World>, point: RespawnPoint) -> Option<Self> {
        let world = if source.dimension == point.dimension {
            source.clone()
        } else {
            source
                .server
                .upgrade()?
                .worlds
                .load()
                .iter()
                .find(|world| world.dimension == point.dimension)?
                .clone()
        };

        let min_chunk_x = point.position.0.x.saturating_sub(2) >> 4;
        let max_chunk_x = point.position.0.x.saturating_add(2) >> 4;
        let min_chunk_z = point.position.0.z.saturating_sub(2) >> 4;
        let max_chunk_z = point.position.0.z.saturating_add(2) >> 4;
        for x in min_chunk_x..=max_chunk_x {
            for z in min_chunk_z..=max_chunk_z {
                world
                    .level
                    .get_or_fetch_chunk(Vector2::new(x, z), |_| ())
                    .await;
            }
        }

        let (position, _) = Self::resolve(&world, &point)?;
        let location = WorldPosition {
            world,
            position,
            yaw: point.yaw,
            pitch: 0.0,
        };
        Some(Self { point, location })
    }

    fn resolve(
        world: &Arc<World>,
        point: &RespawnPoint,
    ) -> Option<(Vector3<f64>, Option<RespawnAnchor>)> {
        let pos = &point.position;
        let state = world.get_block_state_if_loaded(pos)?;
        if point.force {
            let above = world.get_block_state_if_loaded(&pos.up())?;
            return ((state.is_air() || !state.is_solid())
                && (above.is_air() || !above.is_solid()))
            .then(|| (pos.to_f64() + Vector3::new(0.0, 0.1, 0.0), None));
        }
        let block = Block::from_state_id(state.id);
        if block.has_tag(&tag::Block::MINECRAFT_BEDS) {
            let properties = WhiteBedLikeProperties::from_state_id(state.id);
            return Player::find_bed_spawn_position(world, pos, properties.facing)
                .map(|position| (position, None));
        }
        if block != &Block::RESPAWN_ANCHOR {
            return None;
        }
        let mut properties = RespawnAnchorLikeProperties::from_state_id(state.id);
        if properties.charges == 0 {
            return None;
        }
        let position = Player::find_anchor_spawn_position(world, pos)?;
        properties.charges -= 1;
        Some((
            position,
            Some(RespawnAnchor {
                position: *pos,
                charged: state.id,
                spent: properties.to_state_id(block),
            }),
        ))
    }
}

pub(super) struct RespawnCommit {
    request: RespawnRequest,
    saved: Option<SavedRespawn>,
    invalid: Option<RespawnPoint>,
}

impl RespawnCommit {
    pub(super) fn check(
        &self,
        player: &Player,
        destination: &Arc<World>,
    ) -> Result<Option<RespawnAnchor>, ()> {
        if !self.request.applies_to(player) {
            return Err(());
        }
        let Some(saved) = &self.saved else {
            return Ok(None);
        };
        let current = player
            .respawn_point
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if current.as_ref() != Some(&saved.point)
            || !Arc::ptr_eq(destination, &saved.location.world)
        {
            return Err(());
        }
        let (position, anchor) = SavedRespawn::resolve(destination, &saved.point).ok_or(())?;
        if position != saved.location.position {
            return Err(());
        }
        Ok(anchor)
    }

    pub(super) fn clear_invalid_spawn(&self, player: &Player) {
        if let Some(point) = &self.invalid
            && !point.force
        {
            let mut current = player
                .respawn_point
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if current.as_ref() == Some(point) {
                *current = None;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum RespawnRequest {
    Death,
    Manual { alive: bool },
}

impl RespawnRequest {
    fn applies_to(self, player: &Player) -> bool {
        matches!(self, Self::Manual { .. })
            || player.living_entity.dead.load(Ordering::Relaxed)
            || player.living_entity.health.load() <= 0.0
    }

    const fn keeps_data(self) -> bool {
        matches!(self, Self::Manual { alive: true })
    }
}

async fn world_spawn(world: &Arc<World>) -> WorldPosition {
    let (x, y, z, yaw, pitch) = {
        let info = world.level_info.load();
        (
            info.spawn_x,
            info.spawn_y,
            info.spawn_z,
            info.spawn_yaw,
            info.spawn_pitch,
        )
    };
    world
        .level
        .get_or_fetch_chunk(Vector2::new(x >> 4, z >> 4), |_| ())
        .await;
    let top = world.get_top_block(Vector2::new(x, z));
    let y = if top > world.dimension.min_y {
        top + 1
    } else {
        y
    };
    WorldPosition {
        world: world.clone(),
        position: Vector3::new(f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5),
        yaw,
        pitch,
    }
}

impl Player {
    /// Calculates the saved spawn without consuming an anchor charge.
    pub async fn calculate_respawn_point(&self) -> Option<CalculatedRespawnPoint> {
        let point = self
            .respawn_point
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        let saved = SavedRespawn::load(&self.world(), point).await?;
        Some(CalculatedRespawnPoint {
            position: saved.location.position,
            yaw: saved.location.yaw,
            pitch: saved.location.pitch,
            dimension: saved.location.world.dimension.clone(),
        })
    }

    pub(crate) fn request_client_respawn(self: &Arc<Self>) {
        self.start_respawn(&self.world(), RespawnRequest::Death);
    }

    /// Schedules a respawn. Blocking tick hooks can return before the state is applied.
    pub fn request_respawn(self: &Arc<Self>) {
        self.start_respawn(&self.world(), RespawnRequest::Manual { alive: false });
    }

    pub async fn respawn(self: &Arc<Self>) {
        self.respawn_in(&self.world(), false).await;
    }

    pub(crate) async fn respawn_in(self: &Arc<Self>, source: &Arc<World>, alive: bool) {
        let wait = source
            .server
            .upgrade()
            .as_deref()
            .is_some_and(can_wait_for_world_change);
        if let Some(task) = self.start_respawn(source, RespawnRequest::Manual { alive })
            && wait
        {
            let _ = task.await;
        }
    }

    fn start_respawn(
        self: &Arc<Self>,
        source: &Arc<World>,
        request: RespawnRequest,
    ) -> Option<JoinHandle<()>> {
        if !request.applies_to(self) {
            return None;
        }
        let server = source.server.upgrade()?;
        let change = self.reserve_world_change(source)?;
        let player = self.clone();
        self.spawn_task(async move {
            player.run_respawn(&server, &change, request).await;
        })
    }

    async fn run_respawn(
        self: &Arc<Self>,
        server: &Arc<Server>,
        change: &PendingWorldChange,
        request: RespawnRequest,
    ) {
        let source = &change.source;
        let default_world = server.get_world_from_dimension(&Dimension::OVERWORLD);
        let keep_inventory = default_world.level_info.load().game_rules.keep_inventory;
        let stored = self
            .respawn_point
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut saved = match stored.clone() {
            Some(point) => SavedRespawn::load(source, point).await,
            None => None,
        };
        let invalid = if saved.is_none() { stored } else { None };
        let mut destination = match &saved {
            Some(saved) => saved.location.clone(),
            None => world_spawn(&default_world).await,
        };

        let mut spawn_event = PlayerSpawnLocationEvent::new(self.clone(), destination.position);
        server.plugin_manager.fire(server, &mut spawn_event).await;
        destination.position = spawn_event.spawn_pos;
        if !Arc::ptr_eq(source, &destination.world) {
            let mut event = PlayerChangeWorldEvent {
                player: self.clone(),
                previous_world: source.clone(),
                new_world: destination.world.clone(),
                position: destination.position,
                yaw: destination.yaw,
                pitch: destination.pitch,
                cancelled: false,
            };
            server.plugin_manager.fire(server, &mut event).await;
            if event.cancelled {
                destination = world_spawn(source).await;
                saved = None;
            } else {
                if !Arc::ptr_eq(&destination.world, &event.new_world) {
                    saved = None;
                }
                destination = WorldPosition {
                    world: event.new_world,
                    position: event.position,
                    yaw: event.yaw,
                    pitch: event.pitch,
                };
            }
        }
        if !self
            .teleport_destination_allowed(server, destination.position)
            .await
        {
            destination = world_spawn(source).await;
            saved = None;
        }

        let mut notify_invalid = invalid.is_some();
        let respawn = RespawnCommit {
            request,
            saved,
            invalid,
        };
        if !self
            .apply_world_change(server, source, &destination, Some(respawn))
            .await
        {
            if self.client.closed() || server.stop_token.is_cancelled() || !request.applies_to(self)
            {
                return;
            }
            destination = world_spawn(source).await;
            let fallback = RespawnCommit {
                request,
                saved: None,
                invalid: None,
            };
            if !self
                .apply_world_change(server, source, &destination, Some(fallback))
                .await
            {
                return;
            }
            notify_invalid = true;
        }

        self.finish_respawn(
            server,
            source,
            &destination,
            request,
            keep_inventory,
            notify_invalid,
        )
        .await;
    }

    #[expect(clippy::too_many_lines)]
    async fn finish_respawn(
        self: &Arc<Self>,
        server: &Arc<Server>,
        source: &Arc<World>,
        destination: &WorldPosition,
        request: RespawnRequest,
        keep_inventory: bool,
        notify_invalid: bool,
    ) {
        let world = &destination.world;
        let position = destination.position;
        let yaw = destination.yaw;
        let pitch = destination.pitch;
        let mut event = PlayerRespawnEvent::new(
            self.clone(),
            source.clone(),
            world.clone(),
            position,
            yaw,
            pitch,
            request.keeps_data(),
        );
        server.plugin_manager.fire(server, &mut event).await;

        if notify_invalid {
            self.send_client_packet(&CGameEvent::new(GameEvent::NoRespawnBlockAvailable, 0.0))
                .await;
        }
        let last_death = self
            .last_death_location
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        self.send_client_packet(&CRespawn::new(
            PlayerSpawnData::new(
                world.dimension.clone(),
                pumpkin_world::biome::hash_seed(world.level.seed.0),
                self.gamemode.load() as u8,
                self.gamemode.load() as i8,
                false,
                world.is_flat(),
                last_death,
                VarInt(self.get_entity().portal_cooldown.load(Ordering::Relaxed) as i32),
                world.sea_level.into(),
            ),
            u8::from(request.keeps_data()),
        ))
        .await;
        if source.dimension != world.dimension {
            self.send_bedrock_dimension_change(world, position, true)
                .await;
        }
        let spawn = BlockPos(Vector3::new(
            position.x.round() as i32,
            position.y.round() as i32,
            position.z.round() as i32,
        ));
        self.send_packet_now_editioned(
            &CPlayerSpawnPosition::new(
                spawn,
                yaw,
                pitch,
                world.dimension.minecraft_name.to_string(),
            ),
            &pumpkin_protocol::bedrock::client::CSetSpawnPosition {
                spawn_position_type:
                    pumpkin_protocol::bedrock::client::SpawnPositionType::WorldRespawn,
                block_position: spawn,
                dimension_type: bedrock_dimension_id(&world.dimension).into(),
                spawn_block_pos: spawn,
            },
        )
        .await;

        self.living_entity.reset_state();
        self.hunger_manager.restart();
        self.send_permission_lvl_update();
        if !keep_inventory {
            self.set_experience(0, 0.0, 0);
            self.inventory.clear();
        }

        if Arc::ptr_eq(source, world) {
            world
                .entity_tracker
                .respawn_entity(&(self.clone() as Arc<dyn EntityBase>), world);
        } else {
            world.add_arriving_player(self);
            world.entity_tracker.repair_respawned_player(self, world);
        }
        world.send_world_info(self);
        world.send_center_chunk(self).await;
        self.send_committed_world_position(position, yaw, pitch);
        world.refresh_java_player_for_bedrock(self).await;

        if matches!(request, RespawnRequest::Death) && self.client.java().is_some() {
            if server.basic_config.hardcore {
                self.set_gamemode(GameMode::Spectator);
            }
            self.current_screen_handler
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .sync_state();
            self.abilities
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .set_for_gamemode(self.gamemode.load());
            self.send_abilities_update();
        }
        crate::entity::attributes::send_attribute_updates_for_living(
            &self.living_entity,
            vec![
                pumpkin_data::attributes::Attributes::ATTACK_SPEED,
                pumpkin_data::attributes::Attributes::ATTACK_DAMAGE,
            ],
        );
    }
}
