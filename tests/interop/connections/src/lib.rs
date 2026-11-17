//! Native connection-event fixture, installed only into the interop scratch server.

use std::sync::{Arc, LazyLock, Weak};

use pumpkin_core::{
    plugin::server::packet::{
        BedrockConnectionPacketReceivedEvent, BedrockConnectionPacketSentEvent,
        JavaConnectionPacketReceivedEvent, JavaConnectionPacketSentEvent,
    },
    plugin::{
        BoxFuture, Context, EventHandler, EventPriority, Plugin, PluginFuture, PluginMetadata,
    },
    server::Server,
};
use pumpkin_protocol::{
    ConnectionState,
    bedrock::server::{login::SLogin, request_network_settings::SRequestNetworkSettings},
    java::client::status::CStatusResponse,
    packet::{MultiVersionJavaPacket, Packet},
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
                observer,
                EventPriority::Lowest,
                true,
            );
            context.log("PROBE_CONNECTIONS_LOADED");
            Ok(())
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
