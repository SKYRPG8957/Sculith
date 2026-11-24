use crate::command::argument_builder::{ArgumentBuilder, command};
use crate::command::context::command_context::CommandContext;
use crate::command::node::detached::CommandDetachedNode;
use crate::command::node::dispatcher::CommandDispatcher;
use crate::command::node::{CommandExecutor, CommandExecutorResult};
use pumpkin_data::translation;
use sculith_util::PermissionLvl;
use sculith_util::permission::{Permission, PermissionDefault, PermissionRegistry};
use sculith_util::text::click::ClickEvent;
use sculith_util::text::hover::HoverEvent;
use sculith_util::text::{TextComponent, color::NamedColor};
use std::borrow::Cow;
use std::sync::LazyLock;

const DESCRIPTION: &str = "Displays the world seed.";
const PERMISSION: &str = "minecraft:command.seed";

struct SeedCommandExecutor;

pub(super) static SEED_COMMAND: LazyLock<CommandDetachedNode> = LazyLock::new(|| {
    command("seed", DESCRIPTION)
        .requires(PERMISSION)
        .executes(SeedCommandExecutor)
        .build()
});

fn create_copy_on_click_text(content: String) -> TextComponent {
    // A Bedrock text packet flattens translation parameters to strings.
    TextComponent::translate_cross(
        translation::java::COMMANDS_SEED_SUCCESS,
        translation::bedrock::COMMANDS_SEED_SUCCESS,
        [TextComponent::text("[")
            .add_child(
                TextComponent::text(content.clone())
                    .hover_event(HoverEvent::show_text(TextComponent::translate_cross(
                        translation::java::CHAT_COPY_CLICK,
                        translation::java::CHAT_COPY_CLICK,
                        [],
                    )))
                    .click_event(ClickEvent::CopyToClipboard {
                        value: Cow::from(content),
                    })
                    .color_named(NamedColor::Green),
            )
            .add_text("]")],
    )
}

impl CommandExecutor for SeedCommandExecutor {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let seed = context.world().level.seed.0;
        let seed_string = seed.to_string();

        context
            .source
            .send_feedback(create_copy_on_click_text(seed_string), false);

        Ok(seed as i32)
    }
}

pub fn register(dispatcher: &mut CommandDispatcher, registry: &PermissionRegistry) {
    registry.register_permission_or_panic(Permission::new(
        PERMISSION,
        DESCRIPTION,
        // For integrated servers, the permission level is 0,
        // but Pumpkin is always a dedicated server. For dedicated servers,
        // /seed is limited to level 2.
        PermissionDefault::Op(PermissionLvl::Two),
    ));

    dispatcher.register(SEED_COMMAND.clone());
}
