use crate::command::{
    argument_builder::{ArgumentBuilder, command, literal},
    context::command_context::CommandContext,
    node::{CommandExecutor, CommandExecutorResult, dispatcher::CommandDispatcher},
};
use pumpkin_data::packet::CURRENT_MC_VERSION;
use pumpkin_world::{CURRENT_BEDROCK_MC_PROTOCOL, CURRENT_BEDROCK_MC_VERSION};
use sculith_util::{
    PermissionLvl,
    permission::{Permission, PermissionDefault, PermissionRegistry},
    text::TextComponent,
};

// Permission files from older servers may explicitly grant or deny this command.
pub(super) const PERMISSION: &str = "pumpkin:command.pumpkin";
const DESCRIPTION: &str = "Display information about Sculith.";

struct Version;
struct Contributors;
struct Performance;

impl CommandExecutor for Performance {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let server = context.source.server();
        let mut window = server.get_tick_times_nanos_copy();
        let count = (server.tick_count.load(std::sync::atomic::Ordering::Relaxed) as usize)
            .min(window.len());
        if count == 0 {
            return Ok(0);
        }
        let times = &mut window[..count];
        times.sort_unstable();
        let ms = |percent: usize| times[(count * percent).div_ceil(100) - 1] as f64 / 1_000_000.0;
        context.source.send_message(TextComponent::text(format!(
            "SCULITH_TICK sample={count} p50={:.2} p95={:.2} p99={:.2} max={:.2} ms",
            ms(50),
            ms(95),
            ms(99),
            ms(100)
        )));
        Ok(1)
    }
}

impl CommandExecutor for Version {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        context.source.send_message(TextComponent::text(format!(
            "Sculith {} ({})\nBedrock {} (protocol {})\nJava {} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            env!("GIT_HASH"),
            CURRENT_BEDROCK_MC_VERSION,
            CURRENT_BEDROCK_MC_PROTOCOL,
            CURRENT_MC_VERSION,
            CURRENT_MC_VERSION.protocol_version(),
        )));
        Ok(1)
    }
}

impl CommandExecutor for Contributors {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        context.source.send_message(TextComponent::text(
            "Sculith contributors · https://github.com/SKYRPG8957/Sculith/graphs/contributors",
        ));
        Ok(1)
    }
}

pub fn register(dispatcher: &mut CommandDispatcher, registry: &PermissionRegistry) {
    registry.register_permission_or_panic(Permission::new(
        PERMISSION,
        DESCRIPTION,
        PermissionDefault::Op(PermissionLvl::Zero),
    ));
    dispatcher.register_with_aliases(
        command("sculith", DESCRIPTION)
            .requires(PERMISSION)
            .then(literal("performance").executes(Performance))
            .then(literal("contributors").executes(Contributors))
            .executes(Version),
        &["version"],
    );
}
