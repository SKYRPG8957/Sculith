use std::{fs, io::Write, path::Path};

use anyhow::{Context, Result};

pub(super) const TEMPLATE: &str = include_str!("default.toml");

// Do not mistake a dangling symlink for an absent configuration.
pub(super) fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("cannot inspect config {}", path.display()))
        }
    }
}

pub(super) fn create(path: &Path) -> Result<bool> {
    if exists(path)? {
        return Ok(false);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut staged = tempfile::Builder::new()
        .prefix(".sculith-config-")
        .tempfile_in(parent)
        .with_context(|| format!("cannot create config in {}", parent.display()))?;
    staged.write_all(TEMPLATE.as_bytes())?;
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(path) {
        Ok(_) => Ok(true),
        // Another writer won. Read its file; never replace it, even if invalid.
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => {
            Err(error.error).with_context(|| format!("cannot create config {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn first_run_enables_authenticated_persistent_sessions_without_content_or_plugins() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sculith.toml");
        assert!(create(&path).unwrap());
        let config = Config::load_file(&path, false).unwrap();
        assert!(config.java.sessions && config.bedrock.sessions && config.nethernet_enabled());
        assert!(config.java.require_authentication && config.bedrock.require_authentication);
        assert!(config.network.java.bind.ip().is_loopback());
        assert!(config.network.bedrock.bind.ip().is_loopback());
        assert_eq!(config.world.autosave_interval_secs, 60);
        assert_eq!(
            config.world.snapshot_path.unwrap(),
            directory
                .path()
                .canonicalize()
                .unwrap()
                .join("world/region.sculith")
        );
        assert!(config.content.is_none() && config.plugins.is_empty());
        // Reading settings must not create a world or signing key.
        assert!(!directory.path().join("world").exists());
    }

    #[test]
    fn concurrent_first_runs_publish_one_complete_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sculith.toml");
        let barrier = std::sync::Barrier::new(16);
        let created = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        let created = create(&path).unwrap();
                        assert_eq!(fs::read_to_string(&path).unwrap(), TEMPLATE);
                        Config::load_file(&path, false).unwrap();
                        usize::from(created)
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .sum::<usize>()
        });
        assert_eq!(created, 1);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn existing_config_and_directories_are_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sculith.toml");
        let bytes = b"operator settings: invalid TOML on purpose";
        fs::write(&path, bytes).unwrap();
        assert!(!create(&path).unwrap());
        assert!(Config::load_file(&path, true).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let other = directory.path().join("other.toml");
        fs::create_dir(&other).unwrap();
        assert!(!create(&other).unwrap());
        assert!(Config::load_file(&other, true).is_err());
        assert!(other.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_is_preserved_and_rejected_as_missing_input() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sculith.toml");
        let target = directory.path().join("absent.toml");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(exists(&path).unwrap());
        assert!(!create(&path).unwrap());
        assert!(Config::load_file(&path, false).is_err());
        assert_eq!(fs::read_link(path).unwrap(), target);
        assert!(!target.exists());
    }

    #[test]
    fn offline_is_a_validated_in_memory_override_and_does_not_enable_legacy_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server.toml");
        create(&path).unwrap();
        let original = fs::read(&path).unwrap();
        let config = Config::load_for_run(Some(&path), true).unwrap();
        assert!(!config.java.require_authentication && !config.bedrock.require_authentication);
        assert_eq!(fs::read(&path).unwrap(), original);
        let mut config = Config::load_for_run(Some(&path), false).unwrap();
        assert!(config.java.require_authentication && config.bedrock.require_authentication);
        for edition in ["java", "bedrock"] {
            let public = TEMPLATE.replace(
                if edition == "java" {
                    "127.0.0.1:25565"
                } else {
                    "127.0.0.1:19132"
                },
                if edition == "java" {
                    "0.0.0.0:25565"
                } else {
                    "0.0.0.0:19132"
                },
            );
            fs::write(&path, &public).unwrap();
            assert!(Config::load_for_run(Some(&path), true).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), public);
        }
        fs::write(&path, "[server]\nmotd = 'Legacy discovery'\n").unwrap();
        config = Config::load_for_run(Some(&path), true).unwrap();
        assert!(!config.java.sessions && !config.bedrock.sessions);
        let missing = directory.path().join("typo.toml");
        assert!(Config::load_for_run(Some(&missing), true).is_err());
        assert!(!missing.exists());
    }
}
