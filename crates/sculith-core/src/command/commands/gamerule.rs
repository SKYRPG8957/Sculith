use std::sync::LazyLock;

use pumpkin_data::game_rules::{GameRule, GameRuleRegistry, GameRuleValue};
use rustc_hash::FxHashSet;
use sculith_util::PermissionLvl;
use sculith_util::permission::{Permission, PermissionDefault, PermissionRegistry};
use sculith_util::text::TextComponent;

use crate::command::argument_builder::{ArgumentBuilder, argument, command, literal};
use crate::command::argument_types::core::bool::BoolArgumentType;
use crate::command::argument_types::core::integer::IntegerArgumentType;
use crate::command::context::command_context::CommandContext;
use crate::command::node::detached::CommandDetachedNode;
use crate::command::node::dispatcher::CommandDispatcher;
use crate::command::node::{CommandExecutor, CommandExecutorResult};
use crate::world::bedrock_rule_name;

const DESCRIPTION: &str = "Sets or queries a game rule value.";
const PERMISSION: &str = "minecraft:command.gamerule";

struct QueryExecutor(GameRule);

impl CommandExecutor for QueryExecutor {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let key = TextComponent::text(self.0.to_string());
        let game_rule = context.source.world().get_game_rule(&self.0);
        let game_rule_i32_value = match game_rule {
            GameRuleValue::Int(value) => value.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            GameRuleValue::Bool(value) => value as i32,
        };
        let value = TextComponent::text(game_rule.to_string());

        context.source.send_feedback(
            TextComponent::translate_cross(
                "commands.gamerule.query",
                "commands.gamerule.query",
                [key, value],
            ),
            false,
        );

        Ok(game_rule_i32_value)
    }
}

struct SetIntExecutor(GameRule);

impl CommandExecutor for SetIntExecutor {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let key = TextComponent::text(self.0.to_string());
        let arg_value = IntegerArgumentType::get(context, "value")? as i64;

        context
            .source
            .world()
            .set_game_rule(&self.0, GameRuleValue::Int(arg_value));

        let value_component = TextComponent::text(arg_value.to_string());
        context.source.send_feedback(
            TextComponent::translate_cross(
                "commands.gamerule.set",
                "commands.gamerule.set",
                [key, value_component],
            ),
            true,
        );

        Ok(arg_value.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
    }
}

struct SetBoolExecutor(GameRule);

impl CommandExecutor for SetBoolExecutor {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let key = TextComponent::text(self.0.to_string());
        let arg_value = BoolArgumentType::get(context, "value")?;

        context
            .source
            .world()
            .set_game_rule(&self.0, GameRuleValue::Bool(arg_value));

        let value_component = TextComponent::text(arg_value.to_string());
        context.source.send_feedback(
            TextComponent::translate_cross(
                "commands.gamerule.set",
                "commands.gamerule.set",
                [key, value_component],
            ),
            true,
        );

        Ok(arg_value as i32)
    }
}

fn rule_names(rule: &GameRule) -> impl Iterator<Item = String> {
    let java = rule.to_string();
    let bedrock = bedrock_rule_name(rule)
        .map(str::to_ascii_lowercase)
        .filter(|name| name != &java);
    [java, format!("minecraft:{rule}")]
        .into_iter()
        .chain(bedrock)
}

// These writes refresh clocks or player chunk tickets in sibling dimensions.
static COORDINATED_RULES: LazyLock<FxHashSet<String>> = LazyLock::new(|| {
    [GameRule::AdvanceTime, GameRule::SpectatorsGenerateChunks]
        .iter()
        .flat_map(rule_names)
        .collect()
});

pub(super) fn world_owned_rule(text: &str) -> bool {
    let mut arguments = text.split_ascii_whitespace().skip(1);
    let Some(rule) = arguments.next() else {
        return false;
    };
    if arguments.next().is_none() {
        return true;
    }
    arguments.next().is_none() && !COORDINATED_RULES.contains(rule)
}

pub(super) static GAMERULE_COMMAND: LazyLock<CommandDetachedNode> = LazyLock::new(|| {
    let mut cmd = command("gamerule", DESCRIPTION).requires(PERMISSION);
    let rule_registry = GameRuleRegistry::default();

    for rule in GameRule::all() {
        for name in rule_names(rule) {
            let rule_literal = literal(name).executes(QueryExecutor(rule.clone()));
            let branch = match rule_registry.get(rule) {
                GameRuleValue::Int(_) => {
                    let arg = rule
                        .int_bounds()
                        .map_or_else(IntegerArgumentType::any, |(min, max)| {
                            IntegerArgumentType::new(min, max)
                        });
                    rule_literal.then(argument("value", arg).executes(SetIntExecutor(rule.clone())))
                }
                GameRuleValue::Bool(_) => rule_literal.then(
                    argument("value", BoolArgumentType).executes(SetBoolExecutor(rule.clone())),
                ),
            };
            cmd = cmd.then(branch);
        }
    }

    cmd.build()
});

pub fn register(dispatcher: &mut CommandDispatcher, registry: &PermissionRegistry) {
    registry.register_permission_or_panic(Permission::new(
        PERMISSION,
        DESCRIPTION,
        PermissionDefault::Op(PermissionLvl::Two),
    ));
    dispatcher.register(GAMERULE_COMMAND.clone());
}
