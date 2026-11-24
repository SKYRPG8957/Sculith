use sculith_config::{LoadConfiguration, SculithConfig, world::WorldPreset};
use sculith_util::{Difficulty, GameMode, world_seed::Seed};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::{self, Write},
    net::{IpAddr, Ipv4Addr},
    num::{NonZeroU8, NonZeroU16},
    path::Path,
    str::FromStr,
};

pub const FILE_NAME: &str = "server.properties";

#[derive(Clone, Copy)]
pub enum LoadMode {
    Check,
    Run,
    InitializeProperties,
}

pub fn load(dir: &Path, mode: LoadMode) -> io::Result<SculithConfig> {
    let path = dir.join(FILE_NAME);
    let mut properties = read(&path)?;
    let create = properties.is_none()
        && match mode {
            LoadMode::Check => false,
            LoadMode::Run => {
                !dir.join(SculithConfig::get_path()).try_exists()?
                    && !dir.join("pumpkin.toml").try_exists()?
            }
            LoadMode::InitializeProperties => true,
        };
    let mut config = SculithConfig::load_for_check(dir)?;
    if create {
        write_if_missing(dir, &path, &config)?;
        properties = read(&path)?;
    }
    if let Some(properties) = properties {
        properties.apply(&mut config);
    }
    Ok(config)
}

fn read(path: &Path) -> io::Result<Option<Properties>> {
    match fs::read_to_string(path) {
        Ok(raw) => Properties::parse(path, &raw).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("{}: {error}", path.display()),
        )),
    }
}

enum Property {
    ServerName(String),
    ServerIp(IpAddr),
    ServerPort(NonZeroU16),
    MaxPlayers(u32),
    OnlineMode(bool),
    ViewDistance(NonZeroU8),
    TickDistance(NonZeroU8),
    LevelName(String),
    LevelSeed(Seed),
    LevelType(WorldPreset),
    GameMode(GameMode),
    ForceGameMode(bool),
    Difficulty(Difficulty),
    AllowList(bool),
    JavaEnabled(bool),
    JavaPort(NonZeroU16),
}

#[derive(Default)]
struct Properties {
    fields: Vec<Property>,
    ignored: Vec<String>,
}

impl Properties {
    fn parse(path: &Path, raw: &str) -> io::Result<Self> {
        let mut entries = BTreeMap::new();
        for (index, line) in raw.trim_start_matches('\u{feff}').lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let number = index + 1;
            let (key, value) = line
                .split_once('=')
                .map(|(key, value)| (key.trim(), value.trim()))
                .filter(|(key, _)| !key.is_empty())
                .ok_or_else(|| invalid(path, number, "expected key=value"))?;
            entries.insert(key, (number, value));
        }

        let mut properties = Self::default();
        for (key, (line, value)) in entries {
            let entry = Entry {
                path,
                line,
                key,
                value,
            };
            let property = match key {
                "server-name" if valid_server_name(value) => Property::ServerName(value.to_owned()),
                "server-name" => return Err(entry.invalid("a name without a semicolon")),
                "server-ip" if value.is_empty() => Property::ServerIp(Ipv4Addr::UNSPECIFIED.into()),
                "server-ip" => Property::ServerIp(entry.parse("an IPv4 or IPv6 address")?),
                "server-port" => Property::ServerPort(entry.parse("a port from 1 to 65535")?),
                "max-players" => Property::MaxPlayers(entry.parse("a nonnegative player limit")?),
                "online-mode" => Property::OnlineMode(entry.boolean()?),
                "view-distance" => {
                    let chunks: NonZeroU8 = entry.parse("a view distance from 2 to 64")?;
                    if !(2..=64).contains(&chunks.get()) {
                        return Err(entry.invalid("a view distance from 2 to 64"));
                    }
                    Property::ViewDistance(chunks)
                }
                "tick-distance" => {
                    Property::TickDistance(entry.parse("a positive simulation distance")?)
                }
                "level-name" if !value.is_empty() && valid_line(value) => {
                    Property::LevelName(value.to_owned())
                }
                "level-name" => return Err(entry.invalid("a world folder")),
                "level-seed" if value.is_empty() => continue,
                "level-seed" => Property::LevelSeed(Seed::from(value)),
                "level-type" => Property::LevelType(entry.level_type()?),
                "gamemode" => {
                    Property::GameMode(entry.parse("survival, creative, adventure, or spectator")?)
                }
                "force-gamemode" => Property::ForceGameMode(entry.boolean()?),
                "difficulty" => {
                    Property::Difficulty(entry.parse("peaceful, easy, normal, or hard")?)
                }
                "allow-list" => Property::AllowList(entry.boolean()?),
                "java-enabled" => Property::JavaEnabled(entry.boolean()?),
                "java-port" => Property::JavaPort(entry.parse("a port from 1 to 65535")?),
                "transport" if value.eq_ignore_ascii_case("nethernet") => continue,
                "transport" => return Err(entry.invalid("nethernet")),
                _ => {
                    properties.ignored.push(key.to_owned());
                    continue;
                }
            };
            properties.fields.push(property);
        }
        Ok(properties)
    }

    #[expect(
        clippy::print_stderr,
        reason = "configuration is loaded before logging starts"
    )]
    fn apply(self, config: &mut SculithConfig) {
        let basic = &mut config.basic;
        let world = &mut config.advanced.world;
        let bedrock = &mut config.advanced.networking.bedrock;
        let java = &mut config.advanced.networking.java;
        for field in self.fields {
            match field {
                Property::ServerName(value) => bedrock.motd = value,
                Property::ServerIp(value) => bedrock.nethernet.address.set_ip(value),
                Property::ServerPort(value) => bedrock.nethernet.address.set_port(value.get()),
                Property::MaxPlayers(value) => bedrock.max_players = value,
                Property::OnlineMode(value) => bedrock.online_mode = value,
                Property::ViewDistance(value) => bedrock.view_distance = value,
                Property::TickDistance(value) => bedrock.simulation_distance = value,
                Property::LevelName(value) => basic.default_level_name = value,
                Property::LevelSeed(value) => basic.seed = value,
                Property::LevelType(value) => world.generator = value,
                Property::GameMode(value) => basic.default_gamemode = value,
                Property::ForceGameMode(value) => basic.force_gamemode = value,
                Property::Difficulty(value) => basic.default_difficulty = value,
                Property::AllowList(value) => basic.white_list = value,
                Property::JavaEnabled(value) => java.enabled = value,
                Property::JavaPort(value) => java.address.set_port(value.get()),
            }
        }
        if !self.ignored.is_empty() {
            eprintln!(
                "Sculith ignored unsupported {FILE_NAME} keys: {:?}",
                self.ignored
            );
        }
    }
}

struct Entry<'a> {
    path: &'a Path,
    line: usize,
    key: &'a str,
    value: &'a str,
}

impl Entry<'_> {
    fn invalid(&self, expected: &str) -> io::Error {
        invalid(
            self.path,
            self.line,
            &format!("{:?} expects {expected}", self.key),
        )
    }

    fn parse<T: FromStr>(&self, expected: &str) -> io::Result<T> {
        self.value.parse().map_err(|_| self.invalid(expected))
    }

    fn boolean(&self) -> io::Result<bool> {
        if self.value.eq_ignore_ascii_case("true") {
            Ok(true)
        } else if self.value.eq_ignore_ascii_case("false") {
            Ok(false)
        } else {
            Err(self.invalid("true or false"))
        }
    }

    fn level_type(&self) -> io::Result<WorldPreset> {
        if self.value.eq_ignore_ascii_case("flat") {
            Ok(WorldPreset::Flat)
        } else if self.value.eq_ignore_ascii_case("default")
            || self.value.eq_ignore_ascii_case("normal")
        {
            Ok(WorldPreset::Normal)
        } else {
            Err(self.invalid("DEFAULT or FLAT"))
        }
    }
}

fn invalid(path: &Path, line: usize, message: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}:{line}: {message}", path.display()),
    )
}

fn valid_line(value: &str) -> bool {
    !value.contains(['\0', '\r', '\n'])
}

fn valid_server_name(value: &str) -> bool {
    valid_line(value) && !value.contains(';')
}

pub const fn level_type(preset: WorldPreset) -> &'static str {
    match preset {
        WorldPreset::Normal => "DEFAULT",
        WorldPreset::Flat => "FLAT",
    }
}

fn template(config: &SculithConfig) -> String {
    let basic = &config.basic;
    let net = &config.advanced.networking;
    let mut settings = String::from(
        "# Sculith server settings\n\
         # These keys override the corresponding settings in sculith.toml.\n\
         # Restart the server after changing this file.\n\n",
    );
    if valid_server_name(&net.bedrock.motd) && net.bedrock.motd.trim() == net.bedrock.motd {
        let _ = writeln!(settings, "server-name={}", net.bedrock.motd);
    }
    if net.bedrock.nethernet.address.port() != 0 {
        let _ = writeln!(
            settings,
            "server-port={}",
            net.bedrock.nethernet.address.port()
        );
    }
    let _ = writeln!(settings, "online-mode={}", net.bedrock.online_mode);
    let _ = writeln!(settings, "max-players={}", net.bedrock.max_players);
    let _ = writeln!(settings, "view-distance={}", net.bedrock.view_distance);
    let _ = writeln!(
        settings,
        "tick-distance={}",
        net.bedrock.simulation_distance
    );
    let _ = writeln!(settings, "allow-list={}", basic.white_list);
    settings
        .push_str("\n# Choose a new level-name to create a world with this level-type and seed.\n");
    let name = &basic.default_level_name;
    if valid_line(name) && !name.is_empty() && name.trim() == name {
        let _ = writeln!(settings, "level-name={name}");
    }
    let _ = writeln!(
        settings,
        "level-type={}",
        level_type(config.advanced.world.generator)
    );
    let _ = writeln!(settings, "level-seed={}", basic.seed.0 as i64);
    let _ = writeln!(settings, "gamemode={}", basic.default_gamemode.name());
    let _ = writeln!(settings, "force-gamemode={}", basic.force_gamemode);
    let _ = writeln!(settings, "difficulty={}", basic.default_difficulty.name());
    settings.push_str("\n# Optional Java Edition connections.\n");
    let _ = writeln!(settings, "java-enabled={}", net.java.enabled);
    if net.java.address.port() != 0 {
        let _ = writeln!(settings, "java-port={}", net.java.address.port());
    }
    settings
}

fn write_if_missing(dir: &Path, path: &Path, config: &SculithConfig) -> io::Result<()> {
    let mut pending = tempfile::NamedTempFile::new_in(dir)?;
    pending.write_all(template(config).as_bytes())?;
    pending.as_file().sync_all()?;
    match pending.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_take_precedence_for_the_selected_fields() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path();
        fs::write(
            dir.join("sculith.toml"),
            r#"
seed = "47"
default_level_name = "older"
[networking.bedrock.nethernet]
address = "127.0.0.1:19132"
[networking.bedrock.compression]
threshold = 768
"#,
        )
        .unwrap();
        load(dir, LoadMode::Check).unwrap();
        let advanced = fs::read(dir.join("sculith.toml")).unwrap();
        let raw = "\u{feff}# Server\r\nserver-port=65536\r\nserver-port=19247\r\n\
                   server-name=스컬리스 #1=A\r\nlevel-name=Flat arena\r\nlevel-type=FLAT\r\n\
                   level-seed=-9123\r\nview-distance=8\r\ntick-distance=4\r\n\
                   online-mode=FALSE\r\njava-enabled=true\r\njava-port=25647\r\n\
                   server-portv6=19133\r\n";
        fs::write(dir.join(FILE_NAME), raw).unwrap();
        let config = load(dir, LoadMode::Run).unwrap();
        let net = config.advanced.networking;
        assert_eq!(config.basic.default_level_name, "Flat arena");
        assert_eq!(config.basic.seed.0 as i64, -9123);
        assert_eq!(config.advanced.world.generator, WorldPreset::Flat);
        assert_eq!(net.bedrock.motd, "스컬리스 #1=A");
        assert_eq!(net.bedrock.nethernet.address.to_string(), "127.0.0.1:19247");
        assert_eq!(net.bedrock.compression.info.threshold, 768);
        assert_eq!(net.bedrock.view_distance.get(), 8);
        assert_eq!(net.bedrock.simulation_distance.get(), 4);
        assert!(!net.bedrock.online_mode);
        assert!(net.java.enabled);
        assert_eq!(net.java.address.port(), 25647);
        assert!(net.java.online_mode);
        assert_eq!(fs::read(dir.join("sculith.toml")).unwrap(), advanced);
        assert_eq!(fs::read_to_string(dir.join(FILE_NAME)).unwrap(), raw);
    }

    #[test]
    fn legacy_settings_are_copied_only_when_the_operator_initializes_properties() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path();
        let legacy =
            "default_level_name = \"arena\"\nseed = \"-9\"\n[world]\ngenerator = \"flat\"\n";
        fs::write(dir.join("pumpkin.toml"), legacy).unwrap();
        let ordinary = load(dir, LoadMode::Run).unwrap();
        assert_eq!(ordinary.basic.default_level_name, "arena");
        assert!(!dir.join(FILE_NAME).exists());
        load(dir, LoadMode::InitializeProperties).unwrap();
        let path = dir.join(FILE_NAME);
        let generated = fs::read_to_string(&path).unwrap();
        assert!(generated.contains("level-name=arena\nlevel-type=FLAT\nlevel-seed=-9"));
        let patched = generated.replace("level-name=arena", "level-name=custom");
        fs::write(&path, &patched).unwrap();
        let configured = load(dir, LoadMode::InitializeProperties).unwrap();
        assert_eq!(configured.basic.default_level_name, "custom");
        assert_eq!(fs::read_to_string(path).unwrap(), patched);
        assert_eq!(
            fs::read_to_string(dir.join("pumpkin.toml")).unwrap(),
            legacy
        );
    }

    #[test]
    fn invalid_properties_fail_before_creating_toml_defaults() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path();
        let contents = "server-name=Test\nview-distance=65\n";
        fs::write(dir.join(FILE_NAME), contents).unwrap();
        let Err(error) = load(dir, LoadMode::Run) else {
            panic!("invalid view distance was accepted");
        };
        assert!(
            error
                .to_string()
                .contains("server.properties:2: \"view-distance\"")
        );
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!dir.join("sculith.toml").exists());
        assert_eq!(fs::read_to_string(dir.join(FILE_NAME)).unwrap(), contents);
    }
}
