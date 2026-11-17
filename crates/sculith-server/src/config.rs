use sculith_plugin_host::PluginConfig;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use sculith_core::ServerStatus;
use serde::Deserialize;

mod bootstrap;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub network: Network,
    pub bedrock: Bedrock,
    pub java: Java,
    pub world: World,
    pub content: Option<Content>,
    pub plugins: Vec<PluginConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Content {
    pub source: PathBuf,
    pub mapping: PathBuf,
    #[serde(default = "content_bind")]
    pub bind: SocketAddr,
    /// Optional HTTP(S) base URL advertised to Java clients, including a proxy prefix.
    #[serde(default)]
    pub public_url: Option<String>,
}

fn content_bind() -> SocketAddr {
    "127.0.0.1:0".parse().expect("literal loopback address")
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct World {
    /// Optional Sculith snapshot, loaded before listeners start.
    pub snapshot_path: Option<PathBuf>,
    /// Checkpoint changed blocks at this interval; zero saves only on orderly shutdown.
    pub autosave_interval_secs: u64,
}

impl Default for World {
    fn default() -> Self {
        Self {
            snapshot_path: None,
            autosave_interval_secs: 60,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Java {
    pub sessions: bool,
    pub require_authentication: bool,
    pub login_timeout_secs: u64,
}

impl Default for Java {
    fn default() -> Self {
        Self {
            sessions: false,
            require_authentication: true,
            login_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bedrock {
    /// Enable native game sessions instead of discovery alone.
    pub sessions: bool,
    pub require_authentication: bool,
    pub login_timeout_secs: u64,
    /// HTTP/WebRTC on the Bedrock TCP port; remote listeners require explicit media settings.
    pub nethernet: Option<bool>,
    /// Persistent operator key. Never automatically replace an existing invalid key.
    pub nethernet_identity_path: PathBuf,
    /// Explicit UDP endpoints for an authenticated non-loopback NetherNet listener.
    pub nethernet_media: Option<crate::protocol::bedrock::nethernet::PublicMedia>,
}

impl Default for Bedrock {
    fn default() -> Self {
        Self {
            sessions: false,
            require_authentication: true,
            login_timeout_secs: 15,
            nethernet: None,
            nethernet_identity_path: "world/nethernet-identity.pem".into(),
            nethernet_media: None,
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

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let default_path = Path::new("sculith.toml");
        if path.is_none() && !bootstrap::exists(default_path)? {
            // `check` inspects the same first-run settings without writing files.
            return Self::parse(bootstrap::TEMPLATE, &std::env::current_dir()?, false);
        }
        Self::load_file(path.unwrap_or(default_path), false)
    }

    pub fn load_for_run(path: Option<&Path>, offline: bool) -> Result<Self> {
        let path = if let Some(path) = path {
            path
        } else {
            let path = Path::new("sculith.toml");
            if bootstrap::create(path)? {
                tracing::info!(path = %path.display(), "Created server configuration");
            }
            path
        };
        Self::load_file(path, offline)
    }

    fn load_file(path: &Path, offline: bool) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let absolute = path.canonicalize()?;
        let base = absolute.parent().context("config path has no parent")?;
        Self::parse(&content, base, offline)
    }

    fn parse(content: &str, base: &Path, offline: bool) -> Result<Self> {
        let mut config: Self = toml::from_str(content).context("invalid server configuration")?;
        if config.bedrock.nethernet_identity_path.is_relative() {
            config.bedrock.nethernet_identity_path =
                base.join(&config.bedrock.nethernet_identity_path);
        }
        if let Some(snapshot) = &mut config.world.snapshot_path
            && snapshot.is_relative()
        {
            *snapshot = base.join(&*snapshot);
        }
        for plugin in &mut config.plugins {
            if plugin.path.is_relative() {
                plugin.path = base.join(&plugin.path);
            }
        }
        if let Some(content) = &mut config.content {
            if content.source.is_relative() {
                content.source = base.join(&content.source);
            }
            if content.mapping.is_relative() {
                content.mapping = base.join(&content.mapping);
            }
        }
        if offline {
            config.java.require_authentication = false;
            config.bedrock.require_authentication = false;
        }
        // Apply runtime overrides before validating listener and media scope.
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
            self.world.autosave_interval_secs <= 86_400,
            "world.autosave_interval_secs must be 0 through 86400"
        );
        if let Some(content) = &self.content {
            let public_url = content
                .public_url
                .as_deref()
                .map(crate::content::PackBaseUrl::parse)
                .transpose()?;
            ensure!(
                content.bind.ip().is_loopback() || public_url.is_some(),
                "non-loopback content.bind requires content.public_url"
            );
            ensure!(
                !self.java.sessions
                    || self.network.java.bind.ip().is_loopback()
                    || public_url.as_ref().is_some_and(|url| !url.is_loopback()),
                "public Java sessions with content require a non-loopback content.public_url"
            );
            ensure!(
                content.source.is_dir() && content.mapping.is_file(),
                "content source directory and texture mapping must exist"
            );
            ensure!(
                self.java.sessions || (self.bedrock.sessions && self.network.bedrock.enabled),
                "content requires an enabled edition session preview"
            );
        }
        ensure!(
            !self.java.sessions || self.network.java.enabled,
            "Java sessions require an enabled Java listener"
        );
        ensure!(
            !self.java.sessions
                || self.java.require_authentication
                || self.network.java.bind.ip().is_loopback(),
            "unauthenticated Java sessions are restricted to a loopback bind"
        );
        ensure!(
            (1..=60).contains(&self.java.login_timeout_secs),
            "java.login_timeout_secs must be between 1 and 60"
        );
        ensure!(
            !self.java.sessions || self.server.max_players <= 64,
            "Java sessions support at most 64 simultaneous sessions"
        );
        ensure!(
            self.bedrock.nethernet != Some(true)
                || (self.bedrock.sessions && self.network.bedrock.enabled),
            "NetherNet requires enabled Bedrock sessions"
        );
        if let Some(media) = &self.bedrock.nethernet_media {
            ensure!(
                self.nethernet_enabled() && self.bedrock.require_authentication,
                "public NetherNet media requires enabled authenticated sessions"
            );
            media.validate(self.network.bedrock.bind.ip(), self.server.max_players)?;
            ensure!(
                !(media.ports[0]..=media.ports[1]).contains(&self.network.bedrock.bind.port()),
                "NetherNet media ports must not include the Bedrock discovery UDP port"
            );
        }
        ensure!(
            !self.nethernet_enabled()
                || self.network.bedrock.bind.ip().is_loopback()
                || self.bedrock.nethernet_media.is_some(),
            "public NetherNet requires bedrock.nethernet_media settings"
        );
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
            self.plugins.len() <= sculith_plugin_host::MAX_PLUGINS,
            "at most 32 plugins may be configured"
        );
        for plugin in &self.plugins {
            plugin.validate()?;
        }
        Ok(())
    }

    pub fn status(&self) -> Result<ServerStatus> {
        let mut status = ServerStatus::new(self.server.motd.clone(), self.server.max_players)
            .map_err(anyhow::Error::msg)?;
        if self.network.bedrock.enabled && self.bedrock.sessions {
            status.stage = "bedrock-spawn-preview".into();
        }
        if self.java.sessions {
            status.stage = if self.bedrock.sessions && self.network.bedrock.enabled {
                "shared-world-preview"
            } else {
                "java-spawn-preview"
            }
            .into();
        }
        Ok(status)
    }

    pub fn nethernet_enabled(&self) -> bool {
        self.network.bedrock.enabled
            && self.bedrock.sessions
            && self.bedrock.nethernet.unwrap_or_else(|| {
                self.network.bedrock.bind.ip().is_loopback()
                    || self.bedrock.nethernet_media.is_some()
            })
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
    fn content_paths_resolve_against_config_and_public_delivery_requires_an_advertised_url() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("pack")).unwrap();
        std::fs::write(directory.path().join("map.json"), "{}").unwrap();
        let path = directory.path().join("server.toml");
        std::fs::write(&path, "[java]\nsessions = true\nrequire_authentication = false\n[content]\nsource = 'pack'\nmapping = 'map.json'\n").unwrap();
        let mut config = Config::load(Some(&path)).unwrap();
        assert!(config.content.as_ref().unwrap().source.is_absolute());
        assert!(config.content.as_ref().unwrap().mapping.is_absolute());
        config.content.as_mut().unwrap().bind = "0.0.0.0:0".parse().unwrap();
        assert!(config.validate().is_err());
        config.content.as_mut().unwrap().public_url = Some("https://packs.example.com".into());
        config.validate().unwrap();
        config.java.require_authentication = true;
        config.network.java.bind = "0.0.0.0:25565".parse().unwrap();
        config.validate().unwrap();
        for url in [
            None,
            Some("http://127.1"),
            Some("http://[::ffff:127.0.0.1]"),
            Some("http://localhost."),
        ] {
            config.content.as_mut().unwrap().public_url = url.map(str::to_owned);
            assert!(config.validate().is_err(), "{url:?}");
        }
        config.content.as_mut().unwrap().public_url = Some("https://packs.example.com".into());
        config.content.as_mut().unwrap().bind = content_bind();
        config.java.sessions = false;
        assert!(config.validate().is_err());
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
    fn world_autosave_defaults_and_interval_bounds() {
        let config: Config =
            toml::from_str("[world]\nsnapshot_path = 'world/region.sculith'").unwrap();
        assert_eq!(config.world.autosave_interval_secs, 60);
        for seconds in [0, 1, 60, 86_400] {
            let mut config = Config::default();
            config.world.autosave_interval_secs = seconds;
            config.validate().unwrap();
        }
        for seconds in [86_401, u64::MAX] {
            let mut config = Config::default();
            config.world.autosave_interval_secs = seconds;
            assert!(config.validate().is_err());
        }
        assert!(toml::from_str::<Config>("[world]\nautosave_interval_secs = -1").is_err());
    }

    #[test]
    fn java_offline_preview_requires_explicit_local_scope_and_capacity() {
        let mut config = Config::default();
        config.java.sessions = true;
        config.validate().unwrap();
        config.network.java.bind = "0.0.0.0:25565".parse().unwrap();
        config.validate().unwrap();
        config.network.java.bind = "127.0.0.1:25565".parse().unwrap();
        config.java.require_authentication = false;
        config.validate().unwrap();
        assert_eq!(config.status().unwrap().stage, "java-spawn-preview");
        config.server.max_players = 65;
        assert!(config.validate().is_err());
        config.server.max_players = 20;
        for address in ["0.0.0.0:25565", "192.168.1.2:25565", "[::]:25565"] {
            config.network.java.bind = address.parse().unwrap();
            assert!(config.validate().is_err());
        }
        config.network.java.bind = "127.0.0.1:25565".parse().unwrap();
        config.network.java.enabled = false;
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
    fn public_nethernet_requires_authentication_explicit_media_and_capacity() {
        let mut config = Config::default();
        config.bedrock.sessions = true;
        config.bedrock.nethernet = Some(true);
        config.network.bedrock.bind = "0.0.0.0:19132".parse().unwrap();
        assert!(config.validate().is_err());
        config.bedrock.nethernet_media = Some(crate::protocol::bedrock::nethernet::PublicMedia {
            ports: [19133, 19196],
            advertised_ip: Some("192.0.2.10".parse().unwrap()),
        });
        config.validate().unwrap();
        config.bedrock.nethernet = None;
        assert!(config.nethernet_enabled());
        config.validate().unwrap();
        config.bedrock.require_authentication = false;
        assert!(config.validate().is_err());
        config.bedrock.require_authentication = true;
        config.bedrock.nethernet_media.as_mut().unwrap().ports = [19132, 19195];
        assert!(config.validate().is_err());
        config.bedrock.nethernet_media.as_mut().unwrap().ports = [19133, 19134];
        assert!(config.validate().is_err());
        config.bedrock.nethernet_media.as_mut().unwrap().ports = [19133, 19196];
        config.bedrock.nethernet = Some(false);
        assert!(config.validate().is_err());
        config.bedrock.nethernet = Some(true);
        config.network.bedrock.bind = "127.0.0.1:19132".parse().unwrap();
        assert!(config.validate().is_err());
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
    fn nethernet_defaults_to_local_sessions_and_explicit_opt_out_is_preserved() {
        let mut config = Config::default();
        assert!(!config.nethernet_enabled());
        config.bedrock.sessions = true;
        assert!(config.nethernet_enabled());
        config.validate().unwrap();
        config.bedrock.nethernet = Some(false);
        assert!(!config.nethernet_enabled());
        config.bedrock.nethernet = None;
        config.network.bedrock.bind = "0.0.0.0:19132".parse().unwrap();
        config.validate().unwrap();
        assert!(!config.nethernet_enabled());
        config.bedrock.nethernet = Some(true);
        assert!(config.validate().is_err());
        config.network.bedrock.bind = "127.0.0.1:19132".parse().unwrap();
        config.validate().unwrap();
        config.bedrock.sessions = false;
        assert!(config.validate().is_err());
        config.bedrock.sessions = true;
        config.network.bedrock.enabled = false;
        assert!(config.validate().is_err());
    }

    #[test]
    fn identity_path_resolves_against_config_without_creating_a_key_during_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            "[bedrock]\nsessions = true\nnethernet_identity_path = 'keys/operator.pem'\n",
        )
        .unwrap();
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(
            config.bedrock.nethernet_identity_path,
            dir.path().canonicalize().unwrap().join("keys/operator.pem")
        );
        assert!(!dir.path().join("keys").exists());
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
