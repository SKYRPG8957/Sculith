//! Bounded creative-builder inventory owned by the game state, with no wire types.
//!
//! Inventory slots 0..9 are the hotbar. Cursor and created output are separate
//! slots; edition adapters translate their own container and menu indices.
//! Fresh request IDs must be negative and strictly decrease, allowing gaps. The
//! last 64 bounded requests (including failures) can be replayed only with equal
//! semantic contents. Older IDs remain stale after eviction. This matches the
//! negative, decreasing request IDs used for client prediction and prevents an
//! evicted creative request from being applied twice.
//!
//! Every changed occupied slot receives a fresh positive stack ID. Requests
//! operate on a candidate state: failure changes neither stacks nor ID allocation.
//! Request sequence/cache metadata still records a failed bounded request.

use std::collections::{BTreeMap, VecDeque};

use sculith_world::BlockKind;

pub const INVENTORY_SLOTS: usize = 36;
pub const MAX_STACK: u8 = 64;
pub const MAX_ACTIONS: usize = 32;
pub const REQUEST_HISTORY: usize = 64;

/// Catalogue identifiers are independent of both editions' item runtime IDs.
pub const CREATIVE_CATALOGUE: [BlockKind; 4] = [
    BlockKind::Stone,
    BlockKind::Dirt,
    BlockKind::Grass,
    BlockKind::Bedrock,
];

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ItemStack {
    pub kind: BlockKind,
    pub count: u8,
    pub net_id: i32,
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
    lowest_request_id: Option<i32>,
}

impl Default for Inventory {
    fn default() -> Self {
        let mut slots = [None; INVENTORY_SLOTS];
        for (index, kind) in CREATIVE_CATALOGUE[..3].iter().copied().enumerate() {
            slots[index] = Some(ItemStack {
                kind,
                count: MAX_STACK,
                net_id: index as i32 + 1,
            });
        }
        Self {
            state: State {
                slots,
                cursor: None,
                created_output: None,
                next_stack_id: 4,
            },
            history: VecDeque::with_capacity(REQUEST_HISTORY),
            lowest_request_id: None,
        }
    }
}

impl Inventory {
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
        if self.history.len() == REQUEST_HISTORY {
            self.history.pop_front();
        }
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
                    let after_from = state.stack(from.kind, from.count - count)?;
                    let after_to = state.stack(from.kind, total)?;
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
                    let after_from = to
                        .map(|stack| state.stack(stack.kind, stack.count))
                        .transpose()?
                        .flatten();
                    let after_to = from
                        .map(|stack| state.stack(stack.kind, stack.count))
                        .transpose()?
                        .flatten();
                    state.set(source.slot, after_from)?;
                    state.set(destination.slot, after_to)?;
                    changes.insert(source.slot, after_from);
                    changes.insert(destination.slot, after_to);
                }
                Action::Destroy { source, count } => {
                    valid_count(count)?;
                    let from = self
                        .validate_ref(source, request.id, state, &changes)?
                        .ok_or(RejectReason::EmptySource)?;
                    if count > from.count {
                        return Err(RejectReason::InvalidCount);
                    }
                    let after = state.stack(from.kind, from.count - count)?;
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
mod tests {
    use super::*;

    fn reference(slot: Slot, expected_id: i32) -> SlotRef {
        SlotRef { slot, expected_id }
    }

    fn exact(inventory: &Inventory, slot: Slot) -> SlotRef {
        reference(
            slot,
            inventory.get(slot).unwrap().map_or(0, |stack| stack.net_id),
        )
    }

    fn apply(inventory: &mut Inventory, id: i32, actions: Vec<Action>) -> Vec<Change> {
        inventory
            .apply(&Request { id, actions }, true)
            .result
            .unwrap()
    }

    fn transfer(source: SlotRef, destination: SlotRef, count: u8) -> Action {
        Action::Transfer {
            source,
            destination,
            count,
        }
    }

    #[test]
    fn split_merge_swap_and_destroy_preserve_counts_and_unique_ids() {
        let mut inventory = Inventory::default();
        assert_eq!(inventory.slots().len(), 36);
        let source = exact(&inventory, Slot::Inventory(0));
        apply(
            &mut inventory,
            -1,
            vec![transfer(source, reference(Slot::Cursor, 0), 32)],
        );
        assert_eq!(inventory.selected(0).unwrap().unwrap().count, 32);
        assert_eq!(inventory.cursor().unwrap().count, 32);
        let source = exact(&inventory, Slot::Cursor);
        let destination = exact(&inventory, Slot::Inventory(0));
        apply(&mut inventory, -2, vec![transfer(source, destination, 7)]);
        assert_eq!(inventory.selected(0).unwrap().unwrap().count, 39);
        assert_eq!(inventory.cursor().unwrap().count, 25);
        let source = exact(&inventory, Slot::Cursor);
        let destination = exact(&inventory, Slot::Inventory(1));
        apply(
            &mut inventory,
            -3,
            vec![Action::Swap {
                source,
                destination,
            }],
        );
        assert_eq!(inventory.cursor().unwrap().kind, BlockKind::Dirt);
        assert_eq!(
            inventory.get(Slot::Inventory(1)).unwrap().unwrap().count,
            25
        );
        let source = exact(&inventory, Slot::Cursor);
        apply(
            &mut inventory,
            -4,
            vec![Action::Destroy { source, count: 64 }],
        );
        assert_eq!(inventory.cursor(), None);
        let stacks: Vec<_> = inventory.slots().iter().flatten().collect();
        assert_eq!(
            stacks
                .iter()
                .map(|stack| u32::from(stack.count))
                .sum::<u32>(),
            128
        );
        let mut ids: Vec<_> = stacks.iter().map(|stack| stack.net_id).collect();
        assert!(ids.iter().all(|id| *id > 0));
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), stacks.len());
    }

    #[test]
    fn atomic_failure_restores_stacks_and_allocator() {
        let mut inventory = Inventory::default();
        let before = inventory.state.clone();
        let request = Request {
            id: -1,
            actions: vec![
                Action::CraftCreative { catalogue_id: 4 },
                transfer(
                    reference(Slot::CreatedOutput, -1),
                    reference(Slot::Inventory(9), 0),
                    64,
                ),
                Action::Destroy {
                    source: reference(Slot::Inventory(9), -1),
                    count: 65,
                },
            ],
        };
        assert_eq!(
            inventory.apply(&request, true).result,
            Err(RejectReason::InvalidCount)
        );
        assert_eq!(inventory.state, before);
        assert!(inventory.apply(&request, true).replayed);
        assert_eq!(inventory.state, before);
    }

    #[test]
    fn unsupported_adapter_action_is_an_atomic_cached_rejection() {
        let mut inventory = Inventory::default();
        let before = inventory.state.clone();
        let request = Request {
            id: -1,
            actions: vec![
                Action::CraftCreative { catalogue_id: 1 },
                Action::Unsupported,
            ],
        };
        assert_eq!(
            inventory.apply(&request, true).result,
            Err(RejectReason::UnsupportedAction)
        );
        assert_eq!(inventory.state, before);
        assert!(inventory.apply(&request, true).replayed);
        assert_eq!(inventory.state, before);
    }

    #[test]
    fn predictions_are_scoped_to_changed_slots_and_current_or_retained_request() {
        let mut inventory = Inventory::default();
        apply(
            &mut inventory,
            -1,
            vec![
                Action::CraftCreative { catalogue_id: 4 },
                transfer(
                    reference(Slot::CreatedOutput, -1),
                    reference(Slot::Cursor, 0),
                    64,
                ),
                transfer(
                    reference(Slot::Cursor, -1),
                    reference(Slot::Inventory(9), 0),
                    1,
                ),
            ],
        );
        assert_eq!(inventory.cursor().unwrap().count, 63);
        assert_eq!(
            inventory.get(Slot::Inventory(9)).unwrap().unwrap().kind,
            BlockKind::Bedrock
        );
        apply(
            &mut inventory,
            -2,
            vec![transfer(
                reference(Slot::Cursor, -1),
                reference(Slot::Inventory(9), -1),
                2,
            )],
        );
        assert_eq!(inventory.get(Slot::Inventory(9)).unwrap().unwrap().count, 3);
        let before = inventory.state.clone();
        let result = inventory.apply(
            &Request {
                id: -3,
                actions: vec![Action::Mine {
                    source: reference(Slot::Inventory(0), -1),
                }],
            },
            true,
        );
        assert_eq!(result.result, Err(RejectReason::MissingPrediction));
        assert_eq!(inventory.state, before);
        let result = inventory.apply(
            &Request {
                id: -4,
                actions: vec![transfer(
                    reference(Slot::Cursor, -1),
                    reference(Slot::Inventory(10), 0),
                    1,
                )],
            },
            true,
        );
        assert_eq!(result.result, Err(RejectReason::StackMismatch));
        assert_eq!(inventory.state, before);
    }

    #[test]
    fn stale_positive_empty_and_future_claims_reject() {
        let mut inventory = Inventory::default();
        for (id, expected_id, expected_error) in [
            (-1, 12345, RejectReason::StackMismatch),
            (-2, 0, RejectReason::StackMismatch),
            (-3, -4, RejectReason::MissingPrediction),
        ] {
            let before = inventory.state.clone();
            let response = inventory.apply(
                &Request {
                    id,
                    actions: vec![Action::Mine {
                        source: reference(Slot::Inventory(0), expected_id),
                    }],
                },
                true,
            );
            assert_eq!(response.result, Err(expected_error));
            assert_eq!(inventory.state, before);
        }
    }

    #[test]
    fn request_replay_is_equal_bounded_and_never_fresh_after_eviction() {
        let mut inventory = Inventory::default();
        let first = Request {
            id: -1,
            actions: vec![Action::CraftCreative { catalogue_id: 1 }],
        };
        let response = inventory.apply(&first, true);
        let state = inventory.state.clone();
        let replay = inventory.apply(&first, true);
        assert!(replay.replayed);
        assert_eq!(replay.result, response.result);
        assert_eq!(inventory.state, state);
        let conflict = Request {
            id: -1,
            actions: vec![Action::CraftCreative { catalogue_id: 2 }],
        };
        assert_eq!(
            inventory.apply(&conflict, true).result,
            Err(RejectReason::RequestIdConflict)
        );
        for sequence in 2..=(REQUEST_HISTORY as i32 + 1) {
            let id = -sequence;
            let source = exact(&inventory, Slot::Inventory(0));
            apply(&mut inventory, id, vec![Action::Mine { source }]);
        }
        assert_eq!(inventory.history.len(), REQUEST_HISTORY);
        let state = inventory.state.clone();
        assert_eq!(
            inventory.apply(&first, true).result,
            Err(RejectReason::StaleRequest)
        );
        assert_eq!(inventory.state, state);
    }

    #[test]
    fn rejects_invalid_catalogue_count_slot_and_permissions() {
        let cases = [
            (
                Action::CraftCreative { catalogue_id: 0 },
                RejectReason::InvalidCatalogue,
            ),
            (
                Action::CraftCreative { catalogue_id: 5 },
                RejectReason::InvalidCatalogue,
            ),
            (
                Action::Destroy {
                    source: reference(Slot::Inventory(0), 1),
                    count: 0,
                },
                RejectReason::InvalidCount,
            ),
            (
                transfer(
                    reference(Slot::Inventory(36), 0),
                    reference(Slot::Cursor, 0),
                    1,
                ),
                RejectReason::InvalidSlot,
            ),
            (
                transfer(
                    reference(Slot::Inventory(0), 1),
                    reference(Slot::Inventory(0), 1),
                    1,
                ),
                RejectReason::SameSlot,
            ),
            (
                transfer(
                    reference(Slot::Inventory(0), 1),
                    reference(Slot::Inventory(1), 2),
                    1,
                ),
                RejectReason::IncompatibleStack,
            ),
            (
                transfer(
                    reference(Slot::Inventory(0), 1),
                    reference(Slot::CreatedOutput, 0),
                    1,
                ),
                RejectReason::InvalidDestination,
            ),
            (
                Action::Mine {
                    source: reference(Slot::Inventory(9), 0),
                },
                RejectReason::InvalidMiningSlot,
            ),
        ];
        for (action, error) in cases {
            let mut inventory = Inventory::default();
            let before = inventory.state.clone();
            assert_eq!(
                inventory
                    .apply(
                        &Request {
                            id: -1,
                            actions: vec![action]
                        },
                        true
                    )
                    .result,
                Err(error)
            );
            assert_eq!(inventory.state, before);
        }
        let mut inventory = Inventory::default();
        let before = inventory.state.clone();
        assert_eq!(
            inventory
                .apply(
                    &Request {
                        id: -1,
                        actions: vec![Action::CraftCreative { catalogue_id: 1 }]
                    },
                    false
                )
                .result,
            Err(RejectReason::Unauthorized)
        );
        assert_eq!(inventory.state, before);
    }

    #[test]
    fn merge_overflow_and_excessive_removal_are_atomic() {
        let mut inventory = Inventory::default();
        inventory
            .replace_creative(9, Some(BlockKind::Stone), 1, true)
            .unwrap();
        let before = inventory.state.clone();
        let source = exact(&inventory, Slot::Inventory(9));
        let destination = exact(&inventory, Slot::Inventory(0));
        assert_eq!(
            inventory
                .apply(
                    &Request {
                        id: -1,
                        actions: vec![transfer(source, destination, 1)]
                    },
                    true
                )
                .result,
            Err(RejectReason::StackOverflow)
        );
        assert_eq!(inventory.state, before);
        assert_eq!(
            inventory
                .apply(
                    &Request {
                        id: -2,
                        actions: vec![Action::Destroy { source, count: 2 }]
                    },
                    true
                )
                .result,
            Err(RejectReason::InvalidCount)
        );
        assert_eq!(inventory.state, before);
    }

    #[test]
    fn creative_replacement_and_removal_enforce_the_same_authority_and_allowlist() {
        let mut inventory = Inventory::default();
        inventory
            .replace_creative(35, Some(BlockKind::Bedrock), 17, true)
            .unwrap();
        assert_eq!(
            inventory.get(Slot::Inventory(35)).unwrap().unwrap().count,
            17
        );
        let before = inventory.state.clone();
        for (slot, kind, count, authorized) in [
            (0, Some(BlockKind::Stone), 1, false),
            (36, Some(BlockKind::Stone), 1, true),
            (0, Some(BlockKind::Air), 1, true),
            (0, Some(BlockKind::Stone), 0, true),
            (0, Some(BlockKind::Stone), 65, true),
            (0, None, 1, true),
        ] {
            assert!(
                inventory
                    .replace_creative(slot, kind, count, authorized)
                    .is_err()
            );
            assert_eq!(inventory.state, before);
        }
        inventory.replace_creative(35, None, 0, true).unwrap();
        assert_eq!(inventory.get(Slot::Inventory(35)).unwrap(), None);
        assert!(
            inventory
                .replace_creative(35, None, 0, true)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn last_positive_id_is_usable_and_exhaustion_rolls_back_partial_allocation() {
        let mut inventory = Inventory::default();
        inventory.state.next_stack_id = i32::MAX as u64;
        let source = exact(&inventory, Slot::Inventory(0));
        let before = inventory.state.clone();
        let request = Request {
            id: -1,
            actions: vec![transfer(source, reference(Slot::Cursor, 0), 1)],
        };
        assert_eq!(
            inventory.apply(&request, true).result,
            Err(RejectReason::StackIdExhausted)
        );
        assert_eq!(inventory.state, before);
        apply(
            &mut inventory,
            -2,
            vec![Action::CraftCreative { catalogue_id: 4 }],
        );
        assert_eq!(inventory.created_output().unwrap().net_id, i32::MAX);
        let before = inventory.state.clone();
        assert!(
            inventory
                .replace_creative(9, Some(BlockKind::Stone), 1, true)
                .is_err()
        );
        assert_eq!(inventory.state, before);
    }

    #[test]
    fn action_and_sequence_limits_do_not_wrap_or_clone_oversized_requests() {
        let mut inventory = Inventory::default();
        let before = inventory.state.clone();
        let action = Action::CraftCreative { catalogue_id: 1 };
        let request = Request {
            id: -1,
            actions: vec![action; MAX_ACTIONS + 1],
        };
        assert_eq!(
            inventory.apply(&request, true).result,
            Err(RejectReason::TooManyActions)
        );
        assert!(inventory.history.is_empty());
        assert_eq!(inventory.state, before);
        for id in [0, 1] {
            assert_eq!(
                inventory
                    .apply(
                        &Request {
                            id,
                            actions: vec![]
                        },
                        true
                    )
                    .result,
                Err(RejectReason::InvalidRequestId)
            );
        }
        let source = exact(&inventory, Slot::Inventory(0));
        apply(&mut inventory, i32::MIN, vec![Action::Mine { source }]);
        assert_eq!(
            inventory
                .apply(
                    &Request {
                        id: -2,
                        actions: vec![]
                    },
                    true
                )
                .result,
            Err(RejectReason::StaleRequest)
        );
    }

    #[test]
    fn pickup_clicks_split_place_merge_and_swap_from_owner_counts() {
        let mut inventory = Inventory::default();
        inventory
            .replace_creative(9, Some(BlockKind::Stone), 5, true)
            .unwrap();
        inventory
            .click(
                Click::Pickup {
                    slot: 9,
                    right: true,
                },
                true,
            )
            .unwrap();
        assert_eq!(inventory.cursor().unwrap().count, 3);
        assert_eq!(inventory.get(Slot::Inventory(9)).unwrap().unwrap().count, 2);
        inventory
            .click(
                Click::Pickup {
                    slot: 10,
                    right: true,
                },
                true,
            )
            .unwrap();
        assert_eq!(inventory.cursor().unwrap().count, 2);
        assert_eq!(
            inventory.get(Slot::Inventory(10)).unwrap().unwrap().count,
            1
        );
        inventory
            .click(
                Click::Pickup {
                    slot: 9,
                    right: false,
                },
                true,
            )
            .unwrap();
        assert_eq!(inventory.cursor(), None);
        assert_eq!(inventory.get(Slot::Inventory(9)).unwrap().unwrap().count, 4);
        inventory
            .click(
                Click::Pickup {
                    slot: 9,
                    right: false,
                },
                true,
            )
            .unwrap();
        assert_eq!(inventory.get(Slot::Inventory(9)).unwrap(), None);
        assert_eq!(inventory.cursor().unwrap().count, 4);
        inventory
            .click(
                Click::Pickup {
                    slot: 1,
                    right: true,
                },
                true,
            )
            .unwrap();
        assert_eq!(inventory.cursor().unwrap().kind, BlockKind::Dirt);
        assert_eq!(inventory.get(Slot::Inventory(1)).unwrap().unwrap().count, 4);
        inventory
            .click(
                Click::Pickup {
                    slot: 11,
                    right: false,
                },
                true,
            )
            .unwrap();
        assert_eq!(inventory.cursor(), None);
        assert_eq!(
            inventory.get(Slot::Inventory(11)).unwrap().unwrap().count,
            64
        );
        // No click consumes or creates items, including incompatible right-click swaps.
        assert_eq!(
            inventory
                .slots()
                .iter()
                .flatten()
                .map(|stack| u32::from(stack.count))
                .sum::<u32>(),
            197
        );
        assert_eq!(inventory.lowest_request_id, None);
        assert!(inventory.history.is_empty());
    }

    #[test]
    fn pickup_full_empty_and_capacity_limits_have_exact_noop_behavior() {
        let mut inventory = Inventory::default();
        let before = inventory.clone();
        assert!(
            inventory
                .click(
                    Click::Pickup {
                        slot: 9,
                        right: false
                    },
                    true
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(inventory, before);
        inventory
            .click(
                Click::Pickup {
                    slot: 0,
                    right: true,
                },
                true,
            )
            .unwrap();
        inventory
            .replace_creative(9, Some(BlockKind::Stone), 63, true)
            .unwrap();
        inventory
            .click(
                Click::Pickup {
                    slot: 9,
                    right: false,
                },
                true,
            )
            .unwrap();
        assert_eq!(
            inventory.get(Slot::Inventory(9)).unwrap().unwrap().count,
            64
        );
        assert_eq!(inventory.cursor().unwrap().count, 31);
        let before = inventory.clone();
        for right in [false, true] {
            assert!(
                inventory
                    .click(Click::Pickup { slot: 9, right }, true)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(inventory, before);
        }
        inventory
            .click(
                Click::Pickup {
                    slot: 10,
                    right: false,
                },
                true,
            )
            .unwrap();
        assert_eq!(inventory.cursor(), None);
        assert_eq!(
            inventory.get(Slot::Inventory(10)).unwrap().unwrap().count,
            31
        );
    }

    #[test]
    fn hotbar_swap_self_empty_and_bounds_preserve_authority() {
        let mut inventory = Inventory::default();
        let before = inventory.clone();
        assert!(
            inventory
                .click(Click::HotbarSwap { slot: 0, hotbar: 0 }, true)
                .unwrap()
                .is_empty()
        );
        assert_eq!(inventory, before);
        assert!(
            inventory
                .click(Click::HotbarSwap { slot: 9, hotbar: 8 }, true)
                .unwrap()
                .is_empty()
        );
        assert_eq!(inventory, before);
        inventory
            .click(Click::HotbarSwap { slot: 9, hotbar: 1 }, true)
            .unwrap();
        assert_eq!(inventory.get(Slot::Inventory(1)).unwrap(), None);
        assert_eq!(
            inventory.get(Slot::Inventory(9)).unwrap().unwrap().kind,
            BlockKind::Dirt
        );
        assert_eq!(inventory.cursor(), None);
        for click in [
            Click::Pickup {
                slot: 36,
                right: false,
            },
            Click::HotbarSwap {
                slot: 36,
                hotbar: 0,
            },
            Click::HotbarSwap { slot: 9, hotbar: 9 },
        ] {
            let before = inventory.clone();
            assert!(inventory.click(click, true).is_err());
            assert_eq!(inventory, before);
        }
        let before = inventory.clone();
        assert!(
            inventory
                .click(
                    Click::Pickup {
                        slot: 9,
                        right: false
                    },
                    false
                )
                .is_err()
        );
        assert_eq!(inventory, before);
    }

    #[test]
    fn click_stack_id_exhaustion_rolls_back_and_fresh_ids_are_unique() {
        let mut inventory = Inventory::default();
        inventory.state.next_stack_id = i32::MAX as u64;
        let before = inventory.clone();
        assert!(
            inventory
                .click(
                    Click::Pickup {
                        slot: 0,
                        right: true
                    },
                    true
                )
                .is_err()
        );
        assert_eq!(inventory, before);
        inventory.state.next_stack_id = 4;
        let old_ids: Vec<_> = inventory
            .slots()
            .iter()
            .flatten()
            .map(|stack| stack.net_id)
            .collect();
        let changes = inventory
            .click(
                Click::Pickup {
                    slot: 0,
                    right: true,
                },
                true,
            )
            .unwrap();
        assert!(
            changes
                .iter()
                .filter_map(|change| change.stack)
                .all(|stack| stack.net_id > 0 && !old_ids.contains(&stack.net_id))
        );
        let slot_id = inventory.selected(0).unwrap().unwrap().net_id;
        assert_ne!(inventory.cursor().unwrap().net_id, slot_id);
    }
}
