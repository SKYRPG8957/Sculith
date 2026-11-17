use std::sync::{Arc, Mutex};

use pumpkin_data::item_stack::ItemStack;
use pumpkin_inventory::{inventory::Inventory, screen_handler::ScreenHandler, slot::Slot};
use pumpkin_protocol::{
    bedrock::{
        client::{
            inventory_content::CInventoryContent, inventory_slot::CInventorySlot,
            item_stack_response::CItemStackResponse,
        },
        network_item::{ContainerName, FullContainerName, NetworkItemStackDescriptor},
        server::item_stack_request::{
            ItemStackRequest, ItemStackRequestAction as Action,
            ItemStackRequestSlotInfo as SlotInfo,
        },
    },
    codec::var_uint::VarUInt,
};
use pumpkin_util::GameMode;

use crate::{entity::player::Player, net::bedrock::BedrockClient, world::World};

use super::item_stack_request::{
    SlotUpdate, build_stack_response, map_bedrock_container_slot, record_update,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Slot(usize),
    Cursor,
    Created,
}

struct CreativePlan {
    world: Arc<World>,
    screen: Arc<Mutex<dyn ScreenHandler>>,
    slots: Vec<Arc<dyn Slot>>,
    before: Vec<ItemStack>,
    after: Vec<ItemStack>,
    cursor: Arc<Mutex<ItemStack>>,
    cursor_before: ItemStack,
    cursor_after: ItemStack,
    created: ItemStack,
    updates: Vec<SlotUpdate>,
    drops: Vec<ItemStack>,
}

fn same_stack(a: &ItemStack, b: &ItemStack) -> bool {
    a.uid == b.uid && a.are_equal(b)
}

fn player_slot(player: &Player, slot: &dyn Slot) -> Option<usize> {
    let inventory: Arc<dyn Inventory> = player.inventory.clone();
    if !Arc::ptr_eq(&inventory, &slot.get_inventory()) {
        return None;
    }
    match slot.get_index() {
        index @ 0..9 => Some(36 + index),
        index @ 9..36 => Some(index),
        index @ 36..40 => Some(44 - index),
        40 => Some(45),
        _ => None,
    }
}

fn creative_stack(network_id: u32) -> Option<ItemStack> {
    let index = network_id.checked_sub(1)? as usize;
    let entry = pumpkin_data::bedrock_creative::CREATIVE_ENTRIES.get(index)?;
    let mapping = pumpkin_data::item::JavaToBedrockItemMapping::from_bedrock(
        entry.item_id,
        entry.item_aux_value,
    )?;
    let limit = ItemStack::static_new_java(1, mapping.java_item).get_max_stack_size();
    Some(ItemStack::new(limit, mapping.java_item))
}

impl CreativePlan {
    fn prepare(player: &Arc<Player>, request: &ItemStackRequest) -> Result<Self, ()> {
        if player.gamemode.load() != GameMode::Creative {
            return Err(());
        }
        let screen = player
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let handler = screen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slots = handler.get_behaviour().slots.clone();
        let before: Vec<_> = slots.iter().map(|slot| slot.get_cloned_stack()).collect();
        let cursor = handler.get_behaviour().cursor_stack.clone();
        let cursor_before = cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut plan = Self {
            world: player.world(),
            screen: screen.clone(),
            slots,
            after: before.clone(),
            before,
            cursor,
            cursor_after: cursor_before.clone(),
            cursor_before,
            created: ItemStack::EMPTY.clone(),
            updates: Vec::new(),
            drops: Vec::new(),
        };
        let mut created = false;
        for action in &request.actions {
            match action {
                Action::CraftCreative {
                    creative_item_id, ..
                } => {
                    if created {
                        return Err(());
                    }
                    plan.created = creative_stack(creative_item_id.0).ok_or(())?;
                    created = true;
                }
                Action::Take {
                    count,
                    source,
                    destination,
                }
                | Action::Place {
                    count,
                    source,
                    destination,
                } => {
                    plan.transfer(player, &*handler, *count, source, destination)?;
                }
                Action::Swap { slot1, slot2 } => {
                    let a = plan.target(player, &*handler, slot1)?;
                    let b = plan.target(player, &*handler, slot2)?;
                    if a == Target::Created || b == Target::Created {
                        return Err(());
                    }
                    let first = plan.get(a);
                    let second = plan.get(b);
                    plan.can_take(player, a)?;
                    plan.can_take(player, b)?;
                    plan.can_put(a, &second)?;
                    plan.can_put(b, &first)?;
                    plan.put(a, second, slot1);
                    plan.put(b, first, slot2);
                }
                Action::Destroy { count, source }
                | Action::Consume { count, source }
                | Action::Drop { count, source, .. } => {
                    let from = plan.target(player, &*handler, source)?;
                    let mut stack = plan.get(from);
                    if *count == 0 || *count > stack.item_count {
                        return Err(());
                    }
                    plan.can_take(player, from)?;
                    if matches!(action, Action::Drop { .. }) {
                        plan.drops.push(stack.copy_with_count(*count));
                    }
                    stack.decrement(*count);
                    plan.put(from, stack, source);
                }
                Action::CraftResultsDeprecated { .. } | Action::Create { .. } => {}
                _ => return Err(()),
            }
        }
        Ok(plan)
    }

    fn transfer(
        &mut self,
        player: &Player,
        handler: &dyn ScreenHandler,
        count: u8,
        source: &SlotInfo,
        destination: &SlotInfo,
    ) -> Result<(), ()> {
        let from = self.target(player, handler, source)?;
        let to = self.target(player, handler, destination)?;
        if to == Target::Created {
            return Err(());
        }
        let mut source_stack = self.get(from);
        if count == 0 || count > source_stack.item_count {
            return Err(());
        }
        if from == to {
            return Ok(());
        }
        let mut dest = self.get(to);
        if dest.is_empty() {
            dest = source_stack.copy_with_count(count);
        } else if dest.are_items_and_components_equal(&source_stack) {
            dest.item_count = dest.item_count.checked_add(count).ok_or(())?;
        } else {
            return Err(());
        }
        self.can_put(to, &dest)?;
        self.can_take(player, from)?;
        source_stack.decrement(count);
        self.put(from, source_stack, source);
        self.put(to, dest, destination);
        Ok(())
    }

    fn target(
        &self,
        player: &Player,
        handler: &dyn ScreenHandler,
        info: &SlotInfo,
    ) -> Result<Target, ()> {
        if info.container_name.dynamic_id.is_some() {
            return Err(());
        }
        let name = info.container_name.container_name;
        match name {
            ContainerName::CreatedOutput => return Ok(Target::Created),
            ContainerName::Cursor => return Ok(Target::Cursor),
            ContainerName::HotBar if info.slot_id >= 9 => return Err(()),
            ContainerName::Inventory
            | ContainerName::CombinedHotBarAndInventory
            | ContainerName::HotBar
            | ContainerName::Armor
            | ContainerName::Offhand => {}
            _ => return Err(()),
        }
        let index = map_bedrock_container_slot(handler, name, info.slot_id).ok_or(())?;
        let slot = self.slots.get(index).ok_or(())?;
        player_slot(player, slot.as_ref()).ok_or(())?;
        Ok(Target::Slot(index))
    }

    fn get(&self, target: Target) -> ItemStack {
        match target {
            Target::Slot(index) => self.after[index].clone(),
            Target::Cursor => self.cursor_after.clone(),
            Target::Created => self.created.clone(),
        }
    }

    fn can_take(&self, player: &Player, target: Target) -> Result<(), ()> {
        if let Target::Slot(index) = target
            && !self.slots[index].can_take_items(player)
        {
            return Err(());
        }
        Ok(())
    }

    fn can_put(&self, target: Target, stack: &ItemStack) -> Result<(), ()> {
        if !stack.is_empty() && stack.item_count > stack.get_max_stack_size() {
            return Err(());
        }
        if let Target::Slot(index) = target
            && !stack.is_empty()
            && (!self.slots[index].can_insert(stack)
                || stack.item_count > self.slots[index].get_max_item_count_for_stack(stack))
        {
            return Err(());
        }
        Ok(())
    }

    fn put(&mut self, target: Target, stack: ItemStack, info: &SlotInfo) {
        let stack = if stack.is_empty() {
            ItemStack::EMPTY.clone()
        } else {
            stack
        };
        match target {
            Target::Slot(index) => self.after[index] = stack.clone(),
            Target::Cursor => self.cursor_after = stack.clone(),
            Target::Created => self.created = stack.clone(),
        }
        record_update(
            &mut self.updates,
            info.container_name.clone(),
            info.slot_id,
            &stack,
        );
    }

    fn authorize(&self, player: &Arc<Player>) -> bool {
        for (index, stack) in self.after.iter().enumerate() {
            if !same_stack(stack, &self.before[index]) {
                let Some(slot) = player_slot(player, self.slots[index].as_ref()) else {
                    return false;
                };
                if !player.allow_creative_change(slot as i16, stack) {
                    return false;
                }
            }
        }
        if !same_stack(&self.cursor_before, &self.cursor_after)
            && !player.allow_creative_change(-1, &self.cursor_after)
        {
            return false;
        }
        self.drops
            .iter()
            .all(|stack| player.allow_creative_change(-1, stack))
    }

    fn commit(&self, player: &Player) -> bool {
        let current = player
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !Arc::ptr_eq(&current, &self.screen)
            || !Arc::ptr_eq(&player.world(), &self.world)
            || player.gamemode.load() != GameMode::Creative
        {
            return false;
        }
        let mut handler = current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if handler.get_behaviour().slots.len() != self.slots.len() {
            return false;
        }
        for (index, before) in self.before.iter().enumerate() {
            if !same_stack(before, &self.after[index])
                && (!Arc::ptr_eq(&self.slots[index], &handler.get_behaviour().slots[index])
                    || !same_stack(before, &self.slots[index].get_cloned_stack()))
            {
                return false;
            }
        }
        let cursor_changed = !same_stack(&self.cursor_before, &self.cursor_after);
        if cursor_changed && !Arc::ptr_eq(&self.cursor, &handler.get_behaviour().cursor_stack) {
            return false;
        }
        let mut cursor = cursor_changed.then(|| {
            self.cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        if let Some(cursor) = &cursor
            && !same_stack(cursor, &self.cursor_before)
        {
            return false;
        }
        for (index, after) in self.after.iter().enumerate() {
            if !same_stack(&self.before[index], after) {
                self.slots[index].set_stack(after.clone());
                handler.set_received_stack(index, after.clone());
            }
        }
        if let Some(cursor) = &mut cursor {
            **cursor = self.cursor_after.clone();
        }
        true
    }
}

impl BedrockClient {
    pub(super) fn handle_creative_stack_request(
        &self,
        player: &Arc<Player>,
        request: &ItemStackRequest,
    ) {
        let (result, updates) = if let Ok(plan) = CreativePlan::prepare(player, request) {
            let accepted = plan.authorize(player) && plan.commit(player);
            for (index, before) in plan.before.iter().enumerate() {
                let current = plan.slots[index].get_cloned_stack();
                if !before.are_equal(&current)
                    && let Some(slot) = player_slot(player, plan.slots[index].as_ref())
                {
                    player.sync_screen_slot_equipment(slot, &current);
                }
            }
            if accepted {
                for stack in plan.drops {
                    player.drop_item(stack);
                }
                (0, plan.updates)
            } else {
                (1, Vec::new())
            }
        } else {
            (1, Vec::new())
        };
        self.try_enqueue_client_packet(&CItemStackResponse {
            responses: vec![build_stack_response(request.request_id, result, updates)],
        });
        self.resend_creative_inventory(player);
    }

    fn resend_creative_inventory(&self, player: &Player) {
        let slots = player
            .inventory
            .main_inventory
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(NetworkItemStackDescriptor::from)
            .collect();
        let current = player
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let cursor = current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_behaviour()
            .cursor_stack
            .clone();
        let cursor = cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let equipment: Vec<_> = (36..=40)
            .filter_map(|index| {
                super::bedrock_player_inventory_slot(index)
                    .map(|target| (target, player.inventory.get_slot(index)))
            })
            .collect();
        self.try_enqueue_client_packet(&CInventoryContent {
            container_id: VarUInt(0),
            slots,
            full_container_name: FullContainerName {
                container_name: ContainerName::Inventory,
                dynamic_id: None,
            },
            storage_item: NetworkItemStackDescriptor::default(),
        });
        self.try_enqueue_client_packet(&CInventorySlot {
            container_id: VarUInt(124),
            slot: VarUInt(0),
            full_container_name: Some(FullContainerName {
                container_name: ContainerName::Cursor,
                dynamic_id: None,
            }),
            storage_item: None,
            item: NetworkItemStackDescriptor::from(&cursor),
        });
        for (target, stack) in equipment {
            super::send_bedrock_inventory_slot(self, target, &stack);
        }
        player.update_bedrock_shield_sneaking();
    }
}
