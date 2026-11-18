//! Native connection-event fixture, installed only into the interop scratch server.

use std::sync::{Arc, LazyLock, Weak, atomic::Ordering};

use pumpkin_core::{
    plugin::inventory::inventory_creative::InventoryCreativeEvent,
    plugin::inventory::inventory_interact::InventoryInteractEvent,
    plugin::player::inventory_interact::InventoryClickEvent,
    plugin::player::item_held::PlayerItemHeldEvent,
    plugin::player::player_drop_item::PlayerDropItemEvent,
    plugin::server::packet::{
        BedrockConnectionPacketReceivedEvent, BedrockConnectionPacketSentEvent,
        JavaConnectionPacketReceivedEvent, JavaConnectionPacketSentEvent, PacketReceivedEvent,
        PacketSentEvent,
    },
    plugin::{
        BoxFuture, Context, EventHandler, EventPriority, Plugin, PluginFuture, PluginMetadata,
    },
    server::Server,
};
use pumpkin_data::{entity::EntityType, item::Item, item_stack::ItemStack};
use pumpkin_protocol::{
    ConnectionState,
    bedrock::server::{login::SLogin, request_network_settings::SRequestNetworkSettings},
    codec::var_long::VarLong,
    codec::var_ulong::VarULong,
    java::client::status::CStatusResponse,
    packet::{MultiVersionJavaPacket, Packet},
    serial::PacketRead,
};

#[unsafe(no_mangle)]
pub static PUMPKIN_API_VERSION: u32 = pumpkin_core::plugin::PLUGIN_API_VERSION;

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
            context.register_event::<InventoryClickEvent, _>(observer, EventPriority::Lowest, true);
            context.log("PROBE_CONNECTIONS_LOADED");
            Ok(())
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
            if !name.starts_with("Click") && !name.starts_with("Erase") && name != "LegacyTxnNative"
            {
                return;
            }
            let before = event.player.inventory.get_slot(0).item_count;
            let screen = event
                .player
                .player_screen_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_slot(36)
                .get_cloned_stack()
                .item_count;
            let clicked = event
                .clicked_item
                .as_ref()
                .map_or(0, |stack| stack.item_count);
            let cursor = event.cursor.as_ref().map_or(0, |stack| stack.item_count);
            let mutate = name == "ClickMutNative";
            if mutate {
                event
                    .player
                    .inventory
                    .set_slot(0, ItemStack::new(7, &Item::GRASS_BLOCK));
            }
            event.cancelled = name.starts_with("ClickDeny")
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
            if name != "DropDenyNative" && name != "LegacyDropDeny" {
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
            let current = pumpkin_world::CURRENT_BEDROCK_MC_PROTOCOL;
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
        _server: &'a Arc<Server>,
        event: &'a mut JavaConnectionPacketReceivedEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(context) = self.0.upgrade() {
                context.log(format!(
                    "PROBE_JAVA_CONNECTION_RX connection={} state={:?} id={}",
                    event.connection_id, event.state, event.packet_id
                ));
            }
        })
    }
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
