use sculith_util::PermissionLvl;
use sculith_util::permission::{Permission, PermissionDefault, PermissionRegistry};
use sculith_util::text::TextComponent;

use crate::command::argument_builder::{ArgumentBuilder, argument, command};
use crate::command::argument_types::game_profile::GameProfileArgumentType;
use crate::command::context::command_context::CommandContext;
use crate::command::errors::error_types::CommandErrorType;
use crate::command::node::dispatcher::CommandDispatcher;
use crate::command::node::{CommandExecutor, CommandExecutorResult};
use crate::command::suggestion::provider::{SuggestionProvider, SuggestionProviderResult};
use crate::command::suggestion::suggestions::SuggestionsBuilder;
use crate::data::SaveJSONConfiguration;

const DESCRIPTION: &str = "Revokes operator status from a player.";
const PERMISSION: &str = "minecraft:command.deop";

const ERROR_DEOP_FAILED: CommandErrorType<0> = CommandErrorType::new(
    pumpkin_data::translation::java::COMMANDS_DEOP_FAILED,
    pumpkin_data::translation::bedrock::COMMANDS_DEOP_FAILED,
);

struct DeopExecutor;

impl CommandExecutor for DeopExecutor {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let targets = GameProfileArgumentType::get(context, "targets")?;
        let server = context.source.server();
        let changed = {
            let mut config = server.data.operator_config.write().unwrap();
            let mut changed = Vec::new();
            for profile in targets {
                let Some(op_index) = config.ops.iter().position(|o| o.uuid == profile.id) else {
                    continue;
                };
                config.ops.remove(op_index);
                let player = server.get_player_by_uuid(profile.id);
                if let Some(player) = &player {
                    player.permission_lvl.store(PermissionLvl::Zero);
                }
                changed.push((profile, player));
            }
            if changed.is_empty() {
                return Err(ERROR_DEOP_FAILED.create_without_context());
            }
            config.save();
            changed
        };

        let command_dispatcher = server.command_dispatcher.load();
        for (profile, player) in &changed {
            if let Some(player) = player {
                player.publish_permission_lvl(server, &command_dispatcher);
            }
            let msg = TextComponent::translate_cross(
                pumpkin_data::translation::java::COMMANDS_DEOP_SUCCESS,
                pumpkin_data::translation::bedrock::COMMANDS_DEOP_SUCCESS,
                [TextComponent::text(profile.name.clone())],
            );
            context.source.send_feedback(msg, true);
        }
        crate::command::commands::whitelist::kick_non_whitelisted_players(server);
        Ok(changed.len() as i32)
    }
}

struct DeopSuggestionProvider;

impl SuggestionProvider for DeopSuggestionProvider {
    fn suggest(
        &self,
        context: &CommandContext,
        mut builder: SuggestionsBuilder,
    ) -> SuggestionProviderResult {
        let ops = context.server().data.operator_config.read().unwrap();
        for op in &ops.ops {
            builder = builder.suggest(op.name.clone());
        }
        builder.build()
    }
}

pub fn register(dispatcher: &mut CommandDispatcher, registry: &PermissionRegistry) {
    registry.register_permission_or_panic(Permission::new(
        PERMISSION,
        DESCRIPTION,
        PermissionDefault::Op(PermissionLvl::Three),
    ));

    dispatcher.register(
        command("deop", DESCRIPTION).requires(PERMISSION).then(
            argument("targets", GameProfileArgumentType)
                .suggests(DeopSuggestionProvider)
                .executes(DeopExecutor),
        ),
    );
}
