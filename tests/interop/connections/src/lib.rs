//! Native connection-event fixture, installed only into the interop scratch server.

mod storage_contention;

use std::sync::{
    Arc, LazyLock, Weak,
    atomic::{AtomicBool, Ordering},
};

use pumpkin_data::{
    data_component_impl::EquipmentSlot, entity::EntityType, item::Item, item_stack::ItemStack,
};
use pumpkin_inventory::screen_handler::ScreenHandler;
use sculith_core::{
    plugin::inventory::craft_item::CraftItemEvent,
    plugin::inventory::inventory_creative::InventoryCreativeEvent,
    plugin::inventory::inventory_drag::InventoryDragEvent,
    plugin::inventory::inventory_interact::InventoryInteractEvent,
    plugin::inventory::prepare_item_craft::PrepareItemCraftEvent,
    plugin::player::inventory_close::InventoryCloseEvent,
    plugin::player::inventory_interact::InventoryClickEvent,
    plugin::player::item_held::PlayerItemHeldEvent,
    plugin::player::player_drop_item::PlayerDropItemEvent,
    plugin::player::player_interact_event::{InteractAction, PlayerInteractEvent},
    plugin::server::packet::{
        BedrockConnectionPacketReceivedEvent, BedrockConnectionPacketSentEvent,
        JavaConnectionPacketReceivedEvent, JavaConnectionPacketSentEvent, PacketReceivedEvent,
        PacketSentEvent,
    },
    plugin::{
        BoxFuture, Context, EventHandler, EventPriority, Plugin, PluginFuture, PluginMetadata,
    },
    resource_pack::PackEdit,
    server::Server,
};
use sculith_protocol::{
    ConnectionState, ServerPacket,
    bedrock::server::{login::SLogin, request_network_settings::SRequestNetworkSettings},
    codec::var_long::VarLong,
    codec::var_ulong::VarULong,
    java::client::status::CStatusResponse,
    java::server::login::SLoginStart,
    packet::{MultiVersionJavaPacket, Packet},
    serial::PacketRead,
};

#[unsafe(no_mangle)]
pub static SCULITH_API_VERSION: u32 = sculith_core::plugin::PLUGIN_API_VERSION;

#[unsafe(no_mangle)]
pub static METADATA: LazyLock<PluginMetadata> = LazyLock::new(|| PluginMetadata {
    name: "sculith-interop-connections".into(),
    version: "0.0.0".into(),
    authors: Vec::new(),
    description: "Connection event regression fixture".into(),
    dependencies: Vec::new(),
    permissions: Vec::new(),
});

#[unsafe(no_mangle)]
pub extern "Rust" fn plugin() -> Box<dyn Plugin> {
    Box::new(Connections)
}

struct Connections;
struct Observer(Weak<Context>);

const TRACKER_MOTION_TRIGGER: i64 = 0x212223242526272;

impl Plugin for Connections {
    fn on_load(&self, context: Arc<Context>) -> PluginFuture<'_, Result<(), String>> {
        Box::pin(async move {
            context.init_log();
            let observer = Arc::new(Observer(Arc::downgrade(&context)));
            context.register_event::<BedrockConnectionPacketReceivedEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<BedrockConnectionPacketSentEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<JavaConnectionPacketReceivedEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<JavaConnectionPacketSentEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<PlayerItemHeldEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<PacketSentEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<PacketReceivedEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<InventoryCreativeEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<InventoryInteractEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<PlayerDropItemEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<InventoryClickEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<InventoryDragEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<CraftItemEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<PrepareItemCraftEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<PlayerInteractEvent, _>(
                observer.clone(),
                EventPriority::Lowest,
                true,
            );
            context.register_event::<InventoryCloseEvent, _>(observer, EventPriority::Lowest, true);
            storage_contention::register(&context);
            context.log("PROBE_CONNECTIONS_LOADED");
            Ok(())
        })
    }
}

static CLOSE_MUTATED: AtomicBool = AtomicBool::new(false);

impl EventHandler<CraftItemEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut CraftItemEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let name = event.player.gameprofile.name.as_str();
            if !matches!(
                name,
                "CraftReference" | "CraftVeto" | "CraftPrepVeto" | "CraftEdit"
            ) {
                return;
            }
            let screen = event
                .player
                .current_screen_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let input = screen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_behaviour()
                .slots[1]
                .get_cloned_stack()
                .item_count;
            let target = event.player.inventory.get_slot(1).item_count;
            if name == "CraftEdit" {
                event
                    .player
                    .inventory
                    .set_slot(1, ItemStack::new(7, &Item::GRASS_BLOCK));
            }
            event.cancelled = name == "CraftVeto";
            if let Some(context) = self.0.upgrade() {
                context.log(format!(
                    "PROBE_CRAFT player={name} id={} input={input} target={target} cancelled={}",
                    event.recipe_id, event.cancelled
                ));
            }
        })
    }
}

impl EventHandler<PrepareItemCraftEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut PrepareItemCraftEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let name = event.player.gameprofile.name.as_str();
            if !matches!(
                name,
                "CraftReference" | "CraftVeto" | "CraftPrepVeto" | "CraftEdit"
            ) {
                return;
            }
            event.cancelled = name == "CraftPrepVeto";
            if let Some(context) = self.0.upgrade() {
                context.log(format!(
                    "PROBE_CRAFT_PREPARE player={name} id={} cancelled={}",
                    event.recipe_id, event.cancelled
                ));
            }
        })
    }
}

impl EventHandler<InventoryDragEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut InventoryDragEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let name = event.player.gameprofile.name.as_str();
            let (cancel, edit) = match name {
                "DragNativeAllow" => (false, false),
                "DragNativeVeto" => (true, false),
                "DragNativeEdit" => (false, true),
                _ => return,
            };
            let screen = event
                .player
                .current_screen_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let cursor_stack = screen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_behaviour()
                .cursor_stack
                .clone();
            let cursor = cursor_stack
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .item_count;
            let target = event.player.inventory.get_slot(4).item_count;
            if edit {
                event
                    .player
                    .inventory
                    .set_slot(4, ItemStack::new(7, &Item::GRASS_BLOCK));
            }
            event.cancelled = cancel;
            if let Some(context) = self.0.upgrade() {
                context.log(format!("PROBE_CURSOR_DRAG player={name} cursor={cursor} target={target} cancelled={cancel} edited={edit}"));
            }
        })
    }
}

impl EventHandler<InventoryCloseEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut InventoryCloseEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let name = &event.player.gameprofile.name;
            if !name.starts_with("Close") {
                return;
            }
            let before = event.player.inventory.get_slot(0).item_count;
            let (screen, cursor) = {
                let handler = event
                    .player
                    .player_screen_handler
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (
                    handler.get_slot(36).get_cloned_stack().item_count,
                    handler.get_behaviour().cursor_stack.clone(),
                )
            };
            let count = cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .item_count;
            let mutate = name == "CloseMutNative" && !CLOSE_MUTATED.swap(true, Ordering::Relaxed);
            if mutate {
                event
                    .player
                    .inventory
                    .set_slot(0, ItemStack::new(7, &Item::GRASS_BLOCK));
                *cursor
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    ItemStack::new(3, &Item::GRASS_BLOCK);
            }
            if let Some(context) = self.0.upgrade() {
                context.log(format!("PROBE_INVENTORY_CLOSE player={name} window={:?} before={before} screen={screen} cursor={count} mutated={mutate}", event.window_type));
            }
        })
    }
}

impl EventHandler<PlayerInteractEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut PlayerInteractEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let name = &event.player.gameprofile.name;
            if (!name.starts_with("Use") && !name.starts_with("Eat"))
                || event.action != InteractAction::RightClickAir
            {
                return;
            }
            let held = event.player.inventory.get_slot(0).item_count;
            let screen = event
                .player
                .player_screen_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_slot(36)
                .get_cloned_stack()
                .item_count;
            let head = event
                .player
                .inventory
                .entity_equipment
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&EquipmentSlot::HEAD)
                .item_count;
            let active = event
                .player
                .living_entity
                .item_use_time
                .load(Ordering::Relaxed);
            let mutate = name.contains("Mut");
            if mutate {
                event
                    .player
                    .inventory
                    .set_slot(0, ItemStack::new(7, &Item::GRASS_BLOCK));
            }
            event.cancelled = name.contains("Deny");
            if let Some(context) = self.0.upgrade() {
                context.log(format!("PROBE_ITEM_USE player={name} held={held} screen={screen} head={head} active={active} air={} cancelled={} mutated={mutate}", event.clicked_pos.is_none(), event.cancelled));
            }
        })
    }
}

impl EventHandler<InventoryClickEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut InventoryClickEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let name = &event.player.gameprofile.name;
            let storage = matches!(
                name.as_str(),
                "StoreClickDeny" | "StoreMutNative" | "StoreDropDeny"
            );
            if !name.starts_with("Click")
                && !name.starts_with("Erase")
                && name != "LegacyTxnNative"
                && !storage
            {
                return;
            }
            let before = event.player.inventory.get_slot(0).item_count;
            let handler = if storage {
                event
                    .player
                    .current_screen_handler
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            } else {
                event.player.player_screen_handler.clone()
            };
            let slot = handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_behaviour()
                .slots[if storage { 0 } else { 36 }]
            .clone();
            let screen = slot.get_cloned_stack().item_count;
            let clicked = event
                .clicked_item
                .as_ref()
                .map_or(0, |stack| stack.item_count);
            let cursor = event.cursor.as_ref().map_or(0, |stack| stack.item_count);
            let mutate = name == "ClickMutNative" || name == "StoreMutNative";
            if mutate {
                slot.set_stack(ItemStack::new(7, &Item::GRASS_BLOCK));
            }
            event.cancelled = name.starts_with("ClickDeny")
                || name == "StoreClickDeny"
                || (name == "ClickCurNative" && event.raw_slot == -999);
            if let Some(context) = self.0.upgrade() {
                context.log(format!("PROBE_INVENTORY_CLICK player={name} slot={} raw={} type={:?} button={} window={:?} clicked={clicked} cursor={cursor} before={before} screen_before={screen} cancelled={} mutated={mutate}", event.slot, event.raw_slot, event.click_type, event.hotbar_button, event.window_type, event.cancelled));
            }
        })
    }
}

impl EventHandler<InventoryInteractEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut InventoryInteractEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let name = &event.player.gameprofile.name;
            if !name.starts_with("InvDeny")
                && !name.starts_with("InvMut")
                && name != "DropDenyNative"
                && name != "LegacyDropDeny"
                && !name.starts_with("Click")
                && !name.starts_with("Erase")
                && !matches!(
                    name.as_str(),
                    "StoreClickDeny" | "StoreMutNative" | "StoreDropDeny"
                )
            {
                return;
            }
            let count = event.player.inventory.get_slot(0).item_count;
            let screen = event
                .player
                .player_screen_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_slot(36)
                .get_cloned_stack()
                .item_count;
            let mutate = name.starts_with("InvMut");
            if mutate {
                event
                    .player
                    .inventory
                    .set_slot(0, ItemStack::new(7, &Item::GRASS_BLOCK));
            }
            event.cancelled = name.starts_with("InvDeny");
            if let Some(context) = self.0.upgrade() {
                context.log(format!("PROBE_INVENTORY_INTERACT player={name} before={count} screen_before={screen} cancelled={} mutated={mutate}", event.cancelled));
            }
        })
    }
}

impl EventHandler<PlayerDropItemEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut PlayerDropItemEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let name = &event.player.gameprofile.name;
            if name != "DropDenyNative" && name != "LegacyDropDeny" && name != "StoreDropDeny" {
                return;
            }
            let count = event.player.inventory.get_slot(0).item_count;
            let screen = event
                .player
                .player_screen_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_slot(36)
                .get_cloned_stack()
                .item_count;
            event.cancelled = true;
            if let Some(context) = self.0.upgrade() {
                context.log(format!("PROBE_INVENTORY_DROP before={count} screen_before={screen} count={} cancelled=true player={name}", event.count));
            }
        })
    }
}

impl EventHandler<InventoryCreativeEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut InventoryCreativeEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let name = &event.player.gameprofile.name;
            if !name.starts_with("Create") && !name.starts_with("Erase") {
                return;
            }
            let count = event.player.inventory.get_slot(0).item_count;
            let screen_count = event
                .player
                .player_screen_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_slot(36)
                .get_cloned_stack()
                .item_count;
            let mutate = name.contains("Mut");
            if mutate {
                event
                    .player
                    .inventory
                    .set_slot(0, ItemStack::new(7, &Item::GRASS_BLOCK));
            }
            event.cancelled = name.starts_with("EraseDeny")
                || name.contains("Deny")
                    && (name.ends_with("Java") || event.slot == 44 || event.slot < 0);
            if let Some(context) = self.0.upgrade() {
                context.log(format!("PROBE_CREATIVE player={name} slot={} item={} count={} before={count} screen_before={screen_count} cancelled={} mutated={mutate}",event.slot,event.item_id,event.item_count,event.cancelled));
            }
        })
    }
}

impl EventHandler<PacketReceivedEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut PacketReceivedEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if event.player.gameprofile.name != "TrackerDataNative"
                || event.packet_id != pumpkin_data::bedrock_packet::NETWORK_STACK_LATENCY
                || event.payload.get(..8) != Some(TRACKER_MOTION_TRIGGER.to_le_bytes().as_slice())
            {
                return;
            }
            let world = event.player.world();
            let target = world
                .entities
                .load()
                .iter()
                .find(|target| {
                    let entity = target.get_entity();
                    entity.entity_type.id == EntityType::PIG.id && entity.pos.load().x < 6.0
                })
                .cloned();
            if let Some(target) = target {
                let entity = target.get_entity();
                let mut velocity = entity.velocity.load();
                velocity.z = 0.1;
                entity.velocity.store(velocity);
                entity.velocity_dirty.store(true, Ordering::Relaxed);
            }
        })
    }
}

impl EventHandler<PacketSentEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut PacketSentEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let name = &event.player.gameprofile.name;
            let id = if name == "TrackerNative"
                && event.packet_id == pumpkin_data::bedrock_packet::ADD_ACTOR
            {
                VarLong::read(&mut event.payload.as_ref())
                    .ok()
                    .and_then(|id| i32::try_from(id.0).ok())
            } else if name == "TrackerDataNative"
                && event.packet_id == pumpkin_data::bedrock_packet::SET_ACTOR_MOTION
            {
                VarULong::read(&mut event.payload.as_ref())
                    .ok()
                    .and_then(|id| i32::try_from(id.0).ok())
            } else {
                None
            };
            let Some(id) = id else {
                return;
            };
            let world = event.player.world();
            let Some(target) = world.get_entity_by_id(id) else {
                return;
            };
            let entity = target.get_entity();
            if entity.entity_type.id != EntityType::PIG.id || entity.pos.load().x >= 6.0 {
                return;
            }
            event.cancelled = true;
            if let Some(context) = self.0.upgrade() {
                context.log(format!("PROBE_TRACKING_REMOVE_BEGIN player={name} id={id}"));
                world.remove_entity(target.as_ref());
                context.log(format!(
                    "PROBE_TRACKING_REMOVE_END player={name} id={id} removed={}",
                    world.get_entity_by_id(id).is_none()
                ));
            }
        })
    }
}

impl EventHandler<PlayerItemHeldEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut PlayerItemHeldEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let name = &event.player.gameprofile.name;
            if !name.starts_with("Hold")
                && !name.starts_with("Mutate")
                && name != "LegacyHeldNative"
                && name != "LegacyTxnNative"
            {
                return;
            }
            let inventory = &event.player.inventory;
            let current = inventory.get_selected_slot();
            let count = inventory.get_slot(0).item_count;
            let mutate =
                name.starts_with("Mutate") && event.previous_slot == 5 && event.new_slot == 0;
            if mutate {
                inventory.set_slot(0, ItemStack::new(7, &Item::DIAMOND_BLOCK));
            } else if (name.starts_with("Hold") && event.previous_slot == 5 && event.new_slot <= 1)
                || name == "LegacyHeldNative"
            {
                event.cancelled = true;
            }
            if let Some(context) = self.0.upgrade() {
                context.log(format!(
                    "PROBE_PICK_HELD player={name} previous={} next={} current={current} count0={count} cancelled={} mutated={mutate}",
                    event.previous_slot, event.new_slot, event.cancelled
                ));
            }
        })
    }
}

impl EventHandler<BedrockConnectionPacketReceivedEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut BedrockConnectionPacketReceivedEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let current = sculith_world::CURRENT_BEDROCK_MC_PROTOCOL;
            let rewrite = [SRequestNetworkSettings::PACKET_ID, SLogin::PACKET_ID]
                .contains(&event.packet_id)
                && event.payload.get(..4) == Some((current - 1).to_be_bytes().as_slice());
            if rewrite {
                let mut payload = event.payload.to_vec();
                payload[..4].copy_from_slice(&current.to_be_bytes());
                event.payload = payload.into();
            }
            if let Some(context) = self.0.upgrade() {
                context.log(format!("PROBE_BEDROCK_CONNECTION_RX connection={} protocol={} id={} rewritten={rewrite}",event.connection_id,event.protocol_version,event.packet_id));
            }
        })
    }
}

impl EventHandler<BedrockConnectionPacketSentEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut BedrockConnectionPacketSentEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(context) = self.0.upgrade() {
                context.log(format!(
                    "PROBE_BEDROCK_CONNECTION_TX connection={} protocol={} id={}",
                    event.connection_id, event.protocol_version, event.packet_id
                ));
            }
        })
    }
}

impl EventHandler<JavaConnectionPacketReceivedEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        server: &'a Arc<Server>,
        event: &'a mut JavaConnectionPacketReceivedEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(context) = self.0.upgrade() {
                context.log(format!(
                    "PROBE_JAVA_CONNECTION_RX connection={} state={:?} id={}",
                    event.connection_id, event.state, event.packet_id
                ));
                if event.state == ConnectionState::Login
                    && event.packet_id == SLoginStart::to_id(event.version)
                    && SLoginStart::read(&mut &event.payload[..], &event.version)
                        .is_ok_and(|packet| packet.name.as_ref() == "PackEditor")
                {
                    match edit_geometry(server).await {
                        Ok(()) => context.log(
                            "PROBE_PACK_EDIT old revision survived validation; geometry updated",
                        ),
                        Err(error) => context.log(format!("PROBE_PACK_EDIT failed: {error}")),
                    }
                }
            }
        })
    }
}

async fn edit_geometry(server: &Server) -> Result<(), String> {
    const GEOMETRY: &str = "models/entity/sculith_statue.geo.json";
    let packs = server
        .shared_resources
        .as_ref()
        .ok_or("Missing shared resource pack")?;
    let original = packs.snapshot();
    let result = server
        .edit_shared_resources(vec![
            PackEdit::Put {
                path: "rejected.txt".into(),
                bytes: b"rejected".to_vec(),
            },
            PackEdit::Remove {
                path: GEOMETRY.into(),
            },
        ])
        .await;
    if result.is_ok() || !Arc::ptr_eq(&original, &packs.snapshot()) {
        return Err("An incomplete model changed the active pack".into());
    }

    let mut geometry: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../crates/sculith-packs/tests/fixtures/pack/models/entity/sculith_statue.geo.json"
    ))
    .map_err(|error| error.to_string())?;
    geometry["minecraft:geometry"][0]["bones"][1]["cubes"][0]["origin"][2] = (-2).into();
    let bytes = serde_json::to_vec(&geometry).map_err(|error| error.to_string())?;
    let revision = server
        .edit_shared_resources(vec![PackEdit::Put {
            path: GEOMETRY.into(),
            bytes,
        }])
        .await
        .map_err(|error| error.to_string())?;
    if revision.bundle.bedrock_id == original.bundle.bedrock_id
        || revision.bundle.java_id == original.bundle.java_id
    {
        return Err("Geometry edit did not change both pack IDs".into());
    }
    Ok(())
}

impl EventHandler<JavaConnectionPacketSentEvent> for Observer {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut JavaConnectionPacketSentEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let replaced = event.state == ConnectionState::Status
                && event.packet_id == CStatusResponse::to_id(event.version);
            if replaced {
                event.cancelled = true;
                event
                    .extra_packets
                    .push((event.packet_id, event.payload.clone()));
            }
            if let Some(context) = self.0.upgrade() {
                context.log(format!(
                    "PROBE_JAVA_CONNECTION_TX connection={} state={:?} id={} replaced={replaced}",
                    event.connection_id, event.state, event.packet_id
                ));
            }
        })
    }
}
