use std::sync::{Arc, Mutex};

use pumpkin_data::item_stack::ItemStack;
use pumpkin_inventory::{
    container_click::SLOT_INDEX_OUTSIDE,
    inventory::Inventory,
    screen_handler::{ClickType, ScreenHandler},
    slot::Slot,
};
use pumpkin_protocol::{
    bedrock::{
        client::{
            inventory_content::CInventoryContent, inventory_slot::CInventorySlot,
            item_stack_response::CItemStackResponse,
        },
        network_item::{
            ContainerName, FullContainerName, NetworkItemDescriptor, NetworkItemStackDescriptor,
        },
        server::{
            inventory_transaction::{InventoryAction, InventoryActionSource, WINDOW_ID_INVENTORY},
            item_stack_request::{
                ItemStackRequest, ItemStackRequestAction as Action,
                ItemStackRequestSlotInfo as SlotInfo,
            },
        },
    },
    codec::{var_int::VarInt, var_uint::VarUInt},
};
use pumpkin_util::GameMode;

use crate::{
    entity::player::Player,
    net::bedrock::BedrockClient,
    plugin::{
        inventory::inventory_interact::InventoryInteractEvent,
        player::inventory_interact::InventoryClickEvent,
        player::player_drop_item::PlayerDropItemEvent,
    },
    world::World,
};

use super::item_stack_request::{
    SlotUpdate, build_stack_response, map_bedrock_container_slot, record_update,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Slot(usize),
    Cursor,
    Created,
}

struct PlayerInventoryPlan {
    world: Arc<World>,
    mode: GameMode,
    creative: bool,
    clicked_slot: Option<usize>,
    click: Option<InventoryClickEvent>,
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

pub(super) fn matches_item_type_and_count(desc: &NetworkItemDescriptor, stack: &ItemStack) -> bool {
    if stack.is_empty() {
        return desc.id.0 == 0 || desc.stack_size == 0;
    }
    // Client NBT and runtime state never replace data on the server's stack.
    let expected = NetworkItemDescriptor::from(stack);
    desc.id == expected.id
        && desc.aux_value == expected.aux_value
        && desc.stack_size == expected.stack_size
}

const fn player_container(info: &SlotInfo) -> bool {
    matches!(
        info.container_name.container_name,
        ContainerName::Inventory
            | ContainerName::CombinedHotBarAndInventory
            | ContainerName::HotBar
            | ContainerName::Armor
            | ContainerName::Offhand
            | ContainerName::Cursor
    )
}

pub(super) fn is_player_inventory_request(request: &ItemStackRequest) -> bool {
    request.actions.iter().all(|action| match action {
        Action::Take {
            source,
            destination,
            ..
        }
        | Action::Place {
            source,
            destination,
            ..
        } => player_container(source) && player_container(destination),
        Action::Swap { slot1, slot2 } => player_container(slot1) && player_container(slot2),
        Action::Drop { source, .. }
        | Action::Destroy { source, .. }
        | Action::Consume { source, .. } => player_container(source),
        _ => false,
    })
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

impl PlayerInventoryPlan {
    fn prepare(player: &Arc<Player>, request: &ItemStackRequest) -> Result<Self, ()> {
        let mode = player.gamemode.load();
        let creative = request
            .actions
            .iter()
            .any(|action| matches!(action, Action::CraftCreative { .. }));
        if mode == GameMode::Spectator || (creative && mode != GameMode::Creative) {
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
        if !handler.can_use(player.as_ref()) {
            return Err(());
        }
        let slots = handler.get_behaviour().slots.clone();
        let before: Vec<_> = slots.iter().map(|slot| slot.get_cloned_stack()).collect();
        let cursor = handler.get_behaviour().cursor_stack.clone();
        let cursor_before = cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut plan = Self {
            world: player.world(),
            mode,
            creative,
            clicked_slot: None,
            click: None,
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
        plan.apply(player, &*handler, request)?;
        if !creative && !request.actions.is_empty() {
            let (slot, clicked) = if let Some(index) = plan.clicked_slot {
                (
                    i16::try_from(index).map_err(|_| ())?,
                    Some(plan.before[index].clone()),
                )
            } else {
                (SLOT_INDEX_OUTSIDE, None)
            };
            plan.click = Some(InventoryClickEvent::new(
                player,
                handler.window_type(),
                ClickType::Unknown,
                slot,
                slot,
                clicked,
                Some(plan.cursor_before.clone()),
                -1,
            ));
        }
        Ok(plan)
    }

    fn apply(
        &mut self,
        player: &Player,
        handler: &dyn ScreenHandler,
        request: &ItemStackRequest,
    ) -> Result<(), ()> {
        let mut created = false;
        for action in &request.actions {
            match action {
                Action::CraftCreative {
                    creative_item_id, ..
                } => {
                    if created {
                        return Err(());
                    }
                    self.created = creative_stack(creative_item_id.0).ok_or(())?;
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
                    self.transfer(player, handler, *count, source, destination)?;
                }
                Action::Swap { slot1, slot2 } => {
                    let a = self.target(player, handler, slot1)?;
                    let b = self.target(player, handler, slot2)?;
                    if a == Target::Created || b == Target::Created {
                        return Err(());
                    }
                    let first = self.get(a);
                    let second = self.get(b);
                    self.can_take(player, a)?;
                    self.can_take(player, b)?;
                    self.can_put(a, &second)?;
                    self.can_put(b, &first)?;
                    self.put(a, second, slot1);
                    self.put(b, first, slot2);
                }
                Action::Destroy { count, source }
                | Action::Consume { count, source }
                | Action::Drop { count, source, .. } => {
                    if (matches!(action, Action::Destroy { .. }) && self.mode != GameMode::Creative)
                        || (matches!(action, Action::Consume { .. }) && !self.creative)
                    {
                        return Err(());
                    }
                    let from = self.target(player, handler, source)?;
                    let mut stack = self.get(from);
                    if *count == 0 || *count > stack.item_count {
                        return Err(());
                    }
                    self.can_take(player, from)?;
                    if matches!(action, Action::Drop { .. }) {
                        self.drops.push(stack.copy_with_count(*count));
                    }
                    stack.decrement(*count);
                    self.put(from, stack, source);
                }
                Action::CraftResultsDeprecated { .. } | Action::Create { .. } => {}
                _ => return Err(()),
            }
        }
        Ok(())
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
            if let Target::Slot(index) = from {
                self.clicked_slot.get_or_insert(index);
            }
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
            ContainerName::CreatedOutput if self.creative => return Ok(Target::Created),
            ContainerName::Cursor if info.slot_id == 0 => return Ok(Target::Cursor),
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
        if let Target::Slot(index) = target {
            self.clicked_slot.get_or_insert(index);
        }
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
        let Some(server) = self.world.server.upgrade() else {
            return false;
        };
        if let Some(event) = &self.click {
            let mut event = event.clone();
            server.plugin_manager.fire_blocking(&server, &mut event);
            if event.cancelled {
                return false;
            }
        }
        if self.creative {
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
        } else {
            let mut event = InventoryInteractEvent::new(player.clone());
            server.plugin_manager.fire_blocking(&server, &mut event);
            if event.cancelled {
                return false;
            }
        }
        for stack in &self.drops {
            if self.creative && !player.allow_creative_change(-1, stack) {
                return false;
            }
            let mut event = PlayerDropItemEvent::new(
                player.clone(),
                stack.item.registry_key.to_string(),
                stack.item_count,
            );
            server.plugin_manager.fire_blocking(&server, &mut event);
            if event.cancelled {
                return false;
            }
        }
        true
    }

    fn commit(&self, player: &Player) -> bool {
        let current = player
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !Arc::ptr_eq(&current, &self.screen)
            || !Arc::ptr_eq(&player.world(), &self.world)
            || player.gamemode.load() != self.mode
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

    fn execute(self, player: &Arc<Player>) -> Option<Vec<SlotUpdate>> {
        let accepted = self.authorize(player) && self.commit(player);
        for (index, before) in self.before.iter().enumerate() {
            let current = self.slots[index].get_cloned_stack();
            if !before.are_equal(&current)
                && let Some(slot) = player_slot(player, self.slots[index].as_ref())
            {
                player.sync_screen_slot_equipment(slot, &current);
            }
        }
        if !accepted {
            return None;
        }
        for stack in self.drops {
            player.drop_item(stack);
        }
        Some(self.updates)
    }
}

impl BedrockClient {
    pub(super) fn handle_player_inventory_request(
        &self,
        player: &Arc<Player>,
        request: &ItemStackRequest,
    ) {
        let (result, updates) = PlayerInventoryPlan::prepare(player, request)
            .ok()
            .and_then(|plan| plan.execute(player))
            .map_or_else(|| (1, Vec::new()), |updates| (0, updates));
        self.try_enqueue_client_packet(&CItemStackResponse {
            responses: vec![build_stack_response(request.request_id, result, updates)],
        });
        self.resend_player_inventory(player);
    }

    pub(super) fn apply_legacy_inventory_drop(
        player: &Arc<Player>,
        actions: &[InventoryAction],
    ) -> Option<Vec<SlotUpdate>> {
        let [first, second] = actions else {
            return None;
        };
        let (source, dropped) = match (
            InventoryActionSource::from(first.source_type),
            InventoryActionSource::from(second.source_type),
        ) {
            (InventoryActionSource::Container, InventoryActionSource::World) => (first, second),
            (InventoryActionSource::World, InventoryActionSource::Container) => (second, first),
            _ => return None,
        };
        if source.window_id != Some(WINDOW_ID_INVENTORY)
            || source.inventory_slot >= 36
            || dropped.inventory_slot != 0
            || (dropped.old_item.id.0 != 0 && dropped.old_item.stack_size != 0)
        {
            return None;
        }
        let count = u8::try_from(dropped.new_item.stack_size).ok()?;
        let slot = source.inventory_slot as u8;
        let request = ItemStackRequest {
            request_id: VarInt(0),
            actions: vec![Action::Drop {
                count,
                source: SlotInfo {
                    container_name: FullContainerName {
                        container_name: ContainerName::Inventory,
                        dynamic_id: None,
                    },
                    slot_id: slot,
                    stack_id: 0,
                },
                randomly: false,
            }],
            filter_strings: Vec::new(),
            filter_cause: 0,
        };
        let mut plan = PlayerInventoryPlan::prepare(player, &request).ok()?;
        // Legacy hotbar drops do not carry an inventory click gesture.
        plan.click = None;
        let screen_slot = if slot < 9 {
            usize::from(slot) + 36
        } else {
            usize::from(slot)
        };
        let index = plan
            .slots
            .iter()
            .position(|slot| player_slot(player, slot.as_ref()) == Some(screen_slot))?;
        let actual = &plan.before[index];
        let remaining = actual.copy_with_count(actual.item_count.checked_sub(count)?);
        if !matches_item_type_and_count(&source.old_item, actual)
            || !matches_item_type_and_count(&source.new_item, &remaining)
            || !matches_item_type_and_count(&dropped.new_item, &actual.copy_with_count(count))
        {
            return None;
        }
        plan.execute(player)
    }

    pub(super) fn resend_player_inventory(&self, player: &Player) {
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
