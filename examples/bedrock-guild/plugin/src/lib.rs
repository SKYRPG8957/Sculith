#[cfg(not(feature = "companion"))]
use pumpkin_plugin_api::events::DialogClickActionEvent;
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
        server: Server,
        _: pumpkin_plugin_api::command::ConsumedArgs,
    ) -> Result<i32, pumpkin_plugin_api::command::CommandError> {
        if let Some(player) = sender.as_player() {
            menu(&server, &player);
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

fn selection(server: &Server, player: &Player, choice: &str) {
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
    menu(server, player);
}

fn menu(_server: &Server, player: &Player) {
    #[cfg(not(feature = "companion"))]
    if player.as_java().is_some() {
        use pumpkin_plugin_api::wit::pumpkin::plugin::server::CommandSender;
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(1);
        // Native shared form IDs occupy the upper half. Never reuse a Java action ID.
        let Ok(id) = NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            (value < 0x8000_0000).then(|| value + 1)
        }) else {
            tracing::error!("Guild action IDs exhausted");
            return;
        };
        let username = player.get_name();
        if username.is_empty()
            || username.len() > 16
            || !username
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            tracing::error!("Invalid Java profile name");
            return;
        }
        ACTIVE.lock().unwrap().insert(identity(player), id);
        let body = include_str!("dialog-template.json").replace("REQUEST", &id.to_string());
        _server.execute_command(
            &format!("dialog show @a[name={username},limit=1] {body}"),
            CommandSender::Console,
        );
        tracing::info!(name=%player.get_name(),id,"SCULITH_UI_OPEN");
        return;
    }
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
        server: Server,
        event: EventData<FormResponseEvent>,
    ) -> EventData<FormResponseEvent> {
        if event.form_id < 0x8000_0000 || !consume(identity(&event.player), event.form_id) {
            return event;
        }
        let choice = match event.response_data.as_deref().map(str::trim) {
            Some("0") => "accept",
            Some("1") => "list",
            Some("2" | "null") | None => "close",
            _ => "unknown",
        };
        tracing::info!(name=%event.player.get_name(), id=event.form_id, choice, "SCULITH_UI_CHOICE");
        selection(&server, &event.player, choice);
        event
    }
}
#[cfg(not(feature = "companion"))]
struct Clicked;
#[cfg(not(feature = "companion"))]
impl EventHandler<DialogClickActionEvent> for Clicked {
    fn handle(
        &self,
        server: Server,
        event: EventData<DialogClickActionEvent>,
    ) -> EventData<DialogClickActionEvent> {
        let Some((num, idx)) = event
            .id
            .strip_prefix("sculith:guild/")
            .and_then(|s| s.split_once('/'))
        else {
            return event;
        };
        let Ok(id) = num.parse::<u32>() else {
            return event;
        };
        let choice = match idx {
            "0" => "accept",
            "1" => "list",
            "2" => "close",
            _ => return event,
        };
        if !consume(identity(&event.player), id) {
            return event;
        }
        tracing::info!(name=%event.player.get_name(),id,choice,"SCULITH_UI_CHOICE");
        selection(&server, &event.player, choice);
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
            version: "0.2.0".into(),
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
        #[cfg(not(feature = "companion"))]
        ctx.register_event_handler::<DialogClickActionEvent, _>(
            Clicked,
            EventPriority::Normal,
            true,
        )?;
        ctx.register_event_handler::<PlayerLeaveEvent, _>(Left, EventPriority::Normal, true)?;
        ctx.register_event_handler::<FormResponseEvent, _>(Selected, EventPriority::Normal, true)?;
        Ok(())
    }
}
register_plugin!(Expedition);
