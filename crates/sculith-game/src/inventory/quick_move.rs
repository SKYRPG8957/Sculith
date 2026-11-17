use super::{Change, INVENTORY_SLOTS, Inventory, MAX_STACK, RejectReason, Slot};

impl Inventory {
    /// Move between hotbar and main inventory. Merge matching stacks before
    /// filling an empty slot; a partially full destination leaves the remainder.
    /// Cursor/output, selection and request history belong to other operations.
    pub(super) fn quick_move(&mut self, source: u8) -> Result<Vec<Change>, RejectReason> {
        let Some(item) = self.get(Slot::Inventory(source))? else {
            return Ok(vec![]);
        };
        let destinations = if source < 9 { 9..INVENTORY_SLOTS } else { 0..9 };
        let mut candidate = self.state.clone();
        let mut remaining = item.count;
        let mut changes = Vec::new();
        for empty in [false, true] {
            for slot in destinations.clone() {
                if remaining == 0 {
                    break;
                }
                let previous = candidate.slots[slot];
                let available = match previous {
                    None if empty => MAX_STACK,
                    Some(stack) if !empty && stack.kind == item.kind => MAX_STACK - stack.count,
                    _ => 0,
                };
                let moved = remaining.min(available);
                if moved == 0 {
                    continue;
                }
                let stack = candidate.stack(item.kind, previous.map_or(0, |s| s.count) + moved)?;
                candidate.slots[slot] = stack;
                changes.push(Change {
                    slot: Slot::Inventory(slot as u8),
                    stack,
                });
                remaining -= moved;
            }
        }
        if remaining == item.count {
            return Ok(vec![]);
        }
        let stack = candidate.stack(item.kind, remaining)?;
        candidate.slots[usize::from(source)] = stack;
        changes.push(Change {
            slot: Slot::Inventory(source),
            stack,
        });
        self.state = candidate;
        Ok(changes)
    }
}
