use pumpkin_plugin_api::events::{
    DialogClickActionEvent, EventData, EventHandler, EventPriority, PacketReceivedEvent,
    PacketSentEvent, PlayerJoinEvent,
};
use pumpkin_plugin_api::forms::{
    CustomFormBuilder, FormResponseEvent, ModalFormBuilder, SimpleFormBuilder,
};
use pumpkin_plugin_api::text::TextComponent;
use pumpkin_plugin_api::wit::pumpkin::plugin::player::Player;
use pumpkin_plugin_api::wit::pumpkin::plugin::sounds::SoundCategory;
use pumpkin_plugin_api::{Context, Plugin, PluginMetadata, Result, Server, register_plugin};
use std::sync::atomic::{AtomicU32, Ordering};
use std::{collections::BTreeMap, sync::Mutex};
mod models;
mod packets;
struct AudioProbe;
struct Joined;
struct Received(AtomicU32);
struct Sent;
struct DialogReplies;
struct FormReplies;
static FORMS: Mutex<BTreeMap<String, (u32, u8)>> = Mutex::new(BTreeMap::new());

fn open_form(player: &Player, stage: u8) {
    let text = TextComponent::text;
    let form = match stage {
        0 => SimpleFormBuilder::new(text("Sculith forms"), text("Select a button."))
            .button(text("Continue"), None)
            .button(text("Exit"), None)
            .build(),
        1 => ModalFormBuilder::new(text("Sculith forms"), text("Choose No."))
            .button1(text("Yes"))
            .button2(text("No"))
            .build(),
        2 => CustomFormBuilder::new(text("Sculith inputs"))
            .label(text("Input values"))
            .toggle(text("Enabled"), false)
            .slider(text("Range"), -1.0, 5.0, 0.5, 2.5)
            .dropdown(text("Selection"), vec!["A".into(), "B".into()], 1)
            .step_slider(text("Step"), vec!["One".into(), "Two".into()], 0)
            .input(text("Text"), "Hint", "가나다😀")
            .build(),
        3 => SimpleFormBuilder::new(text("Sculith forms"), text("Close this form."))
            .button(text("Unused"), None)
            .build(),
        _ => {
            tracing::info!(player=%player.get_name(), "PROBE_FORMS_COMPLETE");
            return;
        }
    };
    match player.open_form(form) {
        Ok(id) => {
            FORMS.lock().unwrap().insert(player.get_name(), (id, stage));
            let busy = player.open_form(
                SimpleFormBuilder::new(text("Busy"), text("This must not open"))
                    .button(text("Unused"), None)
                    .build(),
            );
            assert!(busy.is_err(), "second pending form accepted");
            tracing::info!(player=%player.get_name(), id, stage, "PROBE_FORM_OPENED");
        }
        Err(error) => tracing::error!(%error, "PROBE_FORM_FAILED"),
    }
}

impl EventHandler<FormResponseEvent> for FormReplies {
    fn handle(
        &self,
        _server: Server,
        event: EventData<FormResponseEvent>,
    ) -> EventData<FormResponseEvent> {
        let state = FORMS.lock().unwrap().remove(&event.player.get_name());
        let Some((id, stage)) = state else {
            return event;
        };
        assert_eq!(event.form_id, id);
        let data = event
            .response_data
            .as_deref()
            .map(|data| serde_json::from_str::<serde_json::Value>(data).unwrap());
        match stage {
            0 => assert_eq!(data, Some(serde_json::json!(0))),
            1 => assert_eq!(data, Some(serde_json::json!(false))),
            2 => assert_eq!(
                data,
                Some(serde_json::json!([null, true, 2.5, 1, 0, "가나다😀"]))
            ),
            3 => assert!(data.is_none()),
            _ => unreachable!(),
        }
        tracing::info!(player=%event.player.get_name(), stage, "PROBE_FORM_REPLY");
        open_form(&event.player, stage + 1);
        event
    }
}

impl EventHandler<DialogClickActionEvent> for DialogReplies {
    fn handle(
        &self,
        _server: Server,
        event: EventData<DialogClickActionEvent>,
    ) -> EventData<DialogClickActionEvent> {
        if models::action(&event.player, &event.id) {
            return event;
        }
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
        if packets::on_join(&event.player) {
            return event;
        }
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
        if name.starts_with("Form") {
            open_form(&event.player, 0);
        }
        models::on_join(&event.player);
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
        mut event: EventData<PacketSentEvent>,
    ) -> EventData<PacketSentEvent> {
        if [86, 87, 119, 121].contains(&event.packet_id) {
            tracing::info!(player=%event.player.get_name(),id=event.packet_id,bytes=event.raw_payload.len(),"PROBE_SENT");
        }
        packets::sent(&mut event);
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
        ctx.register_event_handler::<pumpkin_plugin_api::events::EntitySpawnEvent, _>(
            models::Spawned,
            EventPriority::Normal,
            true,
        )?;
        ctx.register_event_handler::<FormResponseEvent, _>(
            FormReplies,
            EventPriority::Normal,
            true,
        )?;
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
