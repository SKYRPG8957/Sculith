//! Player inventory implementation.
//!
//! This module implements the player's inventory, which consists of:
//! - 36 main inventory slots (3 rows of 9 + hotbar)
//! - Equipment slots (armor + off-hand)
//!
//! The first 9 slots of the main inventory are the hotbar (accessible with number keys).
//! Slots 0-35 are the main inventory, with slot 40 being the off-hand slot.

use crate::entity_equipment::EntityEquipment;
use crate::screen_handler::InventoryPlayer;

use crate::inventory::{Clearable, Inventory};
use pumpkin_data::data_component_impl::EquipmentSlot;
use pumpkin_data::item::Item;
use pumpkin_data::item_stack::ItemStack;
use rustc_hash::FxHashMap;
use sculith_protocol::java::client::play::CSetPlayerInventory;
use sculith_util::Hand;
use std::any::Any;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tracing::warn;

/// The player's inventory.
///
/// Contains 36 main inventory slots (hotbar + main storage) plus
/// equipment slots accessed through [`EntityEquipment`].
pub struct PlayerInventory {
    /// The 36 main inventory slots (slots 0-35).
    ///
    /// The first 9 slots (0-8) are the hotbar, the remaining 27 (9-35) are the main storage.
    pub main_inventory: RwLock<[ItemStack; Self::MAIN_SIZE]>,
    /// Mapping of slot indices to equipment slot types.
    ///
    /// Used to identify which slots correspond to armor and off-hand equipment.
    pub equipment_slots: Arc<FxHashMap<usize, EquipmentSlot>>,
    /// The currently selected hotbar slot index (0-8).
    pub selected_slot: AtomicU8,
    /// The entity equipment storage for armor and off-hand items.
    ///
    /// This is separate from the main inventory and is rendered on the player model.
    pub entity_equipment: Arc<Mutex<EntityEquipment>>,
}

/// A player inventory change prepared for a still-unmodified dropped stack.
pub struct PlayerPickup {
    source: ItemStack,
    count: u8,
    selected: u8,
    slots: Vec<PickupSlot>,
}

struct PickupSlot {
    index: usize,
    before: ItemStack,
    after: ItemStack,
}

impl PlayerPickup {
    #[must_use]
    pub const fn count(&self) -> u8 {
        self.count
    }

    #[must_use]
    pub const fn item(&self) -> &'static Item {
        self.source.item
    }

    /// Complete transfer into one slot that does not change the held equipment.
    #[must_use]
    pub fn single_storage_update(&self) -> Option<(usize, &ItemStack)> {
        let mut updates = self.full_storage_updates()?;
        if updates.len() != 1 {
            return None;
        }
        updates.next()
    }

    /// All destination slots of a complete transfer that leaves held equipment unchanged.
    #[must_use]
    pub fn full_storage_updates(
        &self,
    ) -> Option<impl ExactSizeIterator<Item = (usize, &ItemStack)>> {
        if self.count != self.source.item_count {
            return None;
        }
        self.storage_updates()
    }

    /// Destination slots for a transfer that leaves held equipment unchanged.
    #[must_use]
    pub fn storage_updates(&self) -> Option<impl ExactSizeIterator<Item = (usize, &ItemStack)>> {
        self.slots
            .iter()
            .all(|slot| {
                slot.index < PlayerInventory::MAIN_SIZE && slot.index != usize::from(self.selected)
            })
            .then(|| self.slots.iter().map(|slot| (slot.index, &slot.after)))
    }
}

impl PlayerInventory {
    /// Size of the main inventory (36 slots: 27 storage + 9 hotbar).
    pub const MAIN_SIZE: usize = 36;
    /// Size of the hotbar (9 slots).
    pub const HOTBAR_SIZE: usize = 9;
    /// Slot index for the off-hand (40).
    pub const OFF_HAND_SLOT: usize = 40;

    /// Creates a new player inventory.
    ///
    /// # Arguments
    /// - `entity_equipment` - The entity equipment storage for armor/off-hand
    /// - `equipment_slots` - Mapping of slot indices to equipment slots
    // TODO: Add inventory load from nbt
    pub fn new(
        entity_equipment: Arc<Mutex<EntityEquipment>>,
        equipment_slots: Arc<FxHashMap<usize, EquipmentSlot>>,
    ) -> Self {
        Self {
            main_inventory: RwLock::new(std::array::from_fn(|_| ItemStack::EMPTY.clone())),
            equipment_slots,
            selected_slot: AtomicU8::new(0),
            entity_equipment,
        }
    }

    /// Fast non-blocking count of an item across all main inventory slots.
    pub fn count_item(&self, item: &'static Item) -> u32 {
        let mut total = 0u32;
        if let Ok(inv) = self.main_inventory.try_read() {
            for stack in inv.iter() {
                if stack.get_item().id == item.id {
                    total += u32::from(stack.item_count);
                }
            }
        }
        total
    }

    /// Fast non-blocking check if the main inventory contains a given item.
    pub fn contains_item(&self, item: &'static Item) -> bool {
        if let Ok(inv) = self.main_inventory.try_read() {
            for stack in inv.iter() {
                if !stack.is_empty() && stack.get_item().id == item.id {
                    return true;
                }
            }
        }
        false
    }

    /// Gets the item in the currently selected hotbar slot.
    ///
    /// This is the item the player is currently holding in their main hand.
    pub fn held_item(&self) -> ItemStack {
        let inv = self
            .main_inventory
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inv[self.get_selected_slot() as usize].clone()
    }

    /// Sets the item in the currently selected hotbar slot.
    pub fn set_held_item(&self, stack: ItemStack) {
        let selected = self.get_selected_slot() as usize;
        let mut inv = self
            .main_inventory
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inv[selected] = stack;
    }

    /// Sets the item in the specified hand.
    pub fn set_stack_in_hand(&self, hand: Hand, stack: ItemStack) {
        match hand {
            Hand::Right => self.set_held_item(stack),
            Hand::Left => {
                let Some(slot) = self.equipment_slots.get(&Self::OFF_HAND_SLOT) else {
                    return;
                };
                self.entity_equipment
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .put(slot, stack);
            }
        }
    }

    /// Gets the item in the specified hand.
    ///
    /// # Arguments
    /// - `hand` - Which hand to get the item from
    pub fn get_stack_in_hand(&self, hand: Hand) -> ItemStack {
        match hand {
            Hand::Left => self.off_hand_item(),
            Hand::Right => self.held_item(),
        }
    }

    /// Gets the item in the off-hand.
    ///
    /// Mojang name: `getOffHandStack`
    pub fn off_hand_item(&self) -> ItemStack {
        let Some(slot) = self.equipment_slots.get(&Self::OFF_HAND_SLOT) else {
            return ItemStack::EMPTY.clone();
        };
        self.entity_equipment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(slot)
    }

    /// Swaps the items between main hand and off-hand.
    ///
    /// # Returns
    /// The new main hand item and new off-hand item.
    pub fn swap_item(&self) -> (ItemStack, ItemStack) {
        let Some(slot) = self.equipment_slots.get(&Self::OFF_HAND_SLOT) else {
            return (ItemStack::EMPTY.clone(), ItemStack::EMPTY.clone());
        };
        let mut equipment = self
            .entity_equipment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let selected = self.get_selected_slot() as usize;
        let mut main_inv = self
            .main_inventory
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let main_hand_item = main_inv[selected].clone();
        let new_main = equipment.put(slot, main_hand_item.clone());
        main_inv[selected] = new_main.clone();
        (new_main, main_hand_item)
    }

    /// Checks if a slot index is a valid hotbar slot.
    #[must_use]
    pub const fn is_valid_hotbar_index(slot: usize) -> bool {
        slot < Self::HOTBAR_SIZE
    }

    /// Adds a stack to any available slot, prioritizing stacking with existing items.
    fn add_stack(&self, stack: ItemStack) -> usize {
        let mut slot_index = self.get_occupied_slot_with_room_for_stack(&stack);

        if slot_index == -1 {
            slot_index = self.get_empty_slot();
        }

        if slot_index == -1 {
            stack.item_count as usize
        } else {
            self.add_stack_to_slot(slot_index as usize, stack)
        }
    }

    /// Adds a stack to a specific slot.
    ///
    /// Returns the number of items that couldn't fit.
    fn add_stack_to_slot(&self, slot: usize, stack: ItemStack) -> usize {
        if slot >= Self::MAIN_SIZE {
            if let Some(slot_type) = self.equipment_slots.get(&slot) {
                let mut equipment = self
                    .entity_equipment
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut current = equipment.get(slot_type);
                if current.is_empty() {
                    let max_stack = stack.get_max_stack_size();
                    if stack.item_count <= max_stack {
                        equipment.put(slot_type, stack);
                        return 0;
                    }

                    equipment.put(slot_type, stack.copy_with_count(max_stack));

                    return (stack.item_count - max_stack) as usize;
                } else if matches!(slot_type, EquipmentSlot::OffHand(_))
                    && current.are_items_and_components_equal(&stack)
                {
                    let count_left = current
                        .get_max_stack_size()
                        .saturating_sub(current.item_count);
                    let count_min = stack.item_count.min(count_left);
                    if count_min != 0 {
                        current.increment(count_min);
                        equipment.put(slot_type, current);
                    }
                    return (stack.item_count - count_min) as usize;
                }
            }
            return stack.item_count as usize;
        }

        let mut inv = self
            .main_inventory
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::add_stack_to_main_slot(&mut inv[slot], &stack)
    }

    fn add_stack_to_main_slot(self_stack: &mut ItemStack, stack: &ItemStack) -> usize {
        let mut stack_count = stack.item_count;
        if self_stack.is_empty() {
            *self_stack = stack.copy_with_count(0);
        }

        let count_left = self_stack.get_max_stack_size() - self_stack.item_count;
        let count_min = stack_count.min(count_left);

        if count_min != 0 {
            stack_count -= count_min;
            self_stack.increment(count_min);
        }
        stack_count as usize
    }

    /// Finds an empty slot in the inventory.
    ///
    /// # Returns
    /// The slot index or -1 if inventory is full.
    fn get_empty_slot(&self) -> i16 {
        let inv = self
            .main_inventory
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inv.iter()
            .position(ItemStack::is_empty)
            .map_or(-1, |i| i as i16)
    }

    /// Checks if a stack can be added to an existing stack.
    fn can_stack_add_more(existing_stack: &ItemStack, stack: &ItemStack) -> bool {
        !existing_stack.is_empty()
            && existing_stack.are_items_and_components_equal(stack)
            && existing_stack.is_stackable()
            && existing_stack.item_count < existing_stack.get_max_stack_size()
    }

    /// Finds a slot with the same item type that has room for more items.
    ///
    /// Checks selected slot, off-hand, then other slots.
    fn get_occupied_slot_with_room_for_stack(&self, stack: &ItemStack) -> i16 {
        let selected = self.get_selected_slot() as usize;
        let inv = self
            .main_inventory
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::find_merge_slot(&inv, selected, stack, || {
            Self::can_stack_add_more(&self.off_hand_item(), stack)
        })
        .map_or(-1, |slot| slot as i16)
    }

    fn find_merge_slot(
        main: &[ItemStack; Self::MAIN_SIZE],
        selected: usize,
        stack: &ItemStack,
        off_hand_has_room: impl FnOnce() -> bool,
    ) -> Option<usize> {
        if Self::can_stack_add_more(&main[selected], stack) {
            return Some(selected);
        }
        if off_hand_has_room() {
            return Some(Self::OFF_HAND_SLOT);
        }
        main.iter()
            .position(|item| Self::can_stack_add_more(item, stack))
    }

    /// Inserts a stack into any available slot.
    ///
    /// # Arguments
    /// - `stack` - The stack to insert (modified in place)
    ///
    /// # Returns
    /// `true` if any items were inserted, `false` otherwise.
    pub fn insert_stack_anywhere(&self, stack: &mut ItemStack) -> bool {
        self.insert_stack(-1, stack)
    }

    /// Stages only the slots that can accept part of a dropped stack.
    #[must_use]
    pub fn plan_pickup(&self, source: &ItemStack) -> Option<PlayerPickup> {
        if source.is_empty() {
            return None;
        }
        let main = self.main_inventory.try_read().ok()?;
        let equipment = self.entity_equipment.try_lock().ok()?;
        let selected = self.get_selected_slot();
        let selected_index = usize::from(selected);
        let off_hand = self
            .equipment_slots
            .get(&Self::OFF_HAND_SLOT)
            .and_then(|slot| equipment.equipment.get(slot));
        let merges = std::iter::once((selected_index, main.get(selected_index)?))
            .chain(off_hand.map(|stack| (Self::OFF_HAND_SLOT, stack)))
            .chain(
                main.iter()
                    .enumerate()
                    .filter(|(slot, _)| *slot != selected_index),
            )
            .filter(|(_, stack)| Self::can_stack_add_more(stack, source));
        let empty = main
            .iter()
            .enumerate()
            .filter(|(_, stack)| stack.is_empty());
        let mut count = 0;
        let mut slots = Vec::new();
        for (index, before) in merges.chain(empty) {
            let capacity = if before.is_empty() {
                source.get_max_stack_size()
            } else {
                before
                    .get_max_stack_size()
                    .saturating_sub(before.item_count)
            };
            let accepted = capacity.min(source.item_count - count);
            if accepted == 0 {
                continue;
            }
            let after = if before.is_empty() {
                source.copy_with_count(accepted)
            } else {
                let mut after = before.clone();
                after.increment(accepted);
                after
            };
            slots.push(PickupSlot {
                index,
                before: before.clone(),
                after,
            });
            count += accepted;
            if count == source.item_count {
                break;
            }
        }
        (count > 0).then(|| PlayerPickup {
            source: source.clone(),
            count,
            selected,
            slots,
        })
    }

    /// The item owner holds the source lock. It approves the transfer after the
    /// inventory and source checks, while all affected stacks are pinned.
    pub fn commit_pickup(
        &self,
        pickup: PlayerPickup,
        source: &mut ItemStack,
        approve: impl FnOnce() -> bool,
    ) -> bool {
        let Ok(mut main) = self.main_inventory.try_write() else {
            return false;
        };
        let Ok(mut equipment) = self.entity_equipment.try_lock() else {
            return false;
        };
        let off_hand = self.equipment_slots.get(&Self::OFF_HAND_SLOT);
        let equipped = off_hand
            .and_then(|slot| equipment.equipment.get(slot))
            .unwrap_or(ItemStack::EMPTY);
        if self.get_selected_slot() != pickup.selected
            || source.uid != pickup.source.uid
            || !source.are_equal(&pickup.source)
            || pickup.slots.iter().any(|slot| {
                let current = main.get(slot.index).unwrap_or(equipped);
                current.uid != slot.before.uid || !current.are_equal(&slot.before)
            })
            || !approve()
        {
            return false;
        }
        for slot in pickup.slots {
            if let Some(destination) = main.get_mut(slot.index) {
                *destination = slot.after;
            } else if let Some(off_hand) = off_hand {
                equipment.put(off_hand, slot.after);
            }
        }
        source.decrement(pickup.count);
        true
    }

    /// Inserts a stack into a specific slot or any slot.
    ///
    /// # Arguments
    /// - `slot` - The slot index, or -1 for any slot
    /// - `stack` - The stack to insert (modified in place)
    ///
    /// # Returns
    /// `true` if any items were inserted, `false` otherwise.
    pub fn insert_stack(&self, slot: i16, stack: &mut ItemStack) -> bool {
        if stack.is_empty() {
            return false;
        }

        let mut i;
        let start_count = stack.item_count;

        loop {
            i = stack.item_count;
            if slot == -1 {
                stack.set_count(self.add_stack(stack.clone()) as u8);
            } else {
                stack.set_count(self.add_stack_to_slot(slot as usize, stack.clone()) as u8);
            }

            if stack.is_empty() || stack.item_count >= i {
                break;
            }
        }

        stack.item_count < start_count
    }

    /// Finds the first slot containing a matching stack.
    ///
    /// # Returns
    /// The slot index or -1 if not found.
    pub fn get_slot_with_stack(&self, stack: &ItemStack) -> i16 {
        let inv = self
            .main_inventory
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (i, item) in inv.iter().enumerate() {
            if !item.is_empty() && item.are_items_and_components_equal(stack) {
                return i as i16;
            }
        }
        -1
    }

    /// Finds an empty hotbar slot to swap an item to.
    ///
    /// First looks for empty slots, then slots without enchantments.
    fn get_swappable_hotbar_slot(&self) -> usize {
        let inv = self
            .main_inventory
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let selected_slot = self.get_selected_slot() as usize;
        for i in 0..Self::HOTBAR_SIZE {
            let check_index = (i + selected_slot) % 9;
            if inv[check_index].is_empty() {
                return check_index;
            }
        }

        selected_slot
    }

    /// Swaps an item stack with an item on the hotbar.
    ///
    /// Finds an empty hotbar slot and places the stack there.
    pub fn swap_stack_with_hotbar(&self, stack: ItemStack) {
        let swappable = self.get_swappable_hotbar_slot();
        self.set_selected_slot(swappable as u8);
        let selected = self.get_selected_slot() as usize;
        let mut inv = self
            .main_inventory
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(empty_slot) = inv.iter().position(ItemStack::is_empty)
            && !inv[selected].is_empty()
        {
            inv[empty_slot] = inv[selected].clone();
        }

        inv[selected] = stack;
    }

    /// Swaps the items at two slot indices.
    pub fn swap_slot_with_hotbar(&self, slot: usize) {
        let swappable = self.get_swappable_hotbar_slot();
        self.set_selected_slot(swappable as u8);
        let selected = self.get_selected_slot() as usize;
        let mut inv = self
            .main_inventory
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inv.swap(selected, slot);
    }

    /// Gets the item in the specified slot (synchronously).
    pub fn get_slot(&self, slot: usize) -> ItemStack {
        if slot < Self::MAIN_SIZE {
            let inv = self
                .main_inventory
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inv[slot].clone()
        } else if let Some(slot_type) = self.equipment_slots.get(&slot) {
            self.entity_equipment
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(slot_type)
        } else {
            ItemStack::EMPTY.clone()
        }
    }

    /// Sets the item in the specified slot (synchronously).
    pub fn set_slot(&self, slot: usize, stack: ItemStack) {
        if slot < Self::MAIN_SIZE {
            let mut inv = self
                .main_inventory
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inv[slot] = stack;
        } else if let Some(slot_type) = self.equipment_slots.get(&slot) {
            self.entity_equipment
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .put(slot_type, stack);
        }
    }

    /// Replaces a slot only when its logical contents match the supplied snapshot.
    #[must_use]
    pub fn replace_slot_if_equal(
        &self,
        slot: usize,
        expected: &ItemStack,
        replacement: ItemStack,
    ) -> bool {
        self.replace_matching_slot(slot, replacement, |current| {
            if expected.is_empty() {
                current.is_empty()
            } else {
                current.are_equal(expected)
            }
        })
    }

    /// Replaces a slot if it still contains the same stack with the same contents.
    #[must_use]
    pub fn replace_slot_if_unchanged(
        &self,
        slot: usize,
        expected: &ItemStack,
        replacement: ItemStack,
    ) -> bool {
        self.replace_matching_slot(slot, replacement, |current| {
            current.uid == expected.uid && current.are_equal(expected)
        })
    }

    fn replace_matching_slot(
        &self,
        slot: usize,
        replacement: ItemStack,
        matches: impl FnOnce(&ItemStack) -> bool,
    ) -> bool {
        if slot < Self::MAIN_SIZE {
            let mut inventory = self
                .main_inventory
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let current = &mut inventory[slot];
            if matches(current) {
                *current = replacement;
                return true;
            }
        } else if let Some(slot) = self.equipment_slots.get(&slot) {
            let mut equipment = self
                .entity_equipment
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches(&equipment.get(slot)) {
                equipment.put(slot, replacement);
                return true;
            }
        }
        false
    }

    /// Gives a stack to the player or drops it if inventory is full.
    pub fn offer_or_drop_stack(&self, stack: ItemStack, player: &dyn InventoryPlayer) {
        self.offer(stack, true, player);
    }

    /// Gives a stack to the player, optionally notifying the client.
    ///
    /// # Arguments
    /// - `stack` - The stack to give
    /// - `notify_client` - Whether to send inventory update packets
    /// - `player` - The player to give the stack to
    pub fn offer(&self, stack: ItemStack, notify_client: bool, player: &dyn InventoryPlayer) {
        let mut stack = stack;
        self.offer_to_slots(&mut stack, |slot| {
            if notify_client {
                player.enqueue_slot_set_packet(&CSetPlayerInventory::new(
                    i32::from(slot).into(),
                    &self.get_stack(slot as usize).into(),
                ));
            }
        });
        if !stack.is_empty() {
            player.drop_item(stack, false);
        }
    }

    /// Inserts as much as fits and reports each slot that changed.
    pub(crate) fn offer_to_slots(&self, stack: &mut ItemStack, mut changed: impl FnMut(i16)) {
        while !stack.is_empty() {
            let mut room_for_stack = self.get_occupied_slot_with_room_for_stack(stack);
            if room_for_stack == -1 {
                room_for_stack = self.get_empty_slot();
            }

            if room_for_stack == -1 {
                break;
            }

            let items_fit =
                stack.get_max_stack_size() - self.get_stack(room_for_stack as usize).item_count;
            if self.insert_stack(room_for_stack, &mut stack.split(items_fit)) {
                changed(room_for_stack);
            }
        }
    }

    /// Plans complete offers in order with the usual slot priority, leaving held slots to the
    /// caller's equipment path. Failure leaves the source stacks and inventory untouched.
    #[must_use]
    pub fn plan_offers_to_storage<'a>(
        main: &[ItemStack; Self::MAIN_SIZE],
        selected_slot: u8,
        off_hand: &ItemStack,
        stacks: impl IntoIterator<Item = &'a ItemStack>,
    ) -> Option<Vec<(usize, ItemStack)>> {
        let selected = usize::from(selected_slot);
        if !Self::is_valid_hotbar_index(selected) {
            return None;
        }
        let mut after = main.clone();
        let mut changes = Vec::new();
        for mut remaining in stacks
            .into_iter()
            .filter(|stack| !stack.is_empty())
            .cloned()
        {
            while !remaining.is_empty() {
                let index = Self::find_merge_slot(&after, selected, &remaining, || {
                    Self::can_stack_add_more(off_hand, &remaining)
                })
                .or_else(|| after.iter().position(ItemStack::is_empty))?;
                if index == selected {
                    return None;
                }
                let target = after.get_mut(index)?;
                let fits = remaining
                    .get_max_stack_size()
                    .checked_sub(target.item_count)?;
                if fits == 0 || Self::add_stack_to_main_slot(target, &remaining.split(fits)) != 0 {
                    return None;
                }
                changes.push((index, target.clone()));
            }
        }
        Some(changes)
    }
}

impl Clearable for PlayerInventory {
    fn clear(&self) {
        let mut inv = self
            .main_inventory
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inv.fill_with(|| ItemStack::EMPTY.clone());
        self.entity_equipment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

impl Inventory for PlayerInventory {
    fn size(&self) -> usize {
        Self::MAIN_SIZE + self.equipment_slots.len()
    }

    fn is_empty(&self) -> bool {
        let inv = self
            .main_inventory
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inv.iter().any(|s| !s.is_empty()) {
            return false;
        }

        for slot in self.equipment_slots.values() {
            let eq_item = self
                .entity_equipment
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(slot);
            if !eq_item.is_empty() {
                return false;
            }
        }

        true
    }

    fn get_stack(&self, slot: usize) -> ItemStack {
        if slot < Self::MAIN_SIZE {
            let inv = self
                .main_inventory
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inv[slot].clone()
        } else if let Some(slot) = self.equipment_slots.get(&slot) {
            self.entity_equipment
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(slot)
        } else {
            ItemStack::EMPTY.clone()
        }
    }

    fn remove_stack(&self, slot: usize) -> ItemStack {
        if slot < Self::MAIN_SIZE {
            let mut inv = self
                .main_inventory
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::replace(&mut inv[slot], ItemStack::EMPTY.clone())
        } else if let Some(slot) = self.equipment_slots.get(&slot) {
            self.entity_equipment
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .put(slot, ItemStack::EMPTY.clone())
        } else {
            ItemStack::EMPTY.clone()
        }
    }

    fn remove_stack_specific(&self, slot: usize, amount: u8) -> ItemStack {
        if slot < Self::MAIN_SIZE {
            let mut inv = self
                .main_inventory
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !inv[slot].is_empty() && amount > 0 {
                inv[slot].split(amount)
            } else {
                ItemStack::EMPTY.clone()
            }
        } else if let Some(slot) = self.equipment_slots.get(&slot) {
            let mut equipment = self
                .entity_equipment
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut stack = equipment.get(slot);

            if !stack.is_empty() && amount > 0 {
                let split = stack.split(amount);
                equipment.put(slot, stack);
                split
            } else {
                ItemStack::EMPTY.clone()
            }
        } else {
            ItemStack::EMPTY.clone()
        }
    }

    fn set_stack(&self, slot: usize, stack: ItemStack) {
        if slot < Self::MAIN_SIZE {
            let mut inv = self
                .main_inventory
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inv[slot] = stack;
        } else if let Some(slot) = self.equipment_slots.get(&slot) {
            self.entity_equipment
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .put(slot, stack);
        } else {
            warn!("Failed to get Equipment Slot at {slot}");
        }
    }

    fn mark_dirty(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl PlayerInventory {
    /// Sets the selected hotbar slot.
    ///
    /// # Panics
    /// Panics if the slot index is not a valid hotbar index.
    pub fn set_selected_slot(&self, slot: u8) {
        if Self::is_valid_hotbar_index(slot as usize) {
            self.selected_slot.store(slot, Ordering::Relaxed);
        }
    }

    /// Gets the currently selected hotbar slot index.
    pub fn get_selected_slot(&self) -> u8 {
        self.selected_slot.load(Ordering::Relaxed)
    }

    /// Inventory slot of an equipment slot -> the main hand is the selected hotbar slot
    pub fn equipment_slot_index(&self, slot: &EquipmentSlot) -> Option<usize> {
        if matches!(slot, EquipmentSlot::MainHand(_)) {
            return Some(self.get_selected_slot() as usize);
        }
        self.equipment_slots
            .iter()
            .find_map(|(index, equipment)| (equipment == slot).then_some(*index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_equipment_slots;
    use pumpkin_data::item::Item;

    fn create_test_inventory() -> PlayerInventory {
        let equipment_slots = Arc::new(build_equipment_slots());
        let entity_equipment = Arc::new(Mutex::new(EntityEquipment::new()));
        PlayerInventory::new(entity_equipment, equipment_slots)
    }

    #[test]
    fn storage_offer_keeps_the_world_slot_priority() {
        let inv = create_test_inventory();
        inv.set_slot(0, ItemStack::new(1, &Item::DIRT));
        inv.set_slot(4, ItemStack::new(62, &Item::STONE));
        inv.set_slot(12, ItemStack::new(63, &Item::STONE));
        let before = inv.main_inventory.read().unwrap().clone();
        let mut returned = [2, 2, 1].map(|count| ItemStack::new(count, &Item::STONE));
        let plan = PlayerInventory::plan_offers_to_storage(&before, 0, ItemStack::EMPTY, &returned)
            .unwrap();
        let mut world_updates = Vec::new();
        for stack in &mut returned {
            inv.offer_to_slots(stack, |slot| {
                world_updates.push((slot as usize, inv.get_slot(slot as usize)));
            });
            assert!(stack.is_empty());
        }
        assert_eq!(
            plan.iter()
                .map(|(slot, stack)| (*slot, stack.item_count))
                .collect::<Vec<_>>(),
            [(4, 64), (12, 64), (1, 1), (1, 2)]
        );
        assert_eq!(plan.len(), world_updates.len());
        assert_eq!(plan[2].1.uid, plan[3].1.uid);
        for ((slot, stack), (world_slot, world_stack)) in plan.iter().zip(world_updates) {
            assert_eq!(*slot, world_slot);
            assert!(stack.are_equal(&world_stack));
            if !before[*slot].is_empty() {
                assert_eq!(stack.uid, before[*slot].uid);
            }
        }
    }

    #[test]
    fn storage_offer_rejects_partial_returns_and_held_slots() {
        let mut main = std::array::from_fn(|_| ItemStack::new(64, &Item::DIRT));
        main[14] = ItemStack::new(63, &Item::ARROW);
        let returned = [1, 2].map(|count| ItemStack::new(count, &Item::ARROW));
        let plan = |main: &[ItemStack; PlayerInventory::MAIN_SIZE], off_hand: &ItemStack| {
            PlayerInventory::plan_offers_to_storage(main, 0, off_hand, &returned)
        };
        assert!(plan(&main, ItemStack::EMPTY).is_none());
        assert_eq!(main[14].item_count, 63);

        main[15] = ItemStack::EMPTY.clone();
        assert!(plan(&main, &ItemStack::new(63, &Item::ARROW)).is_none());
        main[0] = ItemStack::new(63, &Item::ARROW);
        assert!(plan(&main, ItemStack::EMPTY).is_none());
    }

    #[test]
    fn add_stack_to_slot_offhand_when_empty() {
        let inv = create_test_inventory();
        let stack = ItemStack::new(10, &Item::ARROW);

        let leftover = inv.add_stack_to_slot(PlayerInventory::OFF_HAND_SLOT, stack);
        assert_eq!(leftover, 0);

        let off_hand = inv.off_hand_item();
        assert_eq!(off_hand.item.id, Item::ARROW.id);
        assert_eq!(off_hand.item_count, 10);
    }

    #[test]
    fn add_stack_to_slot_offhand_stacks_matching_item() {
        let inv = create_test_inventory();
        inv.set_stack_in_hand(Hand::Left, ItemStack::new(10, &Item::ARROW));

        let leftover = inv.add_stack_to_slot(
            PlayerInventory::OFF_HAND_SLOT,
            ItemStack::new(15, &Item::ARROW),
        );
        assert_eq!(leftover, 0);

        let off_hand = inv.off_hand_item();
        assert_eq!(off_hand.item.id, Item::ARROW.id);
        assert_eq!(off_hand.item_count, 25);
    }

    #[test]
    fn add_stack_to_slot_offhand_stacks_up_to_max() {
        let inv = create_test_inventory();
        inv.set_stack_in_hand(Hand::Left, ItemStack::new(60, &Item::ARROW));

        let leftover = inv.add_stack_to_slot(
            PlayerInventory::OFF_HAND_SLOT,
            ItemStack::new(10, &Item::ARROW),
        );
        assert_eq!(leftover, 6);

        let off_hand = inv.off_hand_item();
        assert_eq!(off_hand.item.id, Item::ARROW.id);
        assert_eq!(off_hand.item_count, 64);
    }

    #[test]
    fn add_stack_to_slot_offhand_rejects_different_item() {
        let inv = create_test_inventory();
        inv.set_stack_in_hand(Hand::Left, ItemStack::new(1, &Item::SHIELD));

        let leftover = inv.add_stack_to_slot(
            PlayerInventory::OFF_HAND_SLOT,
            ItemStack::new(10, &Item::ARROW),
        );
        assert_eq!(leftover, 10);

        let off_hand = inv.off_hand_item();
        assert_eq!(off_hand.item.id, Item::SHIELD.id);
        assert_eq!(off_hand.item_count, 1);
    }

    #[test]
    fn add_stack_to_slot_armor_does_not_stack() {
        let inv = create_test_inventory();
        let feet_slot = EquipmentSlot::FEET
            .get_offset_entity_slot_id(PlayerInventory::MAIN_SIZE as i32)
            as usize;
        let boots = ItemStack::new(1, &Item::IRON_BOOTS);

        let leftover = inv.add_stack_to_slot(feet_slot, boots.clone());
        assert_eq!(leftover, 0);

        let leftover_second = inv.add_stack_to_slot(feet_slot, boots);
        assert_eq!(leftover_second, 1);
    }

    #[test]
    fn pickup_stacks_into_offhand() {
        let inv = create_test_inventory();
        inv.set_stack_in_hand(Hand::Left, ItemStack::new(10, &Item::ARROW));

        let mut pickup = ItemStack::new(5, &Item::ARROW);
        let inserted = inv.insert_stack_anywhere(&mut pickup);

        assert!(inserted);
        assert!(pickup.is_empty());
        assert_eq!(inv.off_hand_item().item_count, 15);
    }

    #[test]
    fn pickup_prioritizes_held_main_hand_over_offhand() {
        let inv = create_test_inventory();
        inv.set_held_item(ItemStack::new(60, &Item::ARROW));
        inv.set_stack_in_hand(Hand::Left, ItemStack::new(10, &Item::ARROW));

        let mut pickup = ItemStack::new(10, &Item::ARROW);
        let inserted = inv.insert_stack_anywhere(&mut pickup);

        assert!(inserted);
        assert!(pickup.is_empty());
        assert_eq!(inv.held_item().item_count, 64);
        assert_eq!(inv.off_hand_item().item_count, 16);
    }

    #[test]
    fn pickup_offhand_full_overflows_to_main_inventory() {
        let inv = create_test_inventory();
        inv.set_stack_in_hand(Hand::Left, ItemStack::new(64, &Item::ARROW));

        let mut pickup = ItemStack::new(10, &Item::ARROW);
        let inserted = inv.insert_stack_anywhere(&mut pickup);

        assert!(inserted);
        assert!(pickup.is_empty());
        assert_eq!(inv.off_hand_item().item_count, 64);
        assert_eq!(inv.get_slot(0).item.id, Item::ARROW.id);
        assert_eq!(inv.get_slot(0).item_count, 10);
    }

    #[test]
    fn pickup_does_not_fill_empty_offhand() {
        let inv = create_test_inventory();

        let mut pickup = ItemStack::new(10, &Item::ARROW);
        let inserted = inv.insert_stack_anywhere(&mut pickup);

        assert!(inserted);
        assert!(pickup.is_empty());
        assert!(inv.off_hand_item().is_empty());
        assert_eq!(inv.get_slot(0).item.id, Item::ARROW.id);
        assert_eq!(inv.get_slot(0).item_count, 10);
    }

    #[test]
    fn planned_pickup_keeps_priority_and_rejects_changed_source_or_slots() {
        let inventory = create_test_inventory();
        for index in 0..PlayerInventory::MAIN_SIZE {
            inventory.set_slot(index, ItemStack::new(64, &Item::DIRT));
        }
        inventory.set_held_item(ItemStack::new(63, &Item::ARROW));
        inventory.set_stack_in_hand(Hand::Left, ItemStack::new(63, &Item::ARROW));
        inventory.set_slot(9, ItemStack::new(62, &Item::ARROW));

        let mut source = ItemStack::new(6, &Item::ARROW);
        let plan = inventory.plan_pickup(&source).unwrap();
        assert_eq!(plan.count(), 4);
        assert!(plan.full_storage_updates().is_none());
        assert_eq!(
            plan.slots.iter().map(|slot| slot.index).collect::<Vec<_>>(),
            [0, PlayerInventory::OFF_HAND_SLOT, 9]
        );
        let mut changed_source = source.copy_with_count(6);
        assert!(!inventory.commit_pickup(plan, &mut changed_source, || true));

        let plan = inventory.plan_pickup(&source).unwrap();
        inventory.set_slot(9, ItemStack::new(62, &Item::ARROW));
        let counts = || {
            [
                inventory.held_item().item_count,
                inventory.off_hand_item().item_count,
                inventory.get_slot(9).item_count,
            ]
        };
        assert!(!inventory.commit_pickup(plan, &mut source, || true));
        assert_eq!(counts(), [63, 63, 62]);
        assert_eq!(source.item_count, 6);
        assert_eq!(changed_source.item_count, 6);

        let plan = inventory.plan_pickup(&source).unwrap();
        assert!(!inventory.commit_pickup(plan, &mut source, || false));
        assert_eq!(counts(), [63, 63, 62]);
        assert_eq!(source.item_count, 6);

        let plan = inventory.plan_pickup(&source).unwrap();
        assert!(inventory.commit_pickup(plan, &mut source, || true));
        assert_eq!(counts(), [64, 64, 64]);
        assert_eq!(source.item_count, 2);
        assert!(inventory.plan_pickup(&source).is_none());
    }

    #[test]
    fn full_pickup_exposes_every_non_held_slot() {
        let inventory = create_test_inventory();
        inventory.set_slot(9, ItemStack::new(62, &Item::STONE));
        inventory.set_slot(10, ItemStack::new(61, &Item::STONE));
        let mut source = ItemStack::new(5, &Item::STONE);
        let plan = inventory.plan_pickup(&source).unwrap();
        let updates: Vec<_> = plan
            .full_storage_updates()
            .unwrap()
            .map(|(slot, after)| (slot, after.item_count))
            .collect();
        assert_eq!(updates, [(9, 64), (10, 64)]);
        assert!(plan.single_storage_update().is_none());

        inventory.set_slot(10, ItemStack::new(60, &Item::STONE));
        assert!(!inventory.commit_pickup(plan, &mut source, || true));
        assert_eq!(inventory.get_slot(9).item_count, 62);
        assert_eq!(source.item_count, 5);

        let fresh = inventory.plan_pickup(&source).unwrap();
        assert!(inventory.commit_pickup(fresh, &mut source, || true));
        assert_eq!(inventory.get_slot(9).item_count, 64);
        assert_eq!(inventory.get_slot(10).item_count, 63);
        assert!(source.is_empty());

        inventory.set_held_item(ItemStack::new(63, &Item::STONE));
        let source = ItemStack::new(2, &Item::STONE);
        let held = inventory.plan_pickup(&source).unwrap();
        assert_eq!(held.count(), 2);
        assert!(held.full_storage_updates().is_none());
        assert!(held.storage_updates().is_none());
    }

    #[test]
    fn storage_pickup_exposes_the_remainder_and_regular_slots() {
        let inventory = create_test_inventory();
        for index in 0..PlayerInventory::MAIN_SIZE {
            inventory.set_slot(index, ItemStack::new(64, &Item::DIRT));
        }
        inventory.set_slot(9, ItemStack::new(63, &Item::STONE));
        inventory.set_slot(10, ItemStack::new(62, &Item::STONE));
        let mut source = ItemStack::new(5, &Item::STONE);
        let pickup = inventory.plan_pickup(&source).unwrap();
        assert_eq!(pickup.count(), 3);
        assert!(pickup.full_storage_updates().is_none());
        let updates: Vec<_> = pickup
            .storage_updates()
            .unwrap()
            .map(|(slot, after)| (slot, after.item_count))
            .collect();
        assert_eq!(updates, [(9, 64), (10, 64)]);
        assert!(inventory.commit_pickup(pickup, &mut source, || true));
        assert_eq!(source.item_count, 2);
    }

    /// When the main inventory is completely full and only the off-hand has
    /// partial room, `insert_stack` must still return `true` even though the
    /// second loop iteration makes no progress.
    #[test]
    fn insert_stack_returns_true_on_partial_offhand_with_full_main_inventory() {
        let inv = create_test_inventory();
        for i in 0..PlayerInventory::MAIN_SIZE {
            inv.set_slot(i, ItemStack::new(64, &Item::DIRT));
        }
        inv.set_stack_in_hand(Hand::Left, ItemStack::new(60, &Item::ARROW));

        let mut pickup = ItemStack::new(10, &Item::ARROW);
        let inserted = inv.insert_stack_anywhere(&mut pickup);

        assert!(
            inserted,
            "insert_stack must return true when some items were placed"
        );
        assert_eq!(
            pickup.item_count, 6,
            "6 arrows should remain after filling off-hand to 64"
        );
        assert_eq!(inv.off_hand_item().item_count, 64);
    }
}
