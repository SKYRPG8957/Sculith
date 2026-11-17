//! Native player presentation with an original solid-colour placeholder skin.
//! Account skins/capes/persona transfer are deliberately outside this preview.
use crate::game::PlayerSnapshot;
use anyhow::{Result, ensure};
use bedrock::{
    network::tokio::transport::Transport,
    protocol::{
        ProtoCodec, V2225,
        v662::{
            enums::{BuildPlatform, GameType},
            types::{ActorRuntimeID, ActorUniqueID, PropertySyncData},
        },
        v2168::{enums::ArmSizeType, types::NetworkItemStackDescriptor},
        v2225::{
            packets::{AddPlayerListEntry, AddPlayerPacket, PlayerListEntry, PlayerListPacket},
            types::SerializedSkin,
        },
    },
};
use sculith_runtime::peers::Change;

/// Latest unsent pose per peer. Lifecycle/equipment changes flush earlier poses
/// before themselves; movement alone is emitted on the 20 Hz presentation tick.
/// The owner and plugin event streams retain every committed movement.
#[derive(Default)]
pub struct Pending {
    moves: Vec<Change>,
}

impl Pending {
    pub fn queue(&mut self, changes: Vec<Change>) -> Result<Vec<Change>> {
        let mut immediate = Vec::new();
        for change in changes {
            if let Change::Move { player, .. } = &change {
                if let Some(index) = self.moves.iter().position(|old| matches!(old, Change::Move { player: previous, .. } if previous.id == player.id)) {
                    self.moves[index] = change;
                } else {
                    ensure!(self.moves.len() < 63, "peer movement buffer exceeds player bound");
                    self.moves.push(change);
                }
            } else {
                immediate.append(&mut self.moves);
                immediate.push(change);
            }
        }
        Ok(immediate)
    }

    pub fn drain(&mut self) -> std::vec::Drain<'_, Change> {
        self.moves.drain(..)
    }

    pub fn clear(&mut self) {
        self.moves.clear();
    }
}

pub async fn send<T: Transport, I>(
    conn: &mut super::session::Observed<'_, T>,
    changes: I,
) -> Result<()>
where
    I: IntoIterator<Item = Change>,
    I::IntoIter: Send,
{
    // Roster entries contain skin data and retain their separate byte bound.
    // Other records are small and bounded by 63 peers; batching them avoids a
    // compression/SCTP send for every visible player's movement each tick.
    const MAX_BATCH: usize = 64;
    let changes = changes.into_iter();
    let mut pending = Vec::with_capacity(changes.size_hint().0.min(MAX_BATCH));
    for change in changes {
        let packet = match change {
            Change::InfoAdd(player) => {
                if !pending.is_empty() {
                    conn.send(&pending).await?;
                    pending.clear();
                }
                conn.send(&[info(&player)?]).await?;
                continue;
            }
            Change::InfoRemove { uuid } => V2225::PlayerListPacket(Box::new(PlayerListPacket {
                entries: vec![PlayerListEntry::Remove { uuid }],
            })),
            Change::Spawn(player) => spawn(&player)?,
            Change::Equipment(player) => super::output::equipment(&player)?,
            Change::Move { player, tick } => super::output::movement(&player, tick, false),
            Change::Despawn { id } => super::output::remove_model(id),
        };
        pending.push(packet);
        if pending.len() == MAX_BATCH {
            conn.send(&pending).await?;
            pending.clear();
        }
    }
    if !pending.is_empty() {
        conn.send(&pending).await?;
    }
    Ok(())
}

fn info(player: &PlayerSnapshot) -> Result<V2225> {
    let color = bedrock::protocol::v800::types::Color::deserialize(&mut &[255u8; 4][..])?;
    Ok(V2225::PlayerListPacket(Box::new(PlayerListPacket {
        entries: vec![PlayerListEntry::Add(AddPlayerListEntry {
            uuid: player.profile.uuid,
            target_actor_id: ActorUniqueID(player.id as i64),
            player_name: player.profile.name.clone(),
            xbl_xuid: String::new(),
            play_fab_id: String::new(),
            platform_chat_id: String::new(),
            build_platform: BuildPlatform::Dedicated,
            serialized_skin: skin(player),
            is_teacher: false,
            is_host: false,
            is_sub_client: false,
            color,
        })],
    })))
}

fn skin(player: &PlayerSnapshot) -> SerializedSkin<V2225> {
    SerializedSkin {
        skin_id: "sculith-preview-player-v1".into(),
        skin_resource_patch: r#"{"geometry":{"default":"geometry.sculith.preview_player"}}"#.into(),
        skin_image_width: 64,
        skin_image_height: 64,
        skin_image_bytes: [75, 145, 85, 255].repeat(64 * 64),
        animations: vec![],
        cape_image_width: 0,
        cape_image_height: 0,
        cape_image_bytes: vec![],
        geometry_data: include_str!("preview_player_geometry.json").into(),
        geometry_data_engine_version: "1.26.0".into(),
        animation_data: String::new(),
        cape_id: String::new(),
        full_id: format!("sculith-preview-{}", player.profile.uuid),
        arm_size: ArmSizeType::Wide,
        skin_color: -1,
        persona_pieces: vec![],
        piece_tint_colors: vec![],
        is_premium_skin: false,
        is_persona_skin: false,
        is_persona_cape_on_classic_skin: false,
        is_primary_user: true,
        overrides_player_appearance: true,
        trusted_skin_flag: "True".into(),
        profile_hash: String::new(),
    }
}

fn spawn(player: &PlayerSnapshot) -> Result<V2225> {
    let held = super::output::held(player)?;
    Ok(V2225::AddPlayerPacket(Box::new(AddPlayerPacket {
        uuid: player.profile.uuid,
        player_name: player.profile.name.clone(),
        target_runtime_id: ActorRuntimeID(player.id),
        platform_chat_id: String::new(),
        position: (
            player.feet.x as f32,
            player.feet.y as f32 + super::spawn::EYE_OFFSET,
            player.feet.z as f32,
        ),
        velocity: (0.0, 0.0, 0.0),
        rotation: (player.rotation.pitch as f32, player.rotation.yaw as f32),
        y_head_rotation: player.rotation.head_yaw as f32,
        carried_item: NetworkItemStackDescriptor {
            id: held.id,
            stack_size: held.stack_size,
            aux_value: held.aux_value,
            net_id: held.net_id,
            block_runtime_id: held.block_runtime_id,
            user_data_buffer: held.user_data_buffer,
        },
        player_game_type: GameType::Creative,
        entity_data: vec![],
        synced_properties: PropertySyncData {
            int_entries_list: vec![],
            float_entries_list: vec![],
        },
        abilities_data: super::output::abilities(player),
        actor_links: vec![],
        passenger_of_block_data: None,
        device_id: String::new(),
        build_platform: BuildPlatform::Dedicated,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_poses_keep_the_latest_tick_without_crossing_equipment_or_despawn() {
        let mut game =
            sculith_game::GameState::new(sculith_world::Region::flat_origin(), 1).unwrap();
        let player = game
            .reserve(
                sculith_game::Profile {
                    subject: "test:peer-queue".into(),
                    name: "PeerQueue".into(),
                    uuid: uuid::Uuid::new_v4(),
                    edition: sculith_game::Edition::Bedrock,
                },
                sculith_game::Permissions { can_build: true },
                std::time::Instant::now(),
            )
            .unwrap();
        let player = game.activate(player.id).unwrap();
        let movement = |tick| Change::Move {
            player: player.clone(),
            tick,
        };
        let mut pending = Pending::default();
        assert!(
            pending
                .queue(vec![movement(1), movement(2)])
                .unwrap()
                .is_empty()
        );
        let immediate = pending
            .queue(vec![
                Change::Equipment(player.clone()),
                movement(3),
                Change::Despawn { id: player.id },
            ])
            .unwrap();
        assert!(matches!(
            immediate.as_slice(),
            [
                Change::Move { tick: 2, .. },
                Change::Equipment(_),
                Change::Move { tick: 3, .. },
                Change::Despawn { .. }
            ]
        ));
        assert_eq!(pending.drain().count(), 0);
        assert!(
            pending
                .queue(vec![movement(4), movement(5)])
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            pending.drain().collect::<Vec<_>>().as_slice(),
            [Change::Move { tick: 5, .. }]
        ));
        assert_eq!(pending.drain().count(), 0);
    }
}
