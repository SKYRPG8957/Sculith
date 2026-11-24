use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError, atomic::Ordering::Relaxed},
};

use bytes::Bytes;
use pumpkin_data::{BlockState, BlockStateId, packet::CURRENT_MC_VERSION};
use pumpkin_world::chunk::palette::bedrock_water_state;
use sculith_protocol::{
    bedrock::client::CUpdateBlock,
    java::client::play::{CBlockUpdate, CMultiBlockUpdate},
};
use sculith_util::math::{
    position::{BlockPos, chunk_section_from_pos},
    vector2::Vector2,
    vector3::Vector3,
};

use crate::{
    entity::player::Player,
    net::{ClientPlatform, bedrock::BedrockClient, java::JavaClient},
    plugin::server::packet::PacketSentEvent,
};

use super::World;

type BlockStates = HashMap<BlockPos, BlockStateId>;
type BlockChanges = Vec<(BlockPos, BlockStateId)>;
type SectionUpdates = HashMap<Vector3<i32>, BlockChanges>;
type WorldSectionUpdates = HashMap<(Vector3<i32>, SentViewers), BlockChanges>;

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
    regional: HashMap<BlockPos, RegionalBlockUpdate>,
}

struct RegionalBlockUpdate {
    state: BlockStateId,
    sent: SentViewers,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(super) struct SentViewers(Option<Arc<[(i32, u32)]>>);

impl SentViewers {
    fn new(mut viewers: Vec<(i32, u32)>) -> Self {
        if viewers.is_empty() {
            return Self::default();
        }
        viewers.sort_unstable();
        Self(Some(viewers.into()))
    }

    pub(super) fn contains(&self, player: &Player) -> bool {
        self.0.as_ref().is_some_and(|viewers| {
            viewers
                .binary_search(&(player.entity_id(), player.chunk_send_epoch.load(Relaxed)))
                .is_ok()
        })
    }
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
            .extend(changes.into_iter().map(|(pos, state)| {
                (
                    pos,
                    RegionalBlockUpdate {
                        state,
                        sent: SentViewers::default(),
                    },
                )
            }));
    }

    fn defer_regional(&self, changes: BlockChanges, sent: &SentViewers) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        for (pos, state) in changes {
            pending
                .regional
                .entry(pos)
                .or_insert_with(|| RegionalBlockUpdate {
                    state,
                    sent: sent.clone(),
                });
        }
    }

    pub(super) fn try_publish(&self) -> Option<MutexGuard<'_, ()>> {
        try_lock(&self.publisher)
    }

    pub(super) fn take_world_sections(
        &self,
        current_state: impl Fn(&BlockPos) -> Option<BlockStateId>,
    ) -> WorldSectionUpdates {
        let PendingBlockUpdates { world, regional } =
            std::mem::take(&mut *self.pending.lock().unwrap_or_else(PoisonError::into_inner));
        let mut sections = WorldSectionUpdates::new();
        let remaining = regional.into_iter().filter_map(|(pos, update)| {
            (!world.contains_key(&pos)).then_some((pos, update.state, update.sent))
        });
        let ordinary = world
            .iter()
            .map(|(&pos, &state)| (pos, state, SentViewers::default()));
        for (pos, state, mut sent) in remaining.chain(ordinary) {
            let current = current_state(&pos).unwrap_or(state);
            if current != state {
                sent = SentViewers::default();
            }
            sections
                .entry((chunk_section_from_pos(&pos), sent))
                .or_default()
                .push((pos, current));
        }
        sections
    }
}

fn group_by_section(changes: impl IntoIterator<Item = (BlockPos, BlockStateId)>) -> SectionUpdates {
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
                .extract_if(|pos, update| {
                    update.sent.0.is_none()
                        && owns(pos.chunk_position())
                        && !world.contains_key(pos)
                })
                .take(REGIONAL_UPDATE_LIMIT)
                .map(|(pos, update)| (pos, update.state)),
        );
        drop(pending);

        for (section, updates) in changes {
            let chunk = Vector2::new(section.x, section.z);
            if let Some(remaining) = self.publish_regional_section(chunk, &updates, owns) {
                self.block_updates.defer_regional(updates, &remaining);
            }
        }
    }

    /// Returns the viewers already served if the world still owes this section to anyone.
    fn publish_regional_section(
        &self,
        chunk: Vector2<i32>,
        updates: &[(BlockPos, BlockStateId)],
        owns: &impl Fn(Vector2<i32>) -> bool,
    ) -> Option<SentViewers> {
        // Only the commit's plain block states can use this path. The world
        // publisher reconciles intervening writes and sends block-actor data.
        if updates.iter().any(|&(pos, state)| {
            state.to_state().block_entity_type != u16::MAX
                || self.level.try_read_block_state(pos) != Some(state)
        }) {
            return Some(SentViewers::default());
        }
        let players = self.players.load();
        let mut remaining = false;
        let mut sent = Vec::new();
        let mut java = None;
        let mut bedrock = None;
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
                remaining = true;
                continue;
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
            if !sender.is_chunk_ready(&chunk) || sender.pending_chunks.contains(&chunk) {
                remaining = true;
                continue;
            }
            if !owns(chunk) {
                remaining = true;
                break;
            }
            let recipient = (player.entity_id(), player.chunk_send_epoch.load(Relaxed));
            match player.client.as_ref() {
                ClientPlatform::Java(client) if client.version.load() == CURRENT_MC_VERSION => {
                    let Ok(bytes) = java.get_or_insert_with(|| encode_java_updates(updates)) else {
                        remaining = true;
                        continue;
                    };
                    if client.try_enqueue_packet_data_unobserved(bytes.clone()) {
                        client.flush_channel();
                        sent.push(recipient);
                    }
                }
                ClientPlatform::Bedrock(client) if client.is_current_protocol() => {
                    let Some(packets) =
                        bedrock.get_or_insert_with(|| encode_bedrock_updates(updates))
                    else {
                        remaining = true;
                        continue;
                    };
                    if packets
                        .iter()
                        .all(|bytes| client.try_enqueue_packet_data_unobserved(bytes.clone()))
                    {
                        sent.push(recipient);
                    }
                }
                _ => remaining = true,
            }
        }
        remaining.then(|| SentViewers::new(sent))
    }
}

fn encode_java_updates(
    updates: &[(BlockPos, BlockStateId)],
) -> Result<Bytes, sculith_protocol::ser::WritingError> {
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
        let earlier = SentViewers::new(vec![(6, 0)]);
        queue.defer_regional(vec![(first, stone)], &earlier);
        queue.queue_world([(second, air)]);
        let publisher = queue.try_publish().unwrap();
        assert!(queue.try_publish().is_none());
        drop(publisher);
        let mut pending = queue.take_world_sections(|_| None);
        let key = (chunk_section_from_pos(&first), SentViewers::default());
        assert_eq!(
            pending.remove(&key),
            Some(vec![(first, gold), (second, air)])
        );
        assert!(pending.is_empty());
        assert!(queue.take_world_sections(|_| None).is_empty());
    }

    #[test]
    fn a_world_replacement_reaches_viewers_who_got_the_regional_state() {
        let queue = BlockUpdates::default();
        let first = BlockPos::new(1, 64, 1);
        let second = BlockPos::new(4, 64, 1);
        let replaced = BlockPos::new(6, 64, 1);
        let stone = Block::STONE.default_state.id;
        let gold = Block::GOLD_BLOCK.default_state.id;
        let sent = SentViewers::new(vec![(6, 0), (4, 2)]);
        queue.defer_regional(
            vec![(first, stone), (second, stone), (replaced, stone)],
            &sent,
        );
        queue.queue_world([(replaced, stone)]);
        let sections = queue.take_world_sections(|position| (*position == second).then_some(gold));
        let section = chunk_section_from_pos(&first);
        assert_eq!(sections[&(section, sent)], vec![(first, stone)]);
        assert_eq!(
            sections[&(section, SentViewers::default())],
            vec![(second, gold), (replaced, stone)]
        );
    }
}
