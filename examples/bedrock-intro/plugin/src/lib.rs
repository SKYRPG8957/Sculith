use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
    },
};

use pumpkin_plugin_api::{
    Context, Plugin, PluginMetadata, Result, Server,
    command::{CommandError, CommandSender, ConsumedArgs},
    commands::{Command, CommandHandler},
    events::{
        DialogClickActionEvent, EventData, EventHandler, EventPriority, PlayerJoinEvent,
        PlayerLeaveEvent,
    },
    forms::{FormResponseEvent, SimpleFormBuilder},
    permission::{Permission, PermissionDefault},
    persistent_data::PersistentDataHolder,
    player::Player,
    register_plugin,
    scheduler::SchedulerExt,
    server::CommandSender as ServerCommandSender,
    text::TextComponent,
    uuid::Uuid,
};
use serde_json::json;

const PAGES: [&str; 3] = ["engine", "arena", "status"];
const BUTTONS: [&str; 3] = ["이전", "닫기", "다음"];
const ACTION_PREFIX: &str = "sculith:intro/";
const NAMESPACE: &str = "sculith-intro";
const SEEN_INTRO: &str = "seen-intro";

type PlayerId = (u64, u64);

struct Page {
    id: u32,
    index: usize,
}

static PENDING: Mutex<BTreeMap<PlayerId, Page>> = Mutex::new(BTreeMap::new());
static WAITING_FOR_LOAD: Mutex<BTreeSet<PlayerId>> = Mutex::new(BTreeSet::new());
static NEXT_JAVA_ID: AtomicU32 = AtomicU32::new(1);

fn player_id(player: &Player) -> PlayerId {
    let id = player.get_id();
    (id.high, id.low)
}

fn open_first_page(server: &Server, player: &Player) -> bool {
    if let Err(error) = show_page(server, player, 0) {
        tracing::warn!(%error, "Unable to open the intro page");
        return false;
    }
    player.set_bool(NAMESPACE, SEEN_INTRO, true);
    true
}

fn open_new_player_intros(server: Server) {
    let waiting: Vec<_> = WAITING_FOR_LOAD.lock().unwrap().iter().copied().collect();
    for (high, low) in waiting {
        let Some(player) = server.get_player_by_uuid(Uuid { high, low }) else {
            continue;
        };
        if player.is_client_ready() && WAITING_FOR_LOAD.lock().unwrap().remove(&(high, low)) {
            open_first_page(&server, &player);
        }
    }
}

fn java_target(player: &Player) -> Option<String> {
    let name = player.get_name();
    if name.is_empty()
        || name.len() > 16
        || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        return None;
    }
    Some(format!("@a[name={name},limit=1]"))
}

fn show_page(server: &Server, player: &Player, index: usize) -> Result<(), String> {
    let title = format!("스컬리스 테스트 · {}/{}", index + 1, PAGES.len());
    let identity = player_id(player);
    if player.as_java().is_some() {
        let target = java_target(player).ok_or("Invalid Java profile name")?;
        let id = NEXT_JAVA_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
                (id < 0x8000_0000).then_some(id + 1)
            })
            .map_err(|_| "Intro dialog IDs exhausted")?;
        let font = format!("sculith:intro/{}", PAGES[index]);
        let mut rows = Vec::new();
        for glyph in '\u{e100}'..='\u{e10b}' {
            if !rows.is_empty() {
                rows.push(json!({"text": "\n"}));
            }
            rows.push(json!({"text": glyph.to_string(), "font": font, "shadow_color": 0}));
        }
        for glyphs in [
            ["\u{e120}", "\u{e121}", "\u{e122}"],
            ["\u{e123}", "\u{e124}", "\u{e125}"],
        ] {
            rows.push(json!({"text": "\n"}));
            for (button, glyph) in glyphs.into_iter().enumerate() {
                rows.push(json!({
                    "text": glyph, "font": "sculith:intro/nav", "shadow_color": 0,
                    "click_event": {"action": "custom", "id": format!("{ACTION_PREFIX}{id}/{button}")}
                }));
            }
        }
        let dialog = json!({
            "type": "minecraft:dialog_list", "title": {"text": ""}, "dialogs": [],
            "external_title": {"text": title}, "pause": false, "can_close_with_escape": true,
            "after_action": "close",
            "body": [{"type": "minecraft:plain_message", "width": 230,
                      "contents": {"text": "", "extra": rows}}]
        });
        PENDING.lock().unwrap().insert(identity, Page { id, index });
        server.execute_command(
            &format!("dialog show {target} {dialog}"),
            ServerCommandSender::Console,
        );
    } else {
        let form = BUTTONS
            .iter()
            .fold(
                SimpleFormBuilder::new(TextComponent::text(&title), TextComponent::text("")),
                |form, label| form.button(TextComponent::text(*label), None),
            )
            .build();
        let id = player.open_form(form)?;
        PENDING.lock().unwrap().insert(identity, Page { id, index });
    }
    Ok(())
}

fn select(server: &Server, player: &Player, id: u32, button: Option<&str>) {
    let identity = player_id(player);
    let page = {
        let mut pending = PENDING.lock().unwrap();
        if pending.get(&identity).is_none_or(|page| page.id != id) {
            return;
        }
        pending.remove(&identity).unwrap()
    };
    let next = match button {
        Some("0") => (page.index + PAGES.len() - 1) % PAGES.len(),
        Some("2") => (page.index + 1) % PAGES.len(),
        Some("1") => {
            if player.as_java().is_some()
                && let Some(target) = java_target(player)
            {
                server.execute_command(
                    &format!("dialog clear {target}"),
                    ServerCommandSender::Console,
                );
            }
            return;
        }
        _ => return,
    };
    if let Err(error) = show_page(server, player, next) {
        tracing::warn!(%error, "Unable to open the intro page");
    }
}

struct Intro;

impl CommandHandler for Intro {
    fn handle(
        &self,
        sender: CommandSender,
        server: Server,
        _: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let Some(player) = sender.as_player() else {
            sender.send_message(TextComponent::text("게임에서 /sculithintro를 입력하세요."));
            return Ok(0);
        };
        WAITING_FOR_LOAD.lock().unwrap().remove(&player_id(&player));
        Ok(i32::from(open_first_page(&server, &player)))
    }
}

impl EventHandler<FormResponseEvent> for Intro {
    fn handle(
        &self,
        server: Server,
        event: EventData<FormResponseEvent>,
    ) -> EventData<FormResponseEvent> {
        if event.form_id >= 0x8000_0000 {
            select(
                &server,
                &event.player,
                event.form_id,
                event.response_data.as_deref().map(str::trim),
            );
        }
        event
    }
}

impl EventHandler<DialogClickActionEvent> for Intro {
    fn handle(
        &self,
        server: Server,
        event: EventData<DialogClickActionEvent>,
    ) -> EventData<DialogClickActionEvent> {
        if let Some((id, button)) = event
            .id
            .strip_prefix(ACTION_PREFIX)
            .and_then(|id| id.split_once('/'))
            && let Ok(id) = id.parse::<u32>()
            && id < 0x8000_0000
        {
            select(&server, &event.player, id, Some(button));
        }
        event
    }
}

impl EventHandler<PlayerLeaveEvent> for Intro {
    fn handle(&self, _: Server, event: EventData<PlayerLeaveEvent>) -> EventData<PlayerLeaveEvent> {
        let id = player_id(&event.player);
        PENDING.lock().unwrap().remove(&id);
        WAITING_FOR_LOAD.lock().unwrap().remove(&id);
        event
    }
}

impl EventHandler<PlayerJoinEvent> for Intro {
    fn handle(&self, _: Server, event: EventData<PlayerJoinEvent>) -> EventData<PlayerJoinEvent> {
        if !event
            .player
            .get_bool(NAMESPACE, SEEN_INTRO)
            .unwrap_or(false)
        {
            WAITING_FOR_LOAD
                .lock()
                .unwrap()
                .insert(player_id(&event.player));
        }
        event
    }
}

impl Plugin for Intro {
    fn new() -> Self {
        Self
    }

    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: NAMESPACE.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            authors: vec![],
            description: "Sculith introduction menu for Bedrock and Java".into(),
            permissions: vec![],
            dependencies: vec![],
        }
    }

    fn on_load(&self, ctx: Context) -> Result<()> {
        let permission = "sculith-intro:open";
        ctx.register_permission(&Permission {
            node: permission.into(),
            description: "Open the Sculith introduction menu".into(),
            default: PermissionDefault::Allow,
            children: vec![],
        })?;
        let command = Command::new(
            &["sculithintro".to_owned()],
            "Open the Sculith introduction",
        )
        .execute(Intro);
        ctx.register_command(command, permission);
        ctx.register_event_handler::<FormResponseEvent, _>(Intro, EventPriority::Normal, true)?;
        ctx.register_event_handler::<DialogClickActionEvent, _>(
            Intro,
            EventPriority::Normal,
            true,
        )?;
        ctx.register_event_handler::<PlayerLeaveEvent, _>(Intro, EventPriority::Normal, true)?;
        // Join can precede the client's world-load acknowledgement by many ticks.
        ctx.register_event_handler::<PlayerJoinEvent, _>(Intro, EventPriority::Normal, true)?;
        ctx.schedule_repeating_task(1, 4, open_new_player_intros);
        Ok(())
    }
}

register_plugin!(Intro);
