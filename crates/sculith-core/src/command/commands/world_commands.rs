use std::sync::{Arc, LazyLock};

use rustc_hash::FxHashMap;

use crate::command::CommandSource;
use crate::command::node::attached::{AttachedNode, LiteralAttachedNode, NodeId};
use crate::command::node::detached::{DetachedNode, LiteralDetachedNode};
use crate::command::node::dispatcher::{CommandDispatcher, ResultConsumer, ResultDeferrer};
use crate::command::node::tree::ROOT_NODE_ID;
use crate::command::node::{OwnedNodeData, RedirectModifier, Redirection};
use crate::command::string_reader::StringReader;

use super::{
    difficulty::DIFFICULTY_COMMAND,
    execute::EXECUTE_COMMAND,
    forceload::FORCELOAD_COMMAND,
    gamerule::{GAMERULE_COMMAND, world_owned_rule},
    seed::SEED_COMMAND,
    setblock::SETBLOCK_COMMAND,
    teleport::TELEPORT_COMMAND,
    tickingarea::TICKING_AREA_COMMAND,
    time::TIME_COMMAND,
    weather::WEATHER_COMMAND,
};

pub(super) static DEFAULT_CONSUMER: LazyLock<Arc<dyn ResultConsumer<CommandSource>>> =
    LazyLock::new(|| Arc::new(ResultDeferrer));

/// Checks the live built-in subtree without evaluating plugin code during admission.
pub fn is_world_command(dispatcher: &CommandDispatcher, input: &str) -> bool {
    let text = input.strip_prefix('/').unwrap_or(input);
    let name = text.split(' ').next().unwrap_or_default();
    if dispatcher.is_disabled(name)
        || (text != input
            && dispatcher.has_command(input.split_whitespace().next().unwrap_or_default()))
        || !Arc::ptr_eq(&dispatcher.consumer, &DEFAULT_CONSUMER)
    {
        return false;
    }
    if name == "execute" {
        return self_transfer_matches(dispatcher, text);
    }

    // These built-ins only access the sender's world for simulation state.
    let original = match name {
        "seed" if text == "seed" => &*SEED_COMMAND,
        "time" => &*TIME_COMMAND,
        "weather" => &*WEATHER_COMMAND,
        "difficulty" if text == "difficulty" => &*DIFFICULTY_COMMAND,
        "forceload" => &*FORCELOAD_COMMAND,
        "tickingarea" => &*TICKING_AREA_COMMAND,
        "gamerule" if world_owned_rule(text) => &*GAMERULE_COMMAND,
        "setblock" if strict_block_write(text) => &*SETBLOCK_COMMAND,
        _ => return false,
    };
    command_root(dispatcher, text).is_some_and(|(_, live)| {
        matches_subtree(dispatcher, live, &original.owned, &original.children)
    })
}

fn command_root<'a>(
    dispatcher: &'a CommandDispatcher,
    text: &str,
) -> Option<(NodeId, &'a AttachedNode)> {
    // Use the same command root selection as the parser.
    let roots = dispatcher
        .tree
        .get_relevant_nodes(&mut StringReader::new(text), ROOT_NODE_ID);
    let [root] = roots.as_slice() else {
        return None;
    };
    let live = &dispatcher.tree[*root];
    matches!(live, AttachedNode::Command(_)).then_some((*root, live))
}

fn self_transfer_matches(dispatcher: &CommandDispatcher, text: &str) -> bool {
    // Only the exact built-in dimension redirect and the sender's own
    // coordinate teleport can hand the destination change to its world owner.
    let mut arguments = text.split_ascii_whitespace().skip(1);
    if arguments.next() != Some("in") || arguments.nth(1) != Some("run") {
        return false;
    }
    let Some(verb @ ("tp" | "teleport")) = arguments.next() else {
        return false;
    };
    if arguments.next() != Some("@s") {
        return false;
    }
    let [Some(x), Some(y), Some(z), None] = std::array::from_fn(|_| arguments.next()) else {
        return false;
    };
    [x, y, z]
        .into_iter()
        .all(|axis| axis.parse::<f64>().is_ok_and(f64::is_finite))
        && execute_dimension_path(dispatcher).is_some()
        && verified_teleport(dispatcher, verb).is_some()
}

fn execute_literal<'a>(
    dispatcher: &'a CommandDispatcher,
    command: &'a AttachedNode,
    name: &str,
) -> Option<(&'a LiteralAttachedNode, &'static LiteralDetachedNode)> {
    let AttachedNode::Literal(live) = &dispatcher.tree[*command.children_ref().get(name)?] else {
        return None;
    };
    let DetachedNode::Literal(expected) = EXECUTE_COMMAND.children.get(name)? else {
        return None;
    };
    (live.meta.literal == expected.meta.literal && matches_node(&live.owned, &expected.owned))
        .then_some((live, expected))
}

fn execute_dimension_path(dispatcher: &CommandDispatcher) -> Option<()> {
    let (execute, root) = command_root(dispatcher, "execute")?;
    if root.redirect().is_some()
        || !matches_node(root.owned_node_data_ref(), &EXECUTE_COMMAND.owned)
    {
        return None;
    }
    let (run, _) = execute_literal(dispatcher, root, "run")?;
    let (dimension, expected) = execute_literal(dispatcher, root, "in")?;
    let AttachedNode::Argument(live) = &dispatcher.tree[*dimension.children.get("dimension")?]
    else {
        return None;
    };
    let DetachedNode::Argument(expected) = expected.children.get("dimension")? else {
        return None;
    };
    (run.redirect == Some(Redirection::Root)
        && run.children.is_empty()
        && dimension.redirect.is_none()
        && dimension.children.len() == 1
        && live.redirect == Some(Redirection::Local(execute))
        && live.children.is_empty()
        && live.meta.name == expected.meta.name
        && Arc::ptr_eq(&live.meta.argument_type, &expected.meta.argument_type)
        && matches_node(&live.owned, &expected.owned))
    .then_some(())
}

fn verified_teleport(dispatcher: &CommandDispatcher, verb: &str) -> Option<()> {
    if dispatcher.is_disabled(verb) || dispatcher.is_disabled("teleport") {
        return None;
    }
    let (id, root) = command_root(dispatcher, "teleport")?;
    if !matches_subtree(
        dispatcher,
        root,
        &TELEPORT_COMMAND.owned,
        &TELEPORT_COMMAND.children,
    ) {
        return None;
    }
    if verb == "teleport" {
        return Some(());
    }
    let (_, alias) = command_root(dispatcher, "tp")?;
    let owned = alias.owned_node_data_ref();
    (alias.redirect() == Some(Redirection::Local(id))
        && alias.children_ref().is_empty()
        && owned.command.is_none()
        && !owned.forks
        && matches!(owned.modifier, RedirectModifier::KeepSource)
        && matches_requirements(owned, &TELEPORT_COMMAND.owned))
    .then_some(())
}

fn strict_block_write(text: &str) -> bool {
    // This mode skips block callbacks and neighbor updates. Block-entity NBT
    // is handled by the coordinated command path.
    let mut arguments = text.split_ascii_whitespace();
    arguments.nth(4).is_some_and(|block| !block.contains('{'))
        && arguments.next() == Some("strict")
        && arguments.next().is_none()
}

fn matches_subtree(
    dispatcher: &CommandDispatcher,
    live: &AttachedNode,
    expected: &OwnedNodeData<CommandSource>,
    children: &FxHashMap<String, DetachedNode>,
) -> bool {
    let live_children = live.children_ref();
    if live.redirect().is_some()
        || !matches_node(live.owned_node_data_ref(), expected)
        || live_children.len() != children.len()
    {
        return false;
    }

    children.iter().all(|(name, expected)| {
        let Some(id) = live_children.get(name) else {
            return false;
        };
        let live = &dispatcher.tree[*id];
        match (live, expected) {
            (AttachedNode::Literal(live_literal), DetachedNode::Literal(expected)) => {
                live_literal.meta.literal == expected.meta.literal
                    && matches_subtree(dispatcher, live, &expected.owned, &expected.children)
            }
            (AttachedNode::Argument(live_argument), DetachedNode::Argument(expected)) => {
                live_argument.meta.name == expected.meta.name
                    && Arc::ptr_eq(
                        &live_argument.meta.argument_type,
                        &expected.meta.argument_type,
                    )
                    && matches_subtree(dispatcher, live, &expected.owned, &expected.children)
            }
            _ => false,
        }
    })
}

fn matches_node(
    live: &OwnedNodeData<CommandSource>,
    expected: &OwnedNodeData<CommandSource>,
) -> bool {
    live.global_id == expected.global_id
        && live.forks == expected.forks
        && match (&live.modifier, &expected.modifier) {
            (RedirectModifier::KeepSource, RedirectModifier::KeepSource) => true,
            (RedirectModifier::Custom(live), RedirectModifier::Custom(expected)) => {
                Arc::ptr_eq(live, expected)
            }
            _ => false,
        }
        && match (&live.command, &expected.command) {
            (Some(live), Some(expected)) => Arc::ptr_eq(live, expected),
            (None, None) => true,
            _ => false,
        }
        && matches_requirements(live, expected)
}

fn matches_requirements(
    live: &OwnedNodeData<CommandSource>,
    expected: &OwnedNodeData<CommandSource>,
) -> bool {
    live.requirements.0.len() == expected.requirements.0.len()
        && live
            .requirements
            .0
            .iter()
            .zip(&expected.requirements.0)
            .all(|(live, expected)| Arc::ptr_eq(&live.0, &expected.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sculith_util::permission::PermissionRegistry;

    #[test]
    fn only_the_plain_strict_block_route_runs_with_the_owner() {
        let mut dispatcher = CommandDispatcher::new();
        dispatcher.consumer = DEFAULT_CONSUMER.clone();
        super::super::setblock::register(&mut dispatcher, &PermissionRegistry::default());
        assert!(is_world_command(
            &dispatcher,
            "setblock ~ ~-1 ~ minecraft:hopper[facing=east,enabled=true] strict",
        ));
        for command in [
            "setblock 0 0 0 stone",
            "setblock 0 0 0 stone replace",
            "setblock 0 0 0 chest{Items:[]} strict",
            "setblock 0 0 0 stone strict extra",
            "execute in minecraft:the_nether run setblock 0 0 0 stone strict",
        ] {
            assert!(!is_world_command(&dispatcher, command), "{command}");
        }
    }

    #[test]
    fn a_dimension_self_transfer_uses_only_its_registered_route() {
        let mut dispatcher = CommandDispatcher::new();
        dispatcher.consumer = DEFAULT_CONSUMER.clone();
        let registry = PermissionRegistry::default();
        super::super::execute::register(&mut dispatcher, &registry);
        super::super::teleport::register(&mut dispatcher, &registry);
        for verb in ["tp", "teleport"] {
            let command = format!("execute in minecraft:the_nether run {verb} @s 3.5 72 2.5");
            assert!(is_world_command(&dispatcher, &command));
        }
        for command in [
            "execute in minecraft:the_nether run tp @a 3.5 72 2.5",
            "execute as @s in minecraft:the_nether run tp @s 3.5 72 2.5",
            "execute in minecraft:the_nether run tp @s @p",
            "execute in minecraft:the_nether run tp @s @e[name=\"one two three\"]",
            "execute in minecraft:the_nether run tp @s ~ ~ ~",
            "execute in minecraft:the_nether run tp @s 3.5 72 2.5 10 0",
        ] {
            assert!(!is_world_command(&dispatcher, command), "{command}");
        }
    }

    #[test]
    fn only_metadata_rules_and_same_world_weather_run_with_the_owner() {
        let mut dispatcher = CommandDispatcher::new();
        dispatcher.consumer = DEFAULT_CONSUMER.clone();
        let registry = PermissionRegistry::default();
        super::super::weather::register(&mut dispatcher, &registry);
        super::super::gamerule::register(&mut dispatcher, &registry);
        super::super::difficulty::register(&mut dispatcher, &registry);
        for command in [
            "weather thunder 600t",
            "difficulty",
            "gamerule keepinventory true",
            "gamerule minecraft:fall_damage false",
            "gamerule dodaylightcycle",
            "gamerule spectators_generate_chunks",
        ] {
            assert!(is_world_command(&dispatcher, command), "{command}");
        }
        for command in [
            "difficulty hard",
            "gamerule dodaylightcycle false",
            "gamerule minecraft:advance_time true",
            "gamerule spectators_generate_chunks false",
            "gamerule minecraft:spectators_generate_chunks true",
        ] {
            assert!(!is_world_command(&dispatcher, command), "{command}");
        }
    }

    #[test]
    fn force_loaded_chunks_use_the_owner_for_the_builtin_tree() {
        use crate::command::argument_builder::{ArgumentBuilder, argument, command, literal};
        use crate::command::argument_types::core::string::StringArgumentType;

        let mut dispatcher = CommandDispatcher::new();
        dispatcher.consumer = DEFAULT_CONSUMER.clone();
        super::super::forceload::register(&mut dispatcher, &PermissionRegistry::default());
        for command in [
            "forceload add -48 16",
            "/forceload remove -48 16",
            "forceload remove all",
            "forceload query -48 16",
        ] {
            assert!(is_world_command(&dispatcher, command), "{command}");
        }
        assert!(!is_world_command(
            &dispatcher,
            "execute in minecraft:the_nether run forceload remove all"
        ));
        dispatcher.register(
            command("forceload", "extension")
                .then(literal("query").then(argument("scope", StringArgumentType::SingleWord))),
        );
        assert!(!is_world_command(&dispatcher, "forceload query"));
    }
}
