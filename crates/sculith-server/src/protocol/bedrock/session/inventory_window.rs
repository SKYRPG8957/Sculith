//! Connection-local player-inventory screen acknowledgements, without item mutation.
use std::time::Instant;

use bedrock::protocol::{
    V2225,
    v662::{enums::ContainerID, packets::ContainerOpenPacket, types::ActorUniqueID},
    v685::packets::ContainerClosePacket,
    v898::packets::InteractPacketAction,
    v944::types::NetworkBlockPosition,
    v2225::enums::ContainerType,
};

use crate::game::PlayerSnapshot;

const FIRST_WINDOW: i8 = 2;
const LAST_WINDOW: i8 = 100;

pub(super) struct InventoryWindow {
    open: Option<i8>,
    next: i8,
    credits: f64,
    updated: Instant,
}

impl InventoryWindow {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            open: None,
            next: FIRST_WINDOW,
            credits: 16.0,
            updated: now,
        }
    }

    /// Only self-inventory windows are supported. Unknown/stale notifications
    /// neither create windows nor change owner inventory or permissions.
    pub(super) fn handle(
        &mut self,
        packet: &V2225,
        player: &PlayerSnapshot,
        now: Instant,
    ) -> Option<V2225> {
        match packet {
            V2225::InteractPacket(request)
                if matches!(request.action, InteractPacketAction::OpenInventory)
                    && request.target_runtime_id.0 == player.id
                    && player.initialized
                    && self.open.is_none() =>
            {
                let elapsed = now.checked_duration_since(self.updated)?;
                self.credits = (self.credits + elapsed.as_secs_f64() * 8.0).min(16.0);
                self.updated = now;
                if self.credits < 1.0 {
                    return None;
                }
                self.credits -= 1.0;
                let id = self.next;
                self.next = if id == LAST_WINDOW {
                    FIRST_WINDOW
                } else {
                    id + 1
                };
                self.open = Some(id);
                Some(V2225::ContainerOpenPacket(Box::new(ContainerOpenPacket {
                    container_id: ContainerID(id),
                    container_type: ContainerType::Inventory,
                    position: NetworkBlockPosition {
                        x: player.feet.x.floor() as i32,
                        y: player.feet.y.floor() as i32,
                        z: player.feet.z.floor() as i32,
                    },
                    target_actor_id: ActorUniqueID(-1),
                })))
            }
            V2225::ContainerClosePacket(request)
                if self.open == Some(request.container_id.0)
                    && !request.server_initiated_close
                    && matches!(
                        request.container_type,
                        ContainerType::Inventory | ContainerType::None
                    ) =>
            {
                self.open = None;
                // A close must remain usable after the open budget is spent.
                // Cursor/output belong to the owner and survive this notification.
                Some(V2225::ContainerClosePacket(Box::new(
                    ContainerClosePacket {
                        container_id: request.container_id,
                        container_type: ContainerType::Inventory,
                        server_initiated_close: false,
                    },
                )))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
