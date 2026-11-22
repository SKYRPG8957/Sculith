use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, Weak};

use pumpkin_data::item_stack::ItemStack;
use pumpkin_inventory::player::player_inventory::PlayerInventory;
use tokio::sync::oneshot;

use crate::entity::player::Player;
use crate::world::World;

use super::{Server, WorldOperation};

const WORLD_OPERATION_QUEUE_CAPACITY_FOR_EDITS: usize = 256;

/// Result of an inventory update on the player's world owner.
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
    completion: oneshot::Sender<PlayerInventoryEditStatus>,
}

impl PlayerInventoryEdit {
    pub(super) fn apply(self, server: &Server) {
        let status = if server.stop_token.is_cancelled() || !self.world.is_open() {
            PlayerInventoryEditStatus::Unavailable
        } else if let Some(player) = self.player.upgrade()
            && player.can_simulate_in(&self.world)
            && player.chunk_send_epoch.load(Ordering::Acquire) == self.world_epoch
            && self
                .world
                .get_player_by_uuid(player.gameprofile.id)
                .is_some_and(|active| Arc::ptr_eq(&active, &player))
            && player.inventory().replace_slot_if_equal(
                self.slot,
                &self.expected,
                self.replacement.clone(),
            )
        {
            player.sync_hand_slot(self.slot, self.replacement);
            PlayerInventoryEditStatus::Applied
        } else {
            PlayerInventoryEditStatus::Stale
        };
        let _ = self.completion.send(status);
    }
}

impl Player {
    /// Queues a slot replacement on this player's world, conditional on the expected contents.
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
        server
            .world_operations
            .push(WorldOperation::InventoryEdit(Box::new(
                PlayerInventoryEdit {
                    world,
                    player: Arc::downgrade(self),
                    world_epoch,
                    slot,
                    expected,
                    replacement,
                    completion,
                },
            )));
        Some(PlayerInventoryEditReceipt {
            result,
            status: PlayerInventoryEditStatus::Pending,
        })
    }
}
