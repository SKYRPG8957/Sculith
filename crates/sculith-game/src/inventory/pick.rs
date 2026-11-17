//! Edition-independent block picking. No packet hints grant item authority.
use super::{CREATIVE_CATALOGUE, Change, Inventory, RejectReason, Response, Slot};
use crate::{GameState, InventoryOutcome};
use sculith_world::{BlockKind, BlockPos};
use std::time::Instant;

impl Inventory {
    /// Reuse an existing stack before creating a single creative item. When
    /// replacing a full hotbar, keep its old stack in the first empty main slot.
    /// Existing stacks retain their IDs; only newly picked items allocate IDs.
    pub fn pick(
        &mut self,
        kind: BlockKind,
        selected: u8,
        authorized: bool,
    ) -> Result<(u8, Vec<Change>), RejectReason> {
        self.cancel_drag();
        if !authorized {
            return Err(RejectReason::Unauthorized);
        }
        if selected >= 9 {
            return Err(RejectReason::InvalidSlot);
        }
        if !CREATIVE_CATALOGUE.contains(&kind) {
            return Err(RejectReason::UnsupportedItem);
        }
        let existing = self
            .state
            .slots
            .iter()
            .position(|stack| stack.is_some_and(|s| s.kind == kind));
        if let Some(slot) = existing.filter(|&slot| slot < 9) {
            return Ok((slot as u8, vec![]));
        }
        let empty = self.state.slots.iter().position(Option::is_none);
        let target = if existing.is_some() {
            selected as usize
        } else {
            empty.filter(|&slot| slot < 9).unwrap_or(selected as usize)
        };
        let mut candidate = self.state.clone();
        let previous = candidate.slots[target];
        let picked = existing.and_then(|slot| candidate.slots[slot]);
        let mut changes = Vec::with_capacity(2);
        // Existing main-inventory stack swaps with the held slot. Otherwise,
        // preserve the displaced held stack if any empty main slot exists.
        if let Some(destination) = existing.or_else(|| empty.filter(|&slot| slot >= 9)) {
            let replacement = previous;
            candidate.slots[destination] = replacement;
            if previous.is_some() || existing.is_some() {
                changes.push(Change {
                    slot: Slot::Inventory(destination as u8),
                    stack: replacement,
                });
            }
        }
        let stack = match picked {
            Some(stack) => Some(stack),
            None => candidate.stack(kind, 1)?,
        };
        candidate.slots[target] = stack;
        changes.push(Change {
            slot: Slot::Inventory(target as u8),
            stack,
        });
        self.state = candidate;
        Ok((target as u8, changes))
    }
}

impl GameState {
    /// Resolve the actual world block on the owner; never accept a claimed item.
    pub fn pick_block(
        &mut self,
        id: u64,
        position: BlockPos,
        now: Instant,
    ) -> anyhow::Result<InventoryOutcome> {
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        anyhow::ensure!(player.snapshot.initialized, "player is not initialized");
        let allowed = player.snapshot.permissions.can_build;
        let permitted = player.permit_inventory(now);
        let eye = player.snapshot.feet.eye();
        let decision = if !allowed {
            Err(RejectReason::Unauthorized)
        } else if !permitted {
            Err(RejectReason::RateLimited)
        } else {
            self.region
                .get_block(position)
                .map_err(|_| RejectReason::InvalidDestination)
                .and_then(|kind| {
                    if kind == BlockKind::Air {
                        return Err(RejectReason::UnsupportedItem);
                    }
                    self.reachable(eye, position)
                        .map_err(|_| RejectReason::InvalidDestination)?;
                    Ok(kind)
                })
        };
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        let result = decision
            .and_then(|kind| {
                player
                    .inventory
                    .pick(kind, player.snapshot.selected_slot, true)
            })
            .map(|(selected, changes)| {
                player.snapshot.selected_slot = selected;
                changes
            });
        Ok(player.finish_inventory(Response {
            request_id: 0,
            result,
            replayed: false,
        }))
    }
}
