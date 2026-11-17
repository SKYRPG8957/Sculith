use pumpkin_plugin_api::events::{
    DialogClickActionEvent, EventData, EventHandler, EventPriority, PacketReceivedEvent,
    PacketSentEvent, PlayerJoinEvent,
};
use pumpkin_plugin_api::wit::pumpkin::plugin::sounds::SoundCategory;
use pumpkin_plugin_api::{Context, Plugin, PluginMetadata, Result, Server, register_plugin};
use std::sync::atomic::{AtomicU32, Ordering};
struct AudioProbe;
struct Joined;
struct Received(AtomicU32);
struct Sent;
struct DialogReplies;

impl EventHandler<DialogClickActionEvent> for DialogReplies {
    fn handle(
        &self,
        _server: Server,
        event: EventData<DialogClickActionEvent>,
    ) -> EventData<DialogClickActionEvent> {
        let small = include_bytes!(
            "../../../../crates/pumpkin-protocol/tests/fixtures/java_custom_click_small_777.bin"
        );
        let large = include_bytes!(
            "../../../../crates/pumpkin-protocol/tests/fixtures/java_custom_click_large_777.bin"
        );
        let case = match (event.id.as_str(), event.payload.as_deref()) {
            ("sculith:form/7/cancel", None) => "none",
            ("sculith:input/7/submit", Some(payload)) if payload == &small[24..] => "small",
            ("sculith:input/7/submit", Some(payload)) if payload == &large[25..] => "large",
            _ => return event,
        };
        tracing::info!(player=%event.player.get_name(), case=%case, "PROBE_DIALOG_REPLY");
        event
    }
}
impl EventHandler<PlayerJoinEvent> for Joined {
    fn handle(
        &self,
        _server: Server,
        event: EventData<PlayerJoinEvent>,
    ) -> EventData<PlayerJoinEvent> {
        let name = event.player.get_name();
        tracing::info!(player=%name, "PROBE_JOIN");
        event.player.play_custom_sound_at(
            (-1.25, -60.5, 3.875),
            "sculith:demo_tone",
            SoundCategory::Master,
            2.5,
            0.75,
        );
        event
            .player
            .stop_custom_sound(Some("sculith:demo_tone"), None);
        event.player.get_world().play_custom_sound(
            "sculith:demo_tone",
            SoundCategory::Master,
            (1.125, -60.25, 4.5),
            0.625,
            1.25,
        );
        event
            .player
            .stop_custom_sound(Some("sculith:demo_tone"), None);
        event
    }
}
impl EventHandler<PacketReceivedEvent> for Received {
    fn handle(
        &self,
        _server: Server,
        event: EventData<PacketReceivedEvent>,
    ) -> EventData<PacketReceivedEvent> {
        if self.0.fetch_add(1, Ordering::Relaxed) < 12 {
            tracing::info!(player=%event.player.get_name(),id=event.packet_id,bytes=event.raw_payload.len(),"PROBE_RECEIVED");
        }
        event
    }
}
impl EventHandler<PacketSentEvent> for Sent {
    fn handle(
        &self,
        _server: Server,
        event: EventData<PacketSentEvent>,
    ) -> EventData<PacketSentEvent> {
        if [86, 87, 119, 121].contains(&event.packet_id) {
            tracing::info!(player=%event.player.get_name(),id=event.packet_id,bytes=event.raw_payload.len(),"PROBE_SENT");
        }
        event
    }
}
impl Plugin for AudioProbe {
    fn new() -> Self {
        Self
    }
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "sculith-wasm-audio-probe".into(),
            version: "0.0.0".into(),
            authors: vec![],
            description: "Resource dispatch interop fixture".into(),
            dependencies: vec![],
            permissions: vec![],
        }
    }
    fn on_load(&self, ctx: Context) -> Result<()> {
        ctx.register_event_handler::<DialogClickActionEvent, _>(
            DialogReplies,
            EventPriority::Normal,
            true,
        )?;
        ctx.register_event_handler::<PlayerJoinEvent, _>(Joined, EventPriority::Normal, true)?;
        ctx.register_event_handler::<PacketReceivedEvent, _>(
            Received(AtomicU32::new(0)),
            EventPriority::Lowest,
            true,
        )?;
        ctx.register_event_handler::<PacketSentEvent, _>(Sent, EventPriority::Lowest, true)?;
        tracing::info!("PROBE_LOADED");
        Ok(())
    }
}
register_plugin!(AudioProbe);
