//! Bounded multi-packet cursor distribution. Only completion mutates item state.
use super::{
    Action, Change, Click, DragMode, INVENTORY_SLOTS, Inventory, ItemStack, MAX_STACK,
    RejectReason, Request, Slot, SlotRef,
};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(super) struct Drag {
    slots: u64,
    cursor: ItemStack,
    mode: DragMode,
}

impl Inventory {
    pub(crate) fn cancel_drag(&mut self) {
        self.drag = None;
    }

    pub(super) fn drag(&mut self, click: Click) -> Result<Vec<Change>, RejectReason> {
        if let Click::DragStart { mode } = click {
            // Starting again during a gesture cancels it, as does an empty cursor.
            let active = self.drag.take();
            if active.is_none() {
                self.drag = self.cursor().map(|cursor| Drag {
                    slots: 0,
                    cursor,
                    mode,
                });
            }
            return Ok(vec![]);
        }
        let Some(mut drag) = self.drag.take() else {
            return Ok(vec![]);
        };
        let mode = match click {
            Click::DragAdd { mode, .. } | Click::DragFinish { mode } => mode,
            _ => return Ok(vec![]),
        };
        if drag.mode != mode || self.cursor() != Some(drag.cursor) {
            return Ok(vec![]);
        }
        if let Click::DragAdd { slot, .. } = click {
            if usize::from(slot) >= INVENTORY_SLOTS {
                return Err(RejectReason::InvalidSlot);
            }
            // Full matching stacks still participate in the division, but can
            // receive nothing. Duplicate selection never increases its weight.
            if self.slots()[usize::from(slot)].is_none_or(|s| s.kind == drag.cursor.kind)
                && drag.slots.count_ones() < u32::from(drag.cursor.count)
            {
                drag.slots |= 1u64 << slot;
            }
            self.drag = Some(drag);
            return Ok(vec![]);
        }
        let selected = drag.slots.count_ones();
        if selected == 0 {
            return Ok(vec![]);
        }
        if selected == 1 {
            return self
                .click(
                    Click::Pickup {
                        slot: drag.slots.trailing_zeros() as u8,
                        right: mode == DragMode::OneEach,
                    },
                    true,
                )
                .map_err(|error| {
                    error
                        .downcast_ref::<RejectReason>()
                        .copied()
                        .unwrap_or(RejectReason::UnsupportedAction)
                });
        }
        let share = match mode {
            DragMode::Even => drag.cursor.count / selected as u8,
            DragMode::OneEach => 1,
        };
        let mut actions = Vec::with_capacity(selected as usize);
        for slot in 0..INVENTORY_SLOTS {
            if drag.slots & (1u64 << slot) == 0 {
                continue;
            }
            let target = self.slots()[slot];
            if target.is_some_and(|s| s.kind != drag.cursor.kind) {
                continue;
            }
            let count = share.min(MAX_STACK - target.map_or(0, |s| s.count));
            if count == 0 {
                continue;
            }
            actions.push(Action::Transfer {
                source: SlotRef {
                    slot: Slot::Cursor,
                    expected_id: drag.cursor.net_id,
                },
                destination: SlotRef {
                    slot: Slot::Inventory(slot as u8),
                    expected_id: target.map_or(0, |s| s.net_id),
                },
                count,
            });
        }
        let mut candidate = self.state.clone();
        let changes = self.apply_actions(&Request { id: 0, actions }, &mut candidate)?;
        self.state = candidate;
        Ok(changes)
    }
}
