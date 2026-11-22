use std::sync::{Arc, LazyLock};

use crate::command::CommandSource;
use crate::command::node::attached::NodeClassification;
use crate::command::node::detached::DetachedNode;
use crate::command::node::dispatcher::{CommandDispatcher, ResultConsumer, ResultDeferrer};
use crate::command::node::tree::ROOT_NODE_ID;
use crate::command::node::{OwnedNodeData, RedirectModifier};
use crate::command::string_reader::StringReader;

use super::{seed::SEED_COMMAND, time::TIME_COMMAND};

pub(super) static DEFAULT_CONSUMER: LazyLock<Arc<dyn ResultConsumer<CommandSource>>> =
    LazyLock::new(|| Arc::new(ResultDeferrer));

/// Checks the live literal route without evaluating plugin code during admission.
pub fn is_world_read(dispatcher: &CommandDispatcher, input: &str) -> bool {
    let text = input.strip_prefix('/').unwrap_or(input);
    let mut words = text.split(' ');
    let Some(name) = words.next() else {
        return false;
    };
    let original = match (name, words.next(), words.next(), words.next()) {
        ("seed", None, None, None) => &*SEED_COMMAND,
        ("time", Some("query"), Some("time" | "gametime" | "daytime" | "day"), None) => {
            &*TIME_COMMAND
        }
        _ => return false,
    };
    if dispatcher.is_disabled(name)
        || (text != input && dispatcher.has_command(input.split(' ').next().unwrap_or(input)))
        || !Arc::ptr_eq(&dispatcher.consumer, &DEFAULT_CONSUMER)
    {
        return false;
    }

    let mut parent = ROOT_NODE_ID;
    let mut expected = &original.owned;
    let mut children = &original.children;
    for (depth, word) in text.split(' ').enumerate() {
        let classification = if depth == 0 {
            NodeClassification::Command
        } else {
            let Some(DetachedNode::Literal(literal)) = children.get(word) else {
                return false;
            };
            expected = &literal.owned;
            children = &literal.children;
            NodeClassification::Literal
        };

        // Follow the parser's precedence, including a later registration shadowing a literal.
        let nodes = dispatcher
            .tree
            .get_relevant_nodes(&mut StringReader::new(word), parent);
        let [id] = nodes.as_slice() else {
            return false;
        };
        let node = &dispatcher.tree[*id];
        let live = node.owned_node_data_ref();
        if node.classification() != classification
            || node.redirect().is_some()
            || !matches_node(live, expected)
        {
            return false;
        }
        parent = *id;
    }
    expected.command.is_some()
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
