use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard, PoisonError, TryLockError},
};

use bytes::Bytes;
use pumpkin_data::{BlockState, BlockStateId, packet::CURRENT_MC_VERSION};
use pumpkin_protocol::{
    bedrock::client::CUpdateBlock,
    java::client::play::{CBlockUpdate, CMultiBlockUpdate},
};
use pumpkin_util::math::{
    position::{BlockPos, chunk_section_from_pos},
    vector2::Vector2,
    vector3::Vector3,
};
use pumpkin_world::chunk::palette::bedrock_water_state;

use crate::{
    net::{ClientPlatform, bedrock::BedrockClient, java::JavaClient},
    plugin::server::packet::PacketSentEvent,
};

use super::World;

type BlockStates = HashMap<BlockPos, BlockStateId>;
type SectionUpdates = HashMap<Vector3<i32>, Vec<(BlockPos, BlockStateId)>>;

const REGIONAL_UPDATE_LIMIT: usize = 128;

#[derive(Default)]
pub(super) struct BlockUpdates {
    pending: Mutex<PendingBlockUpdates>,
    // World and region batches must enter each connection's FIFO in order.
    // Packet hooks may reenter a world flush, so a busy publisher leaves work queued.
    publisher: Mutex<()>,
}

#[derive(Default)]
struct PendingBlockUpdates {
    world: BlockStates,
    regional: BlockStates,
}

fn try_lock<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
    }
}

impl BlockUpdates {
    pub(super) fn queue_world(&self, changes: impl IntoIterator<Item = (BlockPos, BlockStateId)>) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .world
            .extend(changes);
    }

    fn queue_regional(&self, changes: impl IntoIterator<Item = (BlockPos, BlockStateId)>) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .regional
            .extend(changes);
    }

    fn retry_regional(&self, changes: Vec<(BlockPos, BlockStateId)>) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        for (pos, state) in changes {
            pending.regional.entry(pos).or_insert(state);
        }
    }

    pub(super) fn try_publish(&self) -> Option<MutexGuard<'_, ()>> {
        try_lock(&self.publisher)
    }

    pub(super) fn take_world(&self) -> BlockStates {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let mut changes = std::mem::take(&mut pending.regional);
        let world = std::mem::take(&mut pending.world);
        if changes.is_empty() {
            return world;
        }
        changes.extend(world);
        changes
    }
}

pub(super) fn group_by_section(
    changes: impl IntoIterator<Item = (BlockPos, BlockStateId)>,
) -> SectionUpdates {
    let mut sections = SectionUpdates::new();
    for (pos, state) in changes {
        sections
            .entry(chunk_section_from_pos(&pos))
            .or_default()
            .push((pos, state));
    }
    sections
}

impl World {
    pub(crate) fn queue_regional_block_updates(
        &self,
        changes: impl IntoIterator<Item = (BlockPos, BlockStateId)>,
    ) {
        self.block_updates.queue_regional(changes);
    }

    pub(super) fn flush_regional_block_updates(&self, owns: &impl Fn(Vector2<i32>) -> bool) {
        let Some(mut pending) = try_lock(&self.block_updates.pending) else {
            return;
        };
        if pending.regional.is_empty()
            || self
                .server
                .upgrade()
                .is_none_or(|server| server.plugin_manager.has_handlers::<PacketSentEvent>())
        {
            return;
        }
        let Some(_publishing) = self.block_updates.try_publish() else {
            return;
        };
        let PendingBlockUpdates { world, regional } = &mut *pending;
        let changes = group_by_section(
            regional
                .extract_if(|pos, _| owns(pos.chunk_position()) && !world.contains_key(pos))
                .take(REGIONAL_UPDATE_LIMIT),
        );
        drop(pending);

        for (section, updates) in changes {
            let chunk = Vector2::new(section.x, section.z);
            if !self.try_publish_regional_section(chunk, &updates, owns) {
                self.block_updates.retry_regional(updates);
            }
        }
    }

    fn try_publish_regional_section(
        &self,
        chunk: Vector2<i32>,
        updates: &[(BlockPos, BlockStateId)],
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> bool {
        // Only the commit's plain block states can use this path. The world
        // publisher reconciles intervening writes and sends block-actor data.
        if updates.iter().any(|&(pos, state)| {
            state.to_state().block_entity_type != u16::MAX
                || self.level.try_read_block_state(pos) != Some(state)
        }) {
            return false;
        }
        let players = self.players.load();
        let mut recipients = Vec::new();
        for player in players.iter() {
            if !player
                .watched_section
                .load()
                .is_within_distance(chunk.x, chunk.y)
                || player.client.closed()
            {
                continue;
            }
            let Some(sender) = try_lock(&player.chunk_sender) else {
                return false;
            };
            // The sender guard orders these packets before a chunk reset or world change.
            if !player.can_tick_in(self)
                || !player
                    .watched_section
                    .load()
                    .is_within_distance(chunk.x, chunk.y)
            {
                continue;
            }
            let current_protocol = match player.client.as_ref() {
                ClientPlatform::Java(client) => client.version.load() == CURRENT_MC_VERSION,
                ClientPlatform::Bedrock(client) => client.is_current_protocol(),
            };
            if !current_protocol || !sender.is_chunk_ready(&chunk) {
                return false;
            }
            recipients.push((player, sender));
        }

        let java = if recipients
            .iter()
            .any(|(player, _)| player.client.java().is_some())
        {
            let Ok(bytes) = encode_java_updates(updates) else {
                return false;
            };
            Some(bytes)
        } else {
            None
        };
        let bedrock = if recipients
            .iter()
            .any(|(player, _)| player.client.bedrock().is_some())
        {
            let Some(bytes) = encode_bedrock_updates(updates) else {
                return false;
            };
            bytes
        } else {
            Vec::new()
        };
        if !owns(chunk) {
            return false;
        }
        for (player, _sender) in recipients {
            match player.client.as_ref() {
                ClientPlatform::Java(client) => {
                    if let Some(bytes) = &java
                        && client.try_enqueue_packet_data_unobserved(bytes.clone())
                    {
                        client.flush_channel();
                    }
                }
                ClientPlatform::Bedrock(client) => {
                    for bytes in &bedrock {
                        if !client.try_enqueue_packet_data_unobserved(bytes.clone()) {
                            break;
                        }
                    }
                }
            }
        }
        true
    }
}

fn encode_java_updates(
    updates: &[(BlockPos, BlockStateId)],
) -> Result<Bytes, pumpkin_protocol::ser::WritingError> {
    if let [(pos, state)] = updates {
        let packet = CBlockUpdate::new(*pos, i32::from(state.as_u16()).into());
        JavaClient::serialize_packet_for_version(&packet, CURRENT_MC_VERSION)
    } else {
        JavaClient::serialize_packet_for_version(
            &CMultiBlockUpdate::new(updates),
            CURRENT_MC_VERSION,
        )
    }
}

fn encode_bedrock_updates(updates: &[(BlockPos, BlockStateId)]) -> Option<Vec<Bytes>> {
    let mut packets = Vec::with_capacity(updates.len() * 2);
    for &(pos, state) in updates {
        for (layer, state) in [(0, state), (1, bedrock_water_state(state))] {
            let mut encoded = Vec::new();
            let packet = CUpdateBlock::with_layer(pos, BlockState::to_be_network_id(state), layer);
            BedrockClient::write_raw_packet(&packet, &mut encoded).ok()?;
            packets.push(encoded.into());
        }
    }
    Some(packets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::Block;

    #[test]
    fn retry_keeps_newer_regional_updates_and_the_world_can_drain_both_queues() {
        let queue = BlockUpdates::default();
        let first = BlockPos::new(1, 64, 1);
        let second = BlockPos::new(4, 64, 1);
        let stone = Block::STONE.default_state.id;
        let gold = Block::GOLD_BLOCK.default_state.id;
        let air = BlockStateId::AIR;
        queue.queue_regional([(first, gold), (second, stone)]);
        queue.retry_regional(vec![(first, stone)]);
        queue.queue_world([(second, air)]);
        let publisher = queue.try_publish().unwrap();
        assert!(queue.try_publish().is_none());
        drop(publisher);
        let pending = queue.take_world();
        assert_eq!(pending, HashMap::from([(first, gold), (second, air)]));
        assert!(queue.take_world().is_empty());
    }
}
