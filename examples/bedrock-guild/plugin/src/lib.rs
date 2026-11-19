use pumpkin_plugin_api::events::{EventData, EventHandler, EventPriority, PlayerLeaveEvent};
use pumpkin_plugin_api::forms::{FormResponseEvent, SimpleFormBuilder};
use pumpkin_plugin_api::persistent_data::PersistentDataHolder;
use pumpkin_plugin_api::text::TextComponent;
use pumpkin_plugin_api::wit::pumpkin::plugin::player::Player;
use pumpkin_plugin_api::{Context, Plugin, PluginMetadata, Result, Server, register_plugin};

struct Expedition;
struct GuildMenu;

impl pumpkin_plugin_api::commands::CommandHandler for GuildMenu {
    fn handle(
        &self,
        sender: pumpkin_plugin_api::command::CommandSender,
        _: Server,
        _: pumpkin_plugin_api::command::ConsumedArgs,
    ) -> Result<i32, pumpkin_plugin_api::command::CommandError> {
        if let Some(player) = sender.as_player() {
            menu(&player);
            return Ok(1);
        }
        sender.send_message(TextComponent::text("게임에서 /guild 를 입력하세요."));
        Ok(0)
    }
}
struct Selected;
struct Left;

use std::{collections::BTreeMap, sync::Mutex};
static ACTIVE: Mutex<BTreeMap<(u64, u64), u32>> = Mutex::new(BTreeMap::new());

fn identity(player: &Player) -> (u64, u64) {
    let id = player.get_id();
    (id.high, id.low)
}
fn consume(name: (u64, u64), id: u32) -> bool {
    let mut active = ACTIVE.lock().unwrap();
    if active.get(&name) != Some(&id) {
        return false;
    }
    active.remove(&name);
    true
}

impl EventHandler<PlayerLeaveEvent> for Left {
    fn handle(&self, _: Server, event: EventData<PlayerLeaveEvent>) -> EventData<PlayerLeaveEvent> {
        ACTIVE.lock().unwrap().remove(&identity(&event.player));
        event
    }
}

fn selection(player: &Player, choice: &str) {
    let accepted = player
        .get_int("sculith-guild", "ridge-survey")
        .unwrap_or_default()
        == 1;
    let message = match (choice, accepted) {
        ("accept", false) => {
            player.set_int("sculith-guild", "ridge-survey", 1);
            "의뢰 수락 · 산등성이 조사"
        }
        ("accept", true) => "진행 중인 의뢰입니다 · 산등성이 조사",
        ("list", true) => "내 의뢰 · 산등성이 조사",
        ("list", false) => "아직 수락한 의뢰가 없습니다.",
        _ => return,
    };
    player.send_system_message(TextComponent::text(message), false);
    menu(player);
}

fn menu(player: &Player) {
    let label = TextComponent::text;
    let form = SimpleFormBuilder::new(label("길드 의뢰"), label("산등성이 조사"))
        .button(label("의뢰 수락"), None)
        .button(label("내 의뢰"), None)
        .button(label("닫기"), None)
        .build();
    match player.open_form(form) {
        Ok(id) => {
            ACTIVE.lock().unwrap().insert(identity(player), id);
            tracing::info!(name=%player.get_name(), id, "SCULITH_UI_OPEN");
        }
        Err(error) => tracing::error!(%error, "SCULITH_UI_ERROR"),
    }
}

impl EventHandler<FormResponseEvent> for Selected {
    fn handle(
        &self,
        _: Server,
        event: EventData<FormResponseEvent>,
    ) -> EventData<FormResponseEvent> {
        if !consume(identity(&event.player), event.form_id) {
            return event;
        }
        let choice = match event.response_data.as_deref().map(str::trim) {
            Some("0") => "accept",
            Some("1") => "list",
            Some("2" | "null") | None => "close",
            _ => "unknown",
        };
        tracing::info!(name=%event.player.get_name(), id=event.form_id, choice, "SCULITH_UI_CHOICE");
        selection(&event.player, choice);
        event
    }
}
impl Plugin for Expedition {
    fn new() -> Self {
        Self
    }
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "sculith-guild".into(),
            version: "0.1.0".into(),
            authors: vec![],
            description: "스컬리스 길드 의뢰".into(),
            permissions: vec![],
            dependencies: vec![],
        }
    }
    fn on_load(&self, ctx: Context) -> Result<()> {
        use pumpkin_plugin_api::{
            commands::Command,
            wit::pumpkin::plugin::permission::{Permission, PermissionDefault},
        };
        let permission = "sculith-guild:guild";
        ctx.register_permission(&Permission {
            node: permission.into(),
            description: "길드 의뢰 메뉴".into(),
            default: PermissionDefault::Allow,
            children: vec![],
        })?;
        let command = Command::new(&["guild".to_string()], "길드 의뢰를 엽니다").execute(GuildMenu);
        ctx.register_command(command, permission);
        ctx.register_event_handler::<PlayerLeaveEvent, _>(Left, EventPriority::Normal, true)?;
        ctx.register_event_handler::<FormResponseEvent, _>(Selected, EventPriority::Normal, true)?;
        Ok(())
    }
}
register_plugin!(Expedition);
