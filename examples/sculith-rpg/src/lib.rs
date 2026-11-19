use pumpkin_plugin_api::{
    Context, Plugin, PluginMetadata, Result, Server,
    command::{CommandError, CommandNode, CommandSender, ConsumedArgs},
    commands::{Command, CommandHandler},
    persistent_data::PersistentDataHolder,
    register_plugin,
    wit::pumpkin::plugin::{
        permission::{Permission, PermissionDefault},
        text::TextComponent,
    },
};

const NAMESPACE: &str = "sculith-rpg";
const PERMISSION: &str = "sculith-rpg:play";

fn total_xp(level: i32, progress: f32) -> i32 {
    let level = i64::from(level.max(0)).min(30_000);
    let (base, span) = match level {
        0..=15 => (level * (level + 6), 2 * level + 7),
        16..=30 => ((5 * level * level - 81 * level + 720) / 2, 5 * level - 38),
        _ => (
            (9 * level * level - 325 * level + 4440) / 2,
            9 * level - 158,
        ),
    };
    (base + (progress.clamp(0.0, 1.0) * span as f32).round() as i64).min(i64::from(i32::MAX)) as i32
}

fn rank(level: i32) -> &'static str {
    match level {
        1..=4 => "새내기",
        5..=9 => "모험가",
        10..=19 => "정예",
        20..=39 => "영웅",
        _ => "전설",
    }
}

struct EarnExperience;
impl pumpkin_plugin_api::events::EventHandler<pumpkin_plugin_api::events::PlayerExpChangeEvent>
    for EarnExperience
{
    fn handle(
        &self,
        _: Server,
        event: pumpkin_plugin_api::events::EventData<
            pumpkin_plugin_api::events::PlayerExpChangeEvent,
        >,
    ) -> pumpkin_plugin_api::events::EventData<pumpkin_plugin_api::events::PlayerExpChangeEvent>
    {
        if event.amount > 0 {
            let current = total_xp(
                event.player.get_experience_level(),
                event.player.get_experience_progress(),
            );
            let earned = event
                .player
                .get_int(NAMESPACE, "peak-xp")
                .unwrap_or_default()
                .max(current)
                .saturating_add(event.amount);
            event.player.set_int(NAMESPACE, "peak-xp", earned);
        }
        event
    }
}

struct Action {
    heal: bool,
}

impl CommandHandler for Action {
    fn handle(
        &self,
        sender: CommandSender,
        _: Server,
        _: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let Some(player) = sender.as_player() else {
            sender.send_message(TextComponent::text("게임에서 /rpg 를 입력하세요."));
            return Ok(0);
        };
        let current = total_xp(
            player.get_experience_level(),
            player.get_experience_progress(),
        );
        let best = player.get_int(NAMESPACE, "peak-xp").unwrap_or_default();
        let xp = best.max(current);
        if xp > best {
            player.set_int(NAMESPACE, "peak-xp", xp);
        }
        let level = 1 + xp / 100;
        let used = player
            .get_int(NAMESPACE, "heals-used")
            .unwrap_or_default()
            .max(0);
        let left = (level - 1 - used).max(0);
        let message = if !self.heal {
            format!(
                "RPG Lv.{level} · {} | XP {xp} | 다음 레벨까지 {} XP | 회복 {left}회 (/rpg heal)",
                rank(level),
                100 - xp % 100
            )
        } else if left == 0 {
            "회복 횟수가 없습니다. RPG 레벨을 올리면 한 번 충전됩니다.".into()
        } else {
            let hp = player.get_health();
            let max = player.get_max_health();
            if hp <= 0.0 || hp >= max {
                "살아 있고 체력이 줄었을 때 회복할 수 있습니다.".into()
            } else {
                let after = (hp + 4.0).min(max);
                player.set_health(after);
                player.set_int(NAMESPACE, "heals-used", used + 1);
                format!("회복 +{} HP | 남은 회복 {}회", after - hp, left - 1)
            }
        };
        sender.send_message(TextComponent::text(&message));
        Ok(1)
    }
}

struct Rpg;
impl Plugin for Rpg {
    fn new() -> Self {
        Self
    }
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: NAMESPACE.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            authors: vec!["Sculith".into()],
            description: "경험치 기반 레벨, 칭호와 회복".into(),
            dependencies: vec![],
            permissions: vec![],
        }
    }
    fn on_load(&self, ctx: Context) -> Result<()> {
        ctx.register_event_handler::<pumpkin_plugin_api::events::PlayerExpChangeEvent, _>(
            EarnExperience,
            pumpkin_plugin_api::events::EventPriority::Lowest,
            false,
        )?;
        ctx.register_permission(&Permission {
            node: PERMISSION.into(),
            description: "RPG 레벨과 회복".into(),
            default: PermissionDefault::Allow,
            children: vec![],
        })?;
        let command = Command::new(&["rpg".to_string()], "RPG 상태와 회복")
            .then(CommandNode::literal("heal").execute(Action { heal: true }))
            .execute(Action { heal: false });
        ctx.register_command(command, PERMISSION);
        Ok(())
    }
}
register_plugin!(Rpg);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn experience_and_ranks() {
        assert_eq!(total_xp(7, 14.0 / 21.0), 105);
        assert_eq!(total_xp(16, 0.0), 352);
        assert_eq!(total_xp(31, 0.0), 1507);
        assert_eq!(rank(40), "전설");
    }
}
