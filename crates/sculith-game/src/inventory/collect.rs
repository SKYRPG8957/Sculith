use super::{Change, INVENTORY_SLOTS, Inventory, MAX_STACK, RejectReason, Slot};

impl Inventory {
    /// Collect matching items into the current cursor. Partial stacks precede
    /// full stacks; main inventory precedes hotbar unless the order is reversed.
    /// The empty clicked slot is checked against current owner state.
    pub(super) fn collect(
        &mut self,
        clicked: u8,
        reverse: bool,
    ) -> Result<Vec<Change>, RejectReason> {
        if self.get(Slot::Inventory(clicked))?.is_some() {
            return Ok(vec![]);
        }
        let Some(cursor) = self.cursor().filter(|s| s.count < MAX_STACK) else {
            return Ok(vec![]);
        };
        let mut candidate = self.state.clone();
        let mut count = cursor.count;
        let mut changes = Vec::new();
        for full in [false, true] {
            for offset in 0..INVENTORY_SLOTS {
                if count == MAX_STACK {
                    break;
                }
                let ordered = if reverse {
                    INVENTORY_SLOTS - 1 - offset
                } else {
                    offset
                };
                let slot = (ordered + 9) % INVENTORY_SLOTS;
                let Some(stack) = candidate.slots[slot] else {
                    continue;
                };
                if stack.kind != cursor.kind || (!full && stack.count == MAX_STACK) {
                    continue;
                }
                let taken = stack.count.min(MAX_STACK - count);
                let remaining = stack.with_count(stack.count - taken);
                candidate.slots[slot] = remaining;
                changes.push(Change {
                    slot: Slot::Inventory(slot as u8),
                    stack: remaining,
                });
                count += taken;
            }
        }
        if count == cursor.count {
            return Ok(vec![]);
        }
        candidate.cursor = cursor.with_count(count);
        changes.push(Change {
            slot: Slot::Cursor,
            stack: candidate.cursor,
        });
        self.state = candidate;
        Ok(changes)
    }
}
