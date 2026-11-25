use std::sync::{Arc, LazyLock};

use rustc_hash::FxHashMap;

use crate::command::CommandSource;
use crate::command::node::attached::AttachedNode;
use crate::command::node::detached::DetachedNode;
use crate::command::node::dispatcher::{CommandDispatcher, ResultConsumer, ResultDeferrer};
use crate::command::node::tree::ROOT_NODE_ID;
use crate::command::node::{OwnedNodeData, RedirectModifier};
use crate::command::string_reader::StringReader;

use super::{seed::SEED_COMMAND, setblock::SETBLOCK_COMMAND, time::TIME_COMMAND};

pub(super) static DEFAULT_CONSUMER: LazyLock<Arc<dyn ResultConsumer<CommandSource>>> =
    LazyLock::new(|| Arc::new(ResultDeferrer));

/// Checks the live built-in subtree without evaluating plugin code during admission.
pub fn is_world_command(dispatcher: &CommandDispatcher, input: &str) -> bool {
    let text = input.strip_prefix('/').unwrap_or(input);
    let name = text.split(' ').next().unwrap_or_default();
    // These built-ins only access the sender's world for simulation state.
    let original = match name {
        "seed" if text == "seed" => &*SEED_COMMAND,
        "time" => &*TIME_COMMAND,
        "setblock" if strict_block_write(text) => &*SETBLOCK_COMMAND,
        _ => return false,
    };
    if dispatcher.is_disabled(name)
        || (text != input
            && dispatcher.has_command(input.split_whitespace().next().unwrap_or_default()))
        || !Arc::ptr_eq(&dispatcher.consumer, &DEFAULT_CONSUMER)
    {
        return false;
    }

    // Use the same root selection as the parser. All descendants of the command
    // must match because argument parsers can consume more than one word.
    let roots = dispatcher
        .tree
        .get_relevant_nodes(&mut StringReader::new(text), ROOT_NODE_ID);
    let [root] = roots.as_slice() else {
        return false;
    };
    let live = &dispatcher.tree[*root];
    matches!(live, AttachedNode::Command(_))
        && matches_subtree(dispatcher, live, &original.owned, &original.children)
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
        && !live.forks
        && matches!(live.modifier, RedirectModifier::KeepSource)
        && match (&live.command, &expected.command) {
            (Some(live), Some(expected)) => Arc::ptr_eq(live, expected),
            (None, None) => true,
            _ => false,
        }
        && live.requirements.0.len() == expected.requirements.0.len()
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
}
