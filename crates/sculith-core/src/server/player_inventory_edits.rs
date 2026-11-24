use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use pumpkin_data::item_stack::ItemStack;
use pumpkin_inventory::player::player_inventory::PlayerInventory;
use tokio::sync::oneshot;

use crate::entity::player::Player;
use crate::net::bedrock::BedrockClient;
use crate::plugin::server::packet::{BedrockConnectionPacketSentEvent, PacketSentEvent};
use crate::world::World;

use super::{Server, WorldOperation};

const WORLD_OPERATION_QUEUE_CAPACITY_FOR_EDITS: usize = 256;

/// Result of an inventory update on the player's simulation owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerInventoryEditStatus {
    Pending,
    Applied,
    /// The slot or the player session changed before the edit ran.
    Stale,
    /// The world unloaded or the server stopped.
    Unavailable,
}

/// Polls a queued inventory update without waiting on the simulation owner.
pub struct PlayerInventoryEditReceipt {
    result: oneshot::Receiver<PlayerInventoryEditStatus>,
    status: PlayerInventoryEditStatus,
}

impl PlayerInventoryEditReceipt {
    /// Returns the latest status, retaining a completed result for later calls.
    pub fn status(&mut self) -> PlayerInventoryEditStatus {
        if self.status == PlayerInventoryEditStatus::Pending {
            self.status = match self.result.try_recv() {
                Ok(status) => status,
                Err(oneshot::error::TryRecvError::Empty) => PlayerInventoryEditStatus::Pending,
                Err(oneshot::error::TryRecvError::Closed) => PlayerInventoryEditStatus::Unavailable,
            };
        }
        self.status
    }
}

pub(super) struct PlayerInventoryEdit {
    pub(super) world: Arc<World>,
    player: Weak<Player>,
    world_epoch: u32,
    slot: usize,
    expected: ItemStack,
    replacement: ItemStack,
    completion: Mutex<Option<oneshot::Sender<PlayerInventoryEditStatus>>>,
    finished: AtomicBool,
}

impl PlayerInventoryEdit {
    pub(super) fn claimed(&self) -> bool {
        self.completion
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none()
    }

    pub(super) fn ready_for_world(&self) -> bool {
        self.player.upgrade().is_none_or(|player| {
            player
                .client
                .bedrock()
                .filter(|client| client.is_current_protocol())
                .is_none_or(|client| {
                    client
                        .inventory_edits()
                        .front()
                        .is_some_and(|edit| std::ptr::eq(edit.as_ref(), self))
                })
        })
    }

    fn take_completion(&self) -> Option<oneshot::Sender<PlayerInventoryEditStatus>> {
        self.completion
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn finish(
        &self,
        completion: oneshot::Sender<PlayerInventoryEditStatus>,
        status: PlayerInventoryEditStatus,
    ) {
        let _ = completion.send(status);
        self.finished.store(true, Ordering::Release);
    }

    fn is_current(&self, player: &Arc<Player>) -> bool {
        player.can_simulate_in(&self.world)
            && player.chunk_send_epoch.load(Ordering::Acquire) == self.world_epoch
            && self
                .world
                .players
                .load()
                .iter()
                .any(|active| Arc::ptr_eq(active, player))
    }

    pub(super) fn apply(&self, server: &Server) {
        let Some(completion) = self.take_completion() else {
            return;
        };
        let status = if server.stop_token.is_cancelled() || !self.world.is_open() {
            PlayerInventoryEditStatus::Unavailable
        } else if let Some(player) = self.player.upgrade()
            && self.is_current(&player)
            && player.inventory().replace_slot_if_equal(
                self.slot,
                &self.expected,
                self.replacement.clone(),
            )
        {
            player.sync_hand_slot(self.slot, self.replacement.clone());
            PlayerInventoryEditStatus::Applied
        } else {
            PlayerInventoryEditStatus::Stale
        };
        self.finish(completion, status);
    }

    fn try_apply_in_region(
        &self,
        world: &World,
        player: &Arc<Player>,
        client: &BedrockClient,
        owns: impl FnOnce() -> bool,
    ) {
        // Only ordinary storage slots can be updated without equipment or open screen effects.
        if !(PlayerInventory::HOTBAR_SIZE..PlayerInventory::MAIN_SIZE).contains(&self.slot) {
            return;
        }
        let Ok(mut inventory) = player.inventory().main_inventory.try_write() else {
            return;
        };
        if !std::ptr::eq(self.world.as_ref(), world)
            || !player.has_stable_world_membership()
            || !self.is_current(player)
            || !owns()
        {
            return;
        }
        let current = &mut inventory[self.slot];
        let matches = if self.expected.is_empty() {
            current.is_empty()
        } else {
            current.are_equal(&self.expected)
        };
        if !matches {
            if let Some(completion) = self.take_completion() {
                self.finish(completion, PlayerInventoryEditStatus::Stale);
            }
            return;
        }
        let Some(packet) = client.encode_player_inventory_slot(self.slot, &self.replacement) else {
            return;
        };
        let Some(completion) = self.take_completion() else {
            return;
        };
        *current = self.replacement.clone();
        player.inventory_changed.store(true, Ordering::Relaxed);
        client.try_enqueue_packet_data_unobserved(packet);
        self.finish(completion, PlayerInventoryEditStatus::Applied);
        tracing::trace!(player = %player.gameprofile.name, slot = self.slot, "Applied player inventory edit on its region");
    }
}

/// Holds the same requests as the world dispatcher, in connection order.
/// The dispatcher owns their lifetime, including during shutdown.
#[derive(Default)]
pub struct PlayerInventoryEdits(Mutex<VecDeque<Weak<PlayerInventoryEdit>>>);

impl PlayerInventoryEdits {
    fn prune(queue: &mut VecDeque<Weak<PlayerInventoryEdit>>) -> Option<Arc<PlayerInventoryEdit>> {
        while let Some(front) = queue.front() {
            if let Some(edit) = front.upgrade()
                && !edit.finished.load(Ordering::Acquire)
            {
                return Some(edit);
            }
            queue.pop_front();
        }
        None
    }

    fn push(&self, edit: &Arc<PlayerInventoryEdit>) {
        let mut queue = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        Self::prune(&mut queue);
        queue.push_back(Arc::downgrade(edit));
    }

    fn front(&self) -> Option<Arc<PlayerInventoryEdit>> {
        Self::prune(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub(crate) fn drain_region(
        &self,
        server: &Server,
        world: &World,
        player: &Arc<Player>,
        client: &BedrockClient,
        owns: impl FnOnce() -> bool,
    ) -> bool {
        let Some(edit) = self.front() else {
            return false;
        };
        if server.stop_token.is_cancelled()
            || server.tick_rate_manager.is_frozen()
            || server.plugin_manager.has_handlers::<PacketSentEvent>()
            || server
                .plugin_manager
                .has_handlers::<BedrockConnectionPacketSentEvent>()
        {
            return false;
        }
        client.with_idle_player_inventory(player, || {
            edit.try_apply_in_region(world, player, client, owns);
        });
        self.front().is_some()
    }
}

impl Player {
    /// Queues a slot replacement on this player's owner, conditional on the expected contents.
    /// A disconnect, teleport, or respawn can invalidate the request before it runs.
    /// Dropping the receipt leaves the accepted edit queued.
    pub fn schedule_inventory_edit(
        self: &Arc<Self>,
        slot: u8,
        expected: ItemStack,
        replacement: ItemStack,
    ) -> Option<PlayerInventoryEditReceipt> {
        let slot = usize::from(slot);
        if slot >= PlayerInventory::MAIN_SIZE
            && !self.inventory().equipment_slots.contains_key(&slot)
        {
            return None;
        }
        let world_epoch = self.chunk_send_epoch.load(Ordering::Acquire);
        let world = self.world();
        let server = world.server.upgrade()?;
        if !self.can_simulate_in(&world) {
            return None;
        }

        // Admit the bounded edit under the gate shared with shutdown and membership submission.
        let closed = server
            .admission_closed
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if *closed
            || server.stop_token.is_cancelled()
            || server.world_operations.len() >= WORLD_OPERATION_QUEUE_CAPACITY_FOR_EDITS
        {
            return None;
        }
        let (completion, result) = oneshot::channel();
        let edit = Arc::new(PlayerInventoryEdit {
            world: world.clone(),
            player: Arc::downgrade(self),
            world_epoch,
            slot,
            expected,
            replacement,
            completion: Mutex::new(Some(completion)),
            finished: AtomicBool::new(false),
        });
        let bedrock = self
            .client
            .bedrock()
            .filter(|client| client.is_current_protocol());
        if let Some(client) = bedrock {
            client.inventory_edits().push(&edit);
        }
        server
            .world_operations
            .push(WorldOperation::InventoryEdit(edit));
        if let Some(client) = bedrock {
            world.offer_regional_inventory_edits(client.id, self);
        }
        Some(PlayerInventoryEditReceipt {
            result,
            status: PlayerInventoryEditStatus::Pending,
        })
    }
}
