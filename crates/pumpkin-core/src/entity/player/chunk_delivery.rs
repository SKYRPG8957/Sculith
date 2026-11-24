use std::sync::{
    Arc, PoisonError,
    atomic::Ordering::{Acquire, Relaxed},
};

use sculith_util::version::JavaMinecraftVersion;

use crate::{
    entity::EntityBase,
    net::{ChunkSender, ClientPlatform, bedrock::BedrockClient, chunk_sender::DispatchedChunk},
    plugin::{server::packet::PacketSentEvent, world::chunk_send::ChunkSend},
    server::{Server, TickDeadline},
    world::{World, chunker},
};

use super::Player;

impl Player {
    pub(super) fn tick_chunk_delivery(&self, server: &Server, deadline: Option<TickDeadline>) {
        let world = self.world();
        match self.client.as_ref() {
            ClientPlatform::Java(client) => {
                self.tick_java_chunk_delivery(&world, client.version.load());
            }
            ClientPlatform::Bedrock(client) => {
                if !self
                    .chunk_sender
                    .try_lock()
                    .is_ok_and(|sender| sender.has_bedrock_chunk_work())
                {
                    return;
                }
                let offered = deadline.is_some()
                    && self.can_dispatch_regional_terrain(client, server)
                    && server.plugin_manager.try_without_native_code().is_some()
                    && world
                        .get_player_by_uuid(self.gameprofile.id)
                        .is_some_and(|player| {
                            std::ptr::eq(player.as_ref(), self)
                                && world.offer_regional_terrain(client.id, &player)
                        });
                if !offered {
                    self.tick_bedrock_chunk_delivery(&world, client, deadline, None);
                }
            }
        }
    }

    fn tick_java_chunk_delivery(&self, world: &Arc<World>, version: JavaMinecraftVersion) {
        let player_chunk = self.get_entity().chunk_pos.load();
        let epoch = self.chunk_send_epoch.load(Relaxed);
        let view_distance = self.watched_section.load().view_distance;
        let Some(mut batch) = self.chunk_sender.try_lock().ok().and_then(|mut sender| {
            sender.prepare_batch(
                &world.level,
                player_chunk,
                view_distance,
                epoch,
                version,
                usize::MAX,
            )
        }) else {
            return;
        };
        let cancelled = match world.server.upgrade() {
            Some(server) if server.plugin_manager.has_handlers::<ChunkSend>() => {
                ChunkSend::retain_blocking(&server, world, &mut batch.chunks)
            }
            _ => Vec::new(),
        };
        let encoded = ChunkSender::encode_batch_shared(&batch, &world.encoded_chunk_cache);
        let current_epoch = self.chunk_send_epoch.load(Relaxed);
        let sent = self.chunk_sender.try_lock().map_or_else(
            |_| Vec::new(),
            |mut sender| {
                sender.cancel_pending(&batch, current_epoch, &cancelled);
                sender.commit_batch(&batch, &encoded, &self.client, current_epoch)
            },
        );
        self.pair_entities_in_chunks(world, &sent);
    }

    pub(crate) fn can_dispatch_regional_terrain(
        &self,
        client: &BedrockClient,
        server: &Server,
    ) -> bool {
        client.is_current_protocol()
            && self.bedrock_spawned.load(Acquire)
            && client.chunk_view_published()
            && chunker::bedrock_view_is_current(self, self.watched_section.load())
            && !server.tick_rate_manager.is_frozen()
            && !server.plugin_manager.has_handlers::<ChunkSend>()
            && !server.plugin_manager.has_handlers::<PacketSentEvent>()
    }

    pub(crate) fn tick_bedrock_chunk_delivery(
        &self,
        world: &Arc<World>,
        client: &Arc<BedrockClient>,
        deadline: Option<TickDeadline>,
        regional: Option<&dyn Fn() -> bool>,
    ) -> bool {
        let Ok(mut sender) = self.chunk_sender.try_lock() else {
            return true;
        };
        if !sender.has_bedrock_chunk_work() {
            return false;
        }
        let view = self.watched_section.load();
        if regional.is_some_and(|owns| {
            !owns()
                || !client.chunk_view_published()
                || !chunker::bedrock_view_is_current(self, view)
        }) || !client.claim_chunk_step(deadline)
        {
            return true;
        }

        let player_chunk = self.get_entity().chunk_pos.load();
        let epoch = self.chunk_send_epoch.load(Relaxed);
        // Share terrain work with the other viewers and allow only one round in flight.
        let max_chunks = if sender.has_pending_bedrock_delivery() {
            0
        } else {
            (256usize / world.players.load().len().max(1)).max(1)
        };
        let batch = if regional.is_some() {
            sender.prepare_regional_bedrock_batch(
                &world.level,
                player_chunk,
                view,
                epoch,
                max_chunks,
            )
        } else {
            sender.prepare_batch(
                &world.level,
                player_chunk,
                view.view_distance,
                epoch,
                JavaMinecraftVersion::V_1_20_2,
                max_chunks,
            )
        };
        let Some(batch) = batch else {
            return sender.has_bedrock_chunk_work();
        };
        if regional.is_some_and(|owns| !owns() || !client.chunk_view_published()) {
            return true;
        }
        let chunks = sender.commit_bedrock_batch(&batch, self.chunk_send_epoch.load(Relaxed));
        let pending = sender.has_bedrock_chunk_work();
        drop(sender);
        if !chunks.is_empty() {
            if regional.is_some() {
                tracing::trace!(player = %self.gameprofile.name, chunks = chunks.len(), "Dispatched Bedrock terrain on its region");
            }
            self.send_bedrock_chunk_batch(world.clone(), client.clone(), chunks, epoch);
        }
        pending
    }

    fn send_bedrock_chunk_batch(
        &self,
        world: Arc<World>,
        client: Arc<BedrockClient>,
        chunks: Vec<DispatchedChunk>,
        epoch: u32,
    ) {
        let uuid = self.gameprofile.id;
        self.spawn_task(async move {
            let (deliveries, chunks): (Vec<_>, Vec<_>) = chunks
                .into_iter()
                .map(|chunk| ((chunk.position, chunk.delivery_token), chunk.chunk))
                .unzip();
            let outcome = client.send_chunks(&chunks, Some(epoch)).await;
            let (deliveries, dropped): (Vec<_>, Vec<_>) = deliveries
                .into_iter()
                .partition(|(pos, _)| outcome.sent.contains(pos));
            let dropped: Vec<_> = dropped
                .into_iter()
                .map(|(pos, token)| (pos, token, outcome.failure(pos)))
                .collect();
            if let Some(player) = world.get_player_by_uuid(uuid).filter(|player| {
                player
                    .client
                    .bedrock()
                    .is_some_and(|active| Arc::ptr_eq(active, &client))
            }) {
                // A reset or re-enqueue assigns a new token before this task finishes.
                let mut sender = player
                    .chunk_sender
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let mut ready = sender.fail_delivery(&dropped);
                ready.extend(sender.mark_delivered(&deliveries));
                drop(sender);
                player.pair_entities_in_chunks(&world, &ready);
                player.try_bedrock_spawn();
            }
        });
    }
}
