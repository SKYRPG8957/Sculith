use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use sculith_core::ServerStatus;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub network: Network,
    pub bedrock: Bedrock,
    pub plugins: Vec<PluginConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bedrock {
    /// Opt-in transport/login preview until chunk loading is implemented.
    pub sessions: bool,
    pub require_authentication: bool,
    pub login_timeout_secs: u64,
}

impl Default for Bedrock {
    fn default() -> Self {
        Self {
            sessions: false,
            require_authentication: true,
            login_timeout_secs: 15,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub motd: String,
    pub max_players: u32,
    pub tick_rate: u32,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            motd: "Sculith / discovery only".into(),
            max_players: 20,
            tick_rate: 20,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Network {
    pub java: Listener,
    pub bedrock: Listener,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            java: Listener {
                enabled: true,
                bind: "127.0.0.1:25565".parse().unwrap(),
            },
            bedrock: Listener {
                enabled: true,
                bind: "127.0.0.1:19132".parse().unwrap(),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    #[serde(default = "enabled")]
    pub enabled: bool,
    pub bind: SocketAddr,
}

fn enabled() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    1000
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let default_path = Path::new("sculith.toml");
        let path = match path {
            Some(path) => Some(path),
            None if default_path.try_exists()? => Some(default_path),
            None => None,
        };
        let mut config: Self = if let Some(path) = path {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read config {}", path.display()))?;
            let mut config: Self =
                toml::from_str(&content).context("invalid server configuration")?;
            let absolute = path.canonicalize()?;
            let base = absolute.parent().context("config path has no parent")?;
            for plugin in &mut config.plugins {
                if plugin.path.is_relative() {
                    plugin.path = base.join(&plugin.path);
                }
            }
            config
        } else {
            Self::default()
        };
        config.validate()?;
        for plugin in &mut config.plugins {
            plugin.path = plugin.path.canonicalize().with_context(|| {
                format!(
                    "plugin executable missing: {} (build it first)",
                    plugin.path.display()
                )
            })?;
            ensure!(
                plugin.path.is_file(),
                "plugin path must be an executable file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                ensure!(
                    plugin.path.metadata()?.permissions().mode() & 0o111 != 0,
                    "plugin is not executable: {}",
                    plugin.path.display()
                );
            }
        }
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        self.status()?;
        ensure!(
            (1..=60).contains(&self.bedrock.login_timeout_secs),
            "bedrock.login_timeout_secs must be between 1 and 60"
        );
        ensure!(
            !self.bedrock.sessions
                || self.bedrock.require_authentication
                || self.network.bedrock.bind.ip().is_loopback(),
            "unauthenticated Bedrock sessions are restricted to a loopback bind"
        );
        ensure!(
            !self.bedrock.sessions || self.server.max_players <= 64,
            "Bedrock spawn preview supports at most 64 simultaneous sessions"
        );
        ensure!(
            (1..=100).contains(&self.server.tick_rate),
            "tick_rate must be between 1 and 100"
        );
        ensure!(
            self.network.java.enabled || self.network.bedrock.enabled,
            "enable at least one edition listener"
        );
        ensure!(
            self.plugins.len() <= 32,
            "at most 32 plugins may be configured"
        );
        for plugin in &self.plugins {
            ensure!(
                (10..=10_000).contains(&plugin.timeout_ms),
                "plugin timeout_ms must be between 10 and 10000"
            );
        }
        Ok(())
    }

    pub fn status(&self) -> Result<ServerStatus> {
        let mut status = ServerStatus::new(self.server.motd.clone(), self.server.max_players)
            .map_err(anyhow::Error::msg)?;
        if self.network.bedrock.enabled && self.bedrock.sessions {
            status.stage = "bedrock-spawn-preview".into();
        }
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bind_both_editions_to_loopback() {
        let config = Config::default();
        config.validate().unwrap();
        assert!(config.network.java.bind.ip().is_loopback());
        assert!(config.network.bedrock.bind.ip().is_loopback());
    }

    #[test]
    fn rejects_typos_invalid_timing_and_disabled_server() {
        assert!(toml::from_str::<Config>("[server]\nmax_player = 20").is_err());
        for value in [0, 101] {
            let mut config = Config::default();
            config.server.tick_rate = value;
            assert!(config.validate().is_err());
        }
        let mut config = Config::default();
        config.network.java.enabled = false;
        config.network.bedrock.enabled = false;
        assert!(config.validate().is_err());
    }

    #[test]
    fn offline_sessions_cannot_be_exposed_to_public_or_lan_addresses() {
        let mut config = Config::default();
        config.bedrock.sessions = true;
        config.bedrock.require_authentication = false;
        config.validate().unwrap();
        for address in ["0.0.0.0:19132", "192.168.1.2:19132", "[::]:19132"] {
            config.network.bedrock.bind = address.parse().unwrap();
            assert!(config.validate().is_err(), "{address}");
        }
    }

    #[test]
    fn preview_capacity_cannot_silently_disagree_with_the_advertised_limit() {
        let mut config = Config::default();
        config.bedrock.sessions = true;
        config.server.max_players = 64;
        config.validate().unwrap();
        config.server.max_players = 65;
        assert!(config.validate().is_err());
    }

    #[test]
    fn relative_plugin_paths_resolve_against_config_not_current_directory() {
        let dir = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(executable, dir.path().join("plugin")).unwrap();
        #[cfg(not(unix))]
        std::fs::copy(executable, dir.path().join("plugin")).unwrap();
        let config = dir.path().join("settings.toml");
        std::fs::write(&config, "[[plugins]]\npath = 'plugin'\n").unwrap();
        let result = Config::load(Some(&config)).unwrap();
        assert!(result.plugins[0].path.is_absolute());
    }
}
