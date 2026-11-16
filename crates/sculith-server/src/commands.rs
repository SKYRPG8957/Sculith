//! Bounded owner-only command registry. Arguments never enter public streams.
use anyhow::{Result, ensure};
use sculith_core::CommandSpec;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub struct Submission {
    pub player_id: u64,
    pub name: String,
    pub plugin: String,
    pub command: String,
    pub arguments: String,
}
pub enum Route {
    Plugin {
        owner: String,
        command: String,
        arguments: String,
    },
    Feedback(String),
}
#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<String, (String, CommandSpec, bool)>,
    sealed: bool,
}
impl Registry {
    pub fn register(&mut self, plugin: &str, specs: Vec<CommandSpec>) -> Result<()> {
        ensure!(!self.sealed, "command registration has closed");
        ensure!(
            !plugin.is_empty() && plugin.len() <= 64,
            "invalid command owner"
        );
        ensure!(
            !specs.is_empty() && specs.len() <= 32 && self.entries.len() + specs.len() <= 128,
            "command registry capacity exceeded"
        );
        ensure!(
            self.entries
                .values()
                .filter(|(owner, _, _)| owner == plugin)
                .count()
                + specs.len()
                <= 32,
            "plugin command capacity exceeded"
        );
        let mut names = BTreeSet::new();
        for spec in &specs {
            spec.validate().map_err(anyhow::Error::msg)?;
            ensure!(
                spec.name != "help"
                    && !self.entries.contains_key(&spec.name)
                    && names.insert(&spec.name),
                "duplicate or reserved command name"
            );
        }
        for spec in specs {
            self.entries
                .insert(spec.name.clone(), (plugin.into(), spec, true));
        }
        Ok(())
    }
    pub fn seal(&mut self) {
        self.sealed = true;
    }
    pub fn disable(&mut self, plugin: &str) {
        for (owner, _, enabled) in self.entries.values_mut() {
            if owner == plugin {
                *enabled = false;
            }
        }
    }
    pub fn list(&self) -> Vec<CommandSpec> {
        let mut entries = vec![CommandSpec {
            name: "help".into(),
            description: "List available commands".into(),
        }];
        entries.extend(
            self.entries
                .values()
                .filter(|(_, _, enabled)| *enabled)
                .map(|(_, spec, _)| spec.clone()),
        );
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
    pub fn route(&self, input: &str) -> Route {
        let input = input.strip_prefix('/').unwrap_or(input);
        let (command, arguments) = input
            .split_once(char::is_whitespace)
            .map_or((input, ""), |(command, args)| (command, args.trim()));
        if command == "help" {
            let entries = self.list();
            let pages = entries.len().div_ceil(8);
            let page = if arguments.is_empty() {
                Some(1)
            } else {
                arguments.parse::<usize>().ok()
            };
            return Route::Feedback(match page.filter(|page| (1..=pages).contains(page)) {
                Some(page) => format!(
                    "Commands {page}/{pages}: {}",
                    entries
                        .iter()
                        .skip((page - 1) * 8)
                        .take(8)
                        .map(|s| format!("/{}", s.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                None => format!("Usage: /help [1..{pages}]"),
            });
        }
        match self.entries.get(command) {
            Some((owner, _, true)) => Route::Plugin {
                owner: owner.clone(),
                command: command.into(),
                arguments: arguments.into(),
            },
            Some(_) => Route::Feedback("This command is temporarily unavailable.".into()),
            None => Route::Feedback("Unknown command. Use /help.".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec(name: &str) -> CommandSpec {
        CommandSpec {
            name: name.into(),
            description: "Test".into(),
        }
    }
    #[test]
    fn registration_is_atomic_bounded_and_sealed_before_sessions() {
        let mut registry = Registry::default();
        assert!(
            registry
                .register("one", vec![spec("ok"), spec("help")])
                .is_err()
        );
        assert_eq!(registry.list().len(), 1);
        registry.register("one", vec![spec("hello")]).unwrap();
        assert!(
            registry
                .register("two", vec![spec("other"), spec("hello")])
                .is_err()
        );
        assert_eq!(registry.list().len(), 2);
        assert!(
            matches!(registry.route("/hello 안녕 😀"),Route::Plugin {owner,arguments,..} if owner=="one" && arguments=="안녕 😀")
        );
        registry.seal();
        assert!(registry.register("two", vec![spec("other")]).is_err());
        registry.disable("one");
        assert!(matches!(registry.route("hello secret"), Route::Feedback(_)));
        assert_eq!(registry.list().len(), 1);
    }
    #[test]
    fn help_stays_bounded_at_capacity() {
        let mut registry = Registry::default();
        for owner in 0..4 {
            registry
                .register(
                    &format!("plugin{owner}"),
                    (0..32)
                        .map(|n| spec(&format!("command_{owner}_{n:022}")))
                        .collect(),
                )
                .unwrap();
        }
        assert!(
            registry
                .register("overflow", vec![spec("overflow")])
                .is_err()
        );
        for page in 1..=17 {
            let Route::Feedback(text) = registry.route(&format!("help {page}")) else {
                panic!()
            };
            assert!(text.len() <= 512);
        }
        for args in ["0", "-1", "99999999999999999999999999", "secret"] {
            assert!(
                matches!(registry.route(&format!("help {args}")),Route::Feedback(text) if text.starts_with("Usage:"))
            );
        }
    }
}
