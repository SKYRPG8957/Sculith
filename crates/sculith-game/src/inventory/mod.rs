//! Bounded creative-builder inventory owned by the game state, with no wire types.
//!
//! Inventory slots 0..9 are the hotbar. Cursor and created output are separate
//! slots; edition adapters translate their own container and menu indices.
//! Fresh request IDs must be negative and strictly decrease, allowing gaps. The
//! up to 64 requests (including failures), within 2,048 cached actions, can be
//! replayed only with equal semantic contents. Older IDs remain stale after eviction. This matches the
//! negative, decreasing request IDs used for client prediction and prevents an
//! evicted creative request from being applied twice.
//!
//! Stack IDs follow items through moves and count changes. Splitting into an
//! empty slot creates a fresh positive ID; merging retains the destination ID. Requests
//! operate on a candidate state: failure changes neither stacks nor ID allocation.
//! Request sequence/cache metadata still records a failed bounded request.

use std::collections::{BTreeMap, VecDeque};

use sculith_world::BlockKind;

#[cfg(test)]
mod batch_tests;
mod collect;
#[cfg(test)]
mod collect_tests;
mod pick;
#[cfg(test)]
mod pick_tests;
mod quick_move;
#[cfg(test)]
mod quick_move_tests;

pub const INVENTORY_SLOTS: usize = sculith_core::inventory::SLOT_COUNT;
pub const MAX_STACK: u8 = 64;
pub const MAX_ACTIONS: usize = 64;
const HISTORY_ACTIONS: usize = 2048;
pub(crate) const ACTIONS_PER_UNIT: usize = 32;
pub const REQUEST_HISTORY: usize = 64;

/// Catalogue identifiers are independent of both editions' item runtime IDs.
pub const CREATIVE_CATALOGUE: [BlockKind; 4] = [
    BlockKind::Stone,
    BlockKind::Dirt,
    BlockKind::Grass,
    BlockKind::Bedrock,
];

pub fn item_kind(name: &str) -> Option<BlockKind> {
    CREATIVE_CATALOGUE
        .into_iter()
        .find(|kind| item_name(*kind) == name)
}

pub fn item_name(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Air => "minecraft:air",
        BlockKind::Stone => "minecraft:stone",
        BlockKind::Dirt => "minecraft:dirt",
        BlockKind::Grass => "minecraft:grass_block",
        BlockKind::Bedrock => "minecraft:bedrock",
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ItemStack {
    pub kind: BlockKind,
    pub count: u8,
    pub net_id: i32,
}

impl ItemStack {
    fn with_count(self, count: u8) -> Option<Self> {
        (count != 0).then_some(Self { count, ..self })
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Slot {
    Inventory(u8),
    Cursor,
    CreatedOutput,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct SlotRef {
    pub slot: Slot,
    /// Positive: exact stack ID; zero: empty; negative: a request prediction.
    pub expected_id: i32,
}

/// Supported stock-Java player-inventory clicks; all counts come from the owner.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Click {
    Pickup { slot: u8, right: bool },
    HotbarSwap { slot: u8, hotbar: u8 },
    QuickMove { slot: u8 },
    Collect { slot: u8, reverse: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A bounded adapter rejection retaining the request ID for an error reply.
    Unsupported,
    CraftCreative {
        catalogue_id: u32,
    },
    /// Both Bedrock Take and Place normalize to this transfer operation.
    Transfer {
        source: SlotRef,
        destination: SlotRef,
        count: u8,
    },
    Swap {
        source: SlotRef,
        destination: SlotRef,
    },
    Destroy {
        source: SlotRef,
        count: u8,
    },
    /// Stack validation/resynchronization only; never another world block edit.
    Mine {
        source: SlotRef,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub id: i32,
    pub actions: Vec<Action>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Change {
    pub slot: Slot,
    pub stack: Option<ItemStack>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RejectReason {
    Unauthorized,
    RateLimited,
    InvalidRequestId,
    StaleRequest,
    RequestIdConflict,
    EmptyRequest,
    TooManyActions,
    InvalidSlot,
    InvalidCount,
    InvalidCatalogue,
    UnsupportedAction,
    UnsupportedItem,
    StackMismatch,
    MissingPrediction,
    SameSlot,
    EmptySource,
    IncompatibleStack,
    StackOverflow,
    InvalidDestination,
    InvalidMiningSlot,
    StackIdExhausted,
    Full,
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "inventory request rejected: {self:?}")
    }
}

impl std::error::Error for RejectReason {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub request_id: i32,
    pub result: Result<Vec<Change>, RejectReason>,
    pub replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct State {
    slots: [Option<ItemStack>; INVENTORY_SLOTS],
    cursor: Option<ItemStack>,
    created_output: Option<ItemStack>,
    // The exhausted sentinel i32::MAX + 1 fits here without wrapping.
    next_stack_id: u64,
}

impl State {
    fn get(&self, slot: Slot) -> Result<Option<ItemStack>, RejectReason> {
        match slot {
            Slot::Inventory(index) => self
                .slots
                .get(usize::from(index))
                .copied()
                .ok_or(RejectReason::InvalidSlot),
            Slot::Cursor => Ok(self.cursor),
            Slot::CreatedOutput => Ok(self.created_output),
        }
    }

    fn set(&mut self, slot: Slot, stack: Option<ItemStack>) -> Result<(), RejectReason> {
        match slot {
            Slot::Inventory(index) => {
                *self
                    .slots
                    .get_mut(usize::from(index))
                    .ok_or(RejectReason::InvalidSlot)? = stack;
            }
            Slot::Cursor => self.cursor = stack,
            Slot::CreatedOutput => self.created_output = stack,
        }
        Ok(())
    }

    fn stack(&mut self, kind: BlockKind, count: u8) -> Result<Option<ItemStack>, RejectReason> {
        if count == 0 {
            return Ok(None);
        }
        let net_id =
            i32::try_from(self.next_stack_id).map_err(|_| RejectReason::StackIdExhausted)?;
        self.next_stack_id = self
            .next_stack_id
            .checked_add(1)
            .ok_or(RejectReason::StackIdExhausted)?;
        Ok(Some(ItemStack {
            kind,
            count,
            net_id,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedRequest {
    request: Request,
    response: Response,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    state: State,
    history: VecDeque<CachedRequest>,
    history_actions: usize,
    lowest_request_id: Option<i32>,
}

impl Default for Inventory {
    fn default() -> Self {
        Self {
            state: State {
                slots: [None; INVENTORY_SLOTS],
                cursor: None,
                created_output: None,
                next_stack_id: 1,
            },
            history: VecDeque::new(),
            history_actions: 0,
            lowest_request_id: None,
        }
    }
}

impl Inventory {
    /// Server-authorized insertion. Merge existing stacks before using empty
    /// player slots; cursor/output are untouched. A full inventory or exhausted
    /// stack allocator rolls back the entire grant, including allocated IDs.
    pub fn give(&mut self, kind: BlockKind, count: u16) -> Result<Vec<Change>, RejectReason> {
        if !CREATIVE_CATALOGUE.contains(&kind) {
            return Err(RejectReason::UnsupportedItem);
        }
        if !(1..=sculith_core::inventory::MAX_GIVE_COUNT).contains(&count) {
            return Err(RejectReason::InvalidCount);
        }
        let capacity: u16 = self
            .state
            .slots
            .iter()
            .map(|stack| match stack {
                None => u16::from(MAX_STACK),
                Some(stack) if stack.kind == kind => u16::from(MAX_STACK - stack.count),
                Some(_) => 0,
            })
            .sum();
        if capacity < count {
            return Err(RejectReason::Full);
        }
        let mut candidate = self.state.clone();
        let mut remaining = count;
        let mut changes = Vec::new();
        for empty in [false, true] {
            for slot in 0..INVENTORY_SLOTS {
                if remaining == 0 {
                    break;
                }
                let previous = candidate.slots[slot];
                let available = match previous {
                    None if empty => MAX_STACK,
                    Some(stack) if !empty && stack.kind == kind => MAX_STACK - stack.count,
                    _ => 0,
                };
                if available == 0 {
                    continue;
                }
                let taken = remaining.min(u16::from(available)) as u8;
                let stack = match previous {
                    Some(stack) => stack.with_count(stack.count + taken),
                    None => candidate.stack(kind, taken)?,
                };
                candidate.slots[slot] = stack;
                changes.push(Change {
                    slot: Slot::Inventory(slot as u8),
                    stack,
                });
                remaining -= u16::from(taken);
            }
        }
        if remaining != 0 {
            return Err(RejectReason::Full);
        }
        self.state = candidate;
        Ok(changes)
    }

    pub fn slots(&self) -> &[Option<ItemStack>; INVENTORY_SLOTS] {
        &self.state.slots
    }

    pub fn cursor(&self) -> Option<ItemStack> {
        self.state.cursor
    }

    pub fn created_output(&self) -> Option<ItemStack> {
        self.state.created_output
    }

    pub fn get(&self, slot: Slot) -> Result<Option<ItemStack>, RejectReason> {
        self.state.get(slot)
    }

    pub fn selected(&self, hotbar_slot: u8) -> Result<Option<ItemStack>, RejectReason> {
        if hotbar_slot >= 9 {
            return Err(RejectReason::InvalidSlot);
        }
        self.get(Slot::Inventory(hotbar_slot))
    }

    /// Apply a bounded inventory click from the owner's current state. Client
    /// prediction hashes and Bedrock request IDs play no part in this operation.
    pub fn click(&mut self, click: Click, authorized: bool) -> anyhow::Result<Vec<Change>> {
        if !authorized {
            return Err(RejectReason::Unauthorized.into());
        }
        let reference = |slot, stack: Option<ItemStack>| SlotRef {
            slot,
            expected_id: stack.map_or(0, |stack| stack.net_id),
        };
        let action = match click {
            Click::QuickMove { slot } => return Ok(self.quick_move(slot)?),
            Click::Collect { slot, reverse } => return Ok(self.collect(slot, reverse)?),
            Click::Pickup { slot, right } => {
                let slot = Slot::Inventory(slot);
                let item = self.get(slot)?;
                let cursor = self.cursor();
                match (item, cursor) {
                    (None, None) => None,
                    (Some(item), None) => Some(Action::Transfer {
                        source: reference(slot, Some(item)),
                        destination: reference(Slot::Cursor, None),
                        count: if right {
                            item.count.div_ceil(2)
                        } else {
                            item.count
                        },
                    }),
                    (item, Some(cursor)) if item.is_none_or(|item| item.kind == cursor.kind) => {
                        let capacity = MAX_STACK - item.map_or(0, |item| item.count);
                        let count = capacity.min(if right { 1 } else { cursor.count });
                        (count > 0).then_some(Action::Transfer {
                            source: reference(Slot::Cursor, Some(cursor)),
                            destination: reference(slot, item),
                            count,
                        })
                    }
                    (item, cursor) => Some(Action::Swap {
                        source: reference(Slot::Cursor, cursor),
                        destination: reference(slot, item),
                    }),
                }
            }
            Click::HotbarSwap { slot, hotbar } => {
                if hotbar >= 9 {
                    return Err(RejectReason::InvalidSlot.into());
                }
                let source = Slot::Inventory(slot);
                let destination = Slot::Inventory(hotbar);
                let from = self.get(source)?;
                let to = self.get(destination)?;
                if source == destination || (from.is_none() && to.is_none()) {
                    None
                } else {
                    Some(Action::Swap {
                        source: reference(source, from),
                        destination: reference(destination, to),
                    })
                }
            }
        };
        let Some(action) = action else {
            return Ok(vec![]);
        };
        let mut candidate = self.state.clone();
        let changes = self.apply_actions(
            &Request {
                id: 0,
                actions: vec![action],
            },
            &mut candidate,
        )?;
        self.state = candidate;
        Ok(changes)
    }

    /// The caller obtains authorization from the owner, never from a packet flag.
    pub fn apply(&mut self, request: &Request, authorized: bool) -> Response {
        let rejected = |reason| Response {
            request_id: request.id,
            result: Err(reason),
            replayed: false,
        };
        if request.id >= 0 {
            return rejected(RejectReason::InvalidRequestId);
        }
        if let Some(cached) = self
            .history
            .iter()
            .find(|entry| entry.request.id == request.id)
        {
            if cached.request != *request {
                return rejected(RejectReason::RequestIdConflict);
            }
            let mut response = cached.response.clone();
            response.replayed = true;
            return response;
        }
        if self
            .lowest_request_id
            .is_some_and(|lowest| request.id >= lowest)
        {
            return rejected(RejectReason::StaleRequest);
        }
        self.lowest_request_id = Some(request.id);
        if request.actions.len() > MAX_ACTIONS {
            // Do not clone an oversized vector into the bounded cache. Its ID is
            // consumed nevertheless, so retrying it cannot later mutate state.
            return rejected(RejectReason::TooManyActions);
        }

        let mut candidate = self.state.clone();
        let result = if !authorized {
            Err(RejectReason::Unauthorized)
        } else if request.actions.is_empty() {
            Err(RejectReason::EmptyRequest)
        } else {
            self.apply_actions(request, &mut candidate)
        };
        if result.is_ok() {
            self.state = candidate;
        }
        let response = Response {
            request_id: request.id,
            result,
            replayed: false,
        };
        while self.history.len() >= REQUEST_HISTORY
            || self.history_actions + request.actions.len() > HISTORY_ACTIONS
        {
            let previous = self.history.pop_front().expect("nonempty bounded history");
            self.history_actions -= previous.request.actions.len();
        }
        self.history_actions += request.actions.len();
        self.history.push_back(CachedRequest {
            request: request.clone(),
            response: response.clone(),
        });
        response
    }

    fn validate_ref(
        &self,
        reference: SlotRef,
        request_id: i32,
        candidate: &State,
        changes: &BTreeMap<Slot, Option<ItemStack>>,
    ) -> Result<Option<ItemStack>, RejectReason> {
        let current = candidate.get(reference.slot)?;
        let expected = if reference.expected_id >= 0 {
            reference.expected_id
        } else if reference.expected_id == request_id {
            changes
                .get(&reference.slot)
                .map(|stack| stack.map_or(0, |stack| stack.net_id))
                .ok_or(RejectReason::MissingPrediction)?
        } else {
            self.history
                .iter()
                .find(|entry| entry.request.id == reference.expected_id)
                .and_then(|entry| entry.response.result.as_ref().ok())
                .and_then(|changes| changes.iter().find(|change| change.slot == reference.slot))
                .map(|change| change.stack.map_or(0, |stack| stack.net_id))
                .ok_or(RejectReason::MissingPrediction)?
        };
        if current.map_or(0, |stack| stack.net_id) != expected {
            return Err(RejectReason::StackMismatch);
        }
        Ok(current)
    }

    fn apply_actions(
        &self,
        request: &Request,
        state: &mut State,
    ) -> Result<Vec<Change>, RejectReason> {
        let mut changes = BTreeMap::new();
        for action in &request.actions {
            match *action {
                Action::Unsupported => return Err(RejectReason::UnsupportedAction),
                Action::CraftCreative { catalogue_id } => {
                    let kind = catalogue_id
                        .checked_sub(1)
                        .and_then(|index| CREATIVE_CATALOGUE.get(index as usize))
                        .copied()
                        .ok_or(RejectReason::InvalidCatalogue)?;
                    let stack = state.stack(kind, MAX_STACK)?;
                    state.set(Slot::CreatedOutput, stack)?;
                    changes.insert(Slot::CreatedOutput, stack);
                }
                Action::Transfer {
                    source,
                    destination,
                    count,
                } => {
                    valid_count(count)?;
                    distinct_slots(source.slot, destination.slot)?;
                    if destination.slot == Slot::CreatedOutput {
                        return Err(RejectReason::InvalidDestination);
                    }
                    let from = self
                        .validate_ref(source, request.id, state, &changes)?
                        .ok_or(RejectReason::EmptySource)?;
                    let to = self.validate_ref(destination, request.id, state, &changes)?;
                    if count > from.count {
                        return Err(RejectReason::InvalidCount);
                    }
                    if to.is_some_and(|stack| stack.kind != from.kind) {
                        return Err(RejectReason::IncompatibleStack);
                    }
                    let total = to.map_or(0, |stack| stack.count) + count;
                    if total > MAX_STACK {
                        return Err(RejectReason::StackOverflow);
                    }
                    let after_from = from.with_count(from.count - count);
                    let after_to = match to {
                        Some(stack) => stack.with_count(total),
                        None if count == from.count => Some(from),
                        None => state.stack(from.kind, count)?,
                    };
                    state.set(source.slot, after_from)?;
                    state.set(destination.slot, after_to)?;
                    changes.insert(source.slot, after_from);
                    changes.insert(destination.slot, after_to);
                }
                Action::Swap {
                    source,
                    destination,
                } => {
                    distinct_slots(source.slot, destination.slot)?;
                    if [source.slot, destination.slot].contains(&Slot::CreatedOutput) {
                        return Err(RejectReason::InvalidDestination);
                    }
                    let from = self.validate_ref(source, request.id, state, &changes)?;
                    let to = self.validate_ref(destination, request.id, state, &changes)?;
                    state.set(source.slot, to)?;
                    state.set(destination.slot, from)?;
                    changes.insert(source.slot, to);
                    changes.insert(destination.slot, from);
                }
                Action::Destroy { source, count } => {
                    valid_count(count)?;
                    let from = self
                        .validate_ref(source, request.id, state, &changes)?
                        .ok_or(RejectReason::EmptySource)?;
                    if count > from.count {
                        return Err(RejectReason::InvalidCount);
                    }
                    let after = from.with_count(from.count - count);
                    state.set(source.slot, after)?;
                    changes.insert(source.slot, after);
                }
                Action::Mine { source } => {
                    if !matches!(source.slot, Slot::Inventory(0..=8)) {
                        return Err(RejectReason::InvalidMiningSlot);
                    }
                    let stack = self.validate_ref(source, request.id, state, &changes)?;
                    changes.insert(source.slot, stack);
                }
            }
        }
        Ok(changes
            .into_iter()
            .map(|(slot, stack)| Change { slot, stack })
            .collect())
    }

    /// Authorized creative replacement/removal, used by stock Java clients.
    /// Adapters must reject component patches and map menu slots before calling.
    pub fn replace_creative(
        &mut self,
        slot: u8,
        kind: Option<BlockKind>,
        count: u8,
        authorized: bool,
    ) -> anyhow::Result<Vec<Change>> {
        if !authorized {
            return Err(RejectReason::Unauthorized.into());
        }
        let slot = Slot::Inventory(slot);
        let before = self.get(slot)?;
        if let Some(kind) = kind {
            valid_count(count)?;
            if !CREATIVE_CATALOGUE.contains(&kind) {
                return Err(RejectReason::UnsupportedItem.into());
            }
        } else if count != 0 {
            return Err(RejectReason::InvalidCount.into());
        }
        if before.map(|stack| (stack.kind, stack.count)) == kind.map(|kind| (kind, count)) {
            return Ok(vec![]);
        }
        let mut candidate = self.state.clone();
        let stack = kind
            .map(|kind| candidate.stack(kind, count))
            .transpose()?
            .flatten();
        candidate.set(slot, stack)?;
        self.state = candidate;
        Ok(vec![Change { slot, stack }])
    }
}

fn valid_count(count: u8) -> Result<(), RejectReason> {
    if !(1..=MAX_STACK).contains(&count) {
        return Err(RejectReason::InvalidCount);
    }
    Ok(())
}

fn distinct_slots(source: Slot, destination: Slot) -> Result<(), RejectReason> {
    if source == destination {
        return Err(RejectReason::SameSlot);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
