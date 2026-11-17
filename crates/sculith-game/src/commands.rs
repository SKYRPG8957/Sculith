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
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Plugin {
        owner: String,
        command: String,
        arguments: String,
    },
    Feedback(String),
}
#[derive(Debug, Clone, Copy)]
pub struct Sources {
    pub player: bool,
    pub console: bool,
}
impl Sources {
    pub const PLAYER: Self = Self {
        player: true,
        console: false,
    };
}
struct Entry {
    owner: String,
    spec: CommandSpec,
    enabled: bool,
    sources: Sources,
}
#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<String, Entry>,
    sealed: bool,
}
impl Registry {
    pub fn register(&mut self, plugin: &str, specs: Vec<CommandSpec>) -> Result<()> {
        self.register_sources(plugin, specs, Sources::PLAYER)
    }
    pub fn register_sources(
        &mut self,
        plugin: &str,
        specs: Vec<CommandSpec>,
        sources: Sources,
    ) -> Result<()> {
        ensure!(!self.sealed, "command registration has closed");
        ensure!(
            sources.player || sources.console,
            "command requires a source"
        );
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
                .filter(|entry| entry.owner == plugin)
                .count()
                + specs.len()
                <= 32,
            "plugin command capacity exceeded"
        );
        let mut names = BTreeSet::new();
        for spec in &specs {
            spec.validate().map_err(anyhow::Error::msg)?;
            ensure!(
                !sources.console || !matches!(spec.name.as_str(), "list" | "save" | "stop"),
                "reserved console command name"
            );
            ensure!(
                spec.name != "help"
                    && !self.entries.contains_key(&spec.name)
                    && names.insert(&spec.name),
                "duplicate or reserved command name"
            );
        }
        for spec in specs {
            self.entries.insert(
                spec.name.clone(),
                Entry {
                    owner: plugin.into(),
                    spec,
                    enabled: true,
                    sources,
                },
            );
        }
        Ok(())
    }
    pub fn seal(&mut self) {
        self.sealed = true;
    }
    pub fn disable(&mut self, plugin: &str) {
        for entry in self.entries.values_mut() {
            if entry.owner == plugin {
                entry.enabled = false;
            }
        }
    }
    pub fn list(&self) -> Vec<CommandSpec> {
        self.list_for(false)
    }
    fn list_for(&self, console: bool) -> Vec<CommandSpec> {
        let mut entries = vec![CommandSpec {
            name: "help".into(),
            description: "List available commands".into(),
        }];
        entries.extend(
            self.entries
                .values()
                .filter(|entry| {
                    entry.enabled
                        && if console {
                            entry.sources.console
                        } else {
                            entry.sources.player
                        }
                })
                .map(|entry| entry.spec.clone()),
        );
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
    pub fn route(&self, input: &str) -> Route {
        self.route_for(input, false)
    }
    pub fn route_console(&self, input: &str) -> Route {
        self.route_for(input, true)
    }
    fn route_for(&self, input: &str, console: bool) -> Route {
        let input = input.strip_prefix('/').unwrap_or(input);
        let (command, arguments) = input
            .split_once(char::is_whitespace)
            .map_or((input, ""), |(command, args)| (command, args.trim()));
        if command == "help" {
            let entries = self.list_for(console);
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
            Some(entry)
                if entry.enabled
                    && if console {
                        entry.sources.console
                    } else {
                        entry.sources.player
                    } =>
            {
                Route::Plugin {
                    owner: entry.owner.clone(),
                    command: command.into(),
                    arguments: arguments.into(),
                }
            }
            Some(entry) if entry.enabled => {
                Route::Feedback("This command is not available from this source.".into())
            }
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
    #[test]
    fn command_sources_preserve_legacy_players_and_hide_console_only_commands() {
        let mut registry = Registry::default();
        registry
            .register("old", vec![spec("legacy"), spec("save")])
            .unwrap();
        assert!(
            registry
                .register_sources(
                    "admin",
                    vec![spec("admin"), spec("stop")],
                    Sources {
                        player: false,
                        console: true
                    }
                )
                .is_err()
        );
        assert!(matches!(
            registry.route_console("admin"),
            Route::Feedback(_)
        ));
        registry
            .register_sources(
                "admin",
                vec![spec("admin")],
                Sources {
                    player: false,
                    console: true,
                },
            )
            .unwrap();
        registry
            .register_sources(
                "both",
                vec![spec("both")],
                Sources {
                    player: true,
                    console: true,
                },
            )
            .unwrap();
        assert!(!registry.list().iter().any(|s| s.name == "admin"));
        assert!(matches!(registry.route("admin secret"), Route::Feedback(_)));
        assert!(matches!(
            registry.route_console("legacy secret"),
            Route::Feedback(_)
        ));
        assert!(matches!(registry.route_console("save"), Route::Feedback(_)));
        assert_eq!(
            registry.route("both 안녕 ; $(echo value)"),
            registry.route_console("both 안녕 ; $(echo value)")
        );
        assert!(
            matches!(registry.route_console("admin secret"),Route::Plugin{owner,arguments,..} if owner=="admin" && arguments=="secret")
        );
        registry.disable("admin");
        assert!(matches!(
            registry.route_console("admin secret"),
            Route::Feedback(_)
        ));
    }
}
