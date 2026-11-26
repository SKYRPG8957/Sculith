use crate::command::argument_builder::{ArgumentBuilder, argument, command};
use crate::command::argument_types::game_profile::GameProfileArgumentType;
use crate::command::context::command_context::CommandContext;
use crate::command::errors::error_types::CommandErrorType;
use crate::command::node::dispatcher::CommandDispatcher;
use crate::command::node::{CommandExecutor, CommandExecutorResult};
use crate::command::suggestion::provider::{SuggestionProvider, SuggestionProviderResult};
use crate::command::suggestion::suggestions::SuggestionsBuilder;
use crate::data::SaveJSONConfiguration;
use pumpkin_data::translation;
use sculith_config::op::Op;
use sculith_util::PermissionLvl;
use sculith_util::permission::{Permission, PermissionDefault, PermissionRegistry};
use sculith_util::text::TextComponent;

pub const ALREADY_OP_ERROR_TYPE: CommandErrorType<0> = CommandErrorType::new(
    translation::java::COMMANDS_OP_FAILED,
    translation::bedrock::COMMANDS_OP_FAILED,
);

const DESCRIPTION: &str = "Grants operator status to a player.";
const PERMISSION: &str = "minecraft:command.op";
const ARG_TARGETS: &str = "targets";

struct OpCommandExecutor;

impl CommandExecutor for OpCommandExecutor {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let server = context.server();
        let profiles = GameProfileArgumentType::get(context, ARG_TARGETS)?;

        let new_level = server.basic_config.op_permission_level;
        let changed = {
            let mut config = server.data.operator_config.write().unwrap();
            let mut changed = Vec::new();
            for profile in profiles {
                if let Some(op) = config.ops.iter_mut().find(|o| o.uuid == profile.id) {
                    if op.level == new_level {
                        continue;
                    }
                    op.level = new_level;
                    op.name.clone_from(&profile.name);
                } else {
                    config
                        .ops
                        .push(Op::new(profile.id, profile.name.clone(), new_level, false));
                }

                let player = server.get_player_by_uuid(profile.id);
                if let Some(player) = &player {
                    player.permission_lvl.store(new_level);
                }
                changed.push((profile, player));
            }

            if changed.is_empty() {
                return Err(ALREADY_OP_ERROR_TYPE.create_without_context());
            }
            config.save();
            changed
        };

        // Packet and permission hooks can read the operator list.
        let command_dispatcher = server.command_dispatcher.load();
        for (profile, player) in &changed {
            if let Some(player) = player {
                player.publish_permission_lvl(server, &command_dispatcher);
            }
            context.source.send_feedback(
                TextComponent::translate_cross(
                    translation::java::COMMANDS_OP_SUCCESS,
                    translation::bedrock::COMMANDS_OP_SUCCESS,
                    [TextComponent::text(profile.name.clone())],
                ),
                true,
            );
        }
        Ok(changed.len() as i32)
    }
}

struct OpSuggestionProvider;

impl SuggestionProvider for OpSuggestionProvider {
    fn suggest(
        &self,
        context: &CommandContext,
        mut builder: SuggestionsBuilder,
    ) -> SuggestionProviderResult {
        // Suggest every non-opped player.
        let ops = context.server().data.operator_config.read().unwrap();
        for player in context.source.server().get_all_players() {
            if ops.ops.iter().all(|op| op.uuid != player.gameprofile.id) {
                builder = builder.suggest(player.gameprofile.name.clone());
            }
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
        command("op", DESCRIPTION).requires(PERMISSION).then(
            argument(ARG_TARGETS, GameProfileArgumentType)
                .suggests(OpSuggestionProvider)
                .executes(OpCommandExecutor),
        ),
    );
}
