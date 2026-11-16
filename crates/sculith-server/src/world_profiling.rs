//! Optional world mailbox reports; snapshots contain metadata, never commands.
use anyhow::{Result, ensure};
use clap::Args;
use sculith_runtime::{WorldHandle, profiling::Snapshot};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tracing::{info, warn};

#[derive(Debug, Default, Args)]
#[group(id = "world_profiling")]
pub struct Options {
    /// Print bounded world mailbox wall-time statistics every 30 seconds.
    #[arg(long, conflicts_with = "profile_world")]
    monitor_world: bool,
    /// Capture world admission, queue and service wall timings for 1..300 seconds.
    #[arg(long,requires="world_profile_output",value_parser=clap::value_parser!(u64).range(1..=300))]
    profile_world: Option<u64>,
    /// Write a new world JSON report; existing files are never overwritten.
    #[arg(long, requires = "profile_world")]
    world_profile_output: Option<PathBuf>,
}
fn identity(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(parent.canonicalize()?.join(
        path.file_name()
            .ok_or_else(|| anyhow::anyhow!("profile output needs a filename"))?,
    ))
}
impl Options {
    pub fn validate(&self, plugin_output: Option<&Path>) -> Result<()> {
        ensure!(
            !(self.monitor_world && self.profile_world.is_some()),
            "choose monitor or detailed world profiling"
        );
        ensure!(
            self.profile_world.is_some() == self.world_profile_output.is_some(),
            "world profiling requires a duration and output path"
        );
        if let Some(seconds) = self.profile_world {
            ensure!(
                (1..=300).contains(&seconds),
                "world profile duration must be 1..300 seconds"
            );
        }
        if let Some(path) = &self.world_profile_output {
            crate::profiling::validate_output(path)?;
            if let Some(plugin) = plugin_output {
                let world = identity(path)?;
                let plugin = identity(plugin)?;
                #[cfg(windows)]
                let same = world.to_string_lossy().to_lowercase()
                    == plugin.to_string_lossy().to_lowercase();
                #[cfg(not(windows))]
                let same = world == plugin;
                ensure!(
                    !same,
                    "plugin and world profiles require separate output files"
                );
            }
        }
        Ok(())
    }
    pub fn start(self, world: &WorldHandle) -> Result<Reporter> {
        let deadline = if let Some(seconds) = self.profile_world {
            let duration = Duration::from_secs(seconds);
            world.start_profile(duration)?;
            info!(seconds, "World mailbox profile started");
            Some(Instant::now() + duration)
        } else {
            world.monitor(self.monitor_world)?;
            if self.monitor_world {
                info!("World mailbox monitoring enabled");
            }
            None
        };
        Ok(Reporter {
            monitor: self.monitor_world,
            next_monitor: Instant::now() + Duration::from_secs(30),
            deadline,
            path: self.world_profile_output,
            task: None,
        })
    }
}
pub struct Reporter {
    monitor: bool,
    next_monitor: Instant,
    deadline: Option<Instant>,
    path: Option<PathBuf>,
    task: Option<tokio::task::JoinHandle<()>>,
}
#[derive(Serialize)]
struct Report {
    finished_reason: &'static str,
    #[serde(flatten)]
    metrics: Snapshot,
}
impl Reporter {
    pub fn poll(&mut self, world: &WorldHandle) {
        if !self.monitor && self.deadline.is_none() {
            return;
        }
        let now = Instant::now();
        if self.deadline.is_some_and(|end| now >= end) {
            self.deadline = None;
            self.export(world.profile_snapshot(), "duration");
            if let Err(error) = world.monitor(false) {
                warn!(%error,"World profile could not be disabled");
            }
        }
        if self.monitor && now >= self.next_monitor {
            self.next_monitor = now + Duration::from_secs(30);
            let snapshot = world.profile_snapshot();
            info!(
                region_id = snapshot.region_id,
                completed = snapshot.completed_commands,
                occupied_capacity = snapshot.occupied_capacity,
                queue_p95_ns = snapshot.recent.p95.map(|t| t.queue_ns),
                service_p95_ns = snapshot.recent.p95.map(|t| t.service_ns),
                "World mailbox"
            );
        }
    }
    fn export(&mut self, metrics: Snapshot, reason: &'static str) {
        if let Some(path) = self.path.take() {
            self.task = Some(tokio::task::spawn_blocking(
                move || match crate::profiling::save(
                    &path,
                    &Report {
                        finished_reason: reason,
                        metrics,
                    },
                ) {
                    Ok(()) => info!(path=%path.display(),"World mailbox profile saved"),
                    Err(error) => {
                        warn!(path=%path.display(),error=%format!("{error:#}"),"World mailbox profile could not be saved")
                    }
                },
            ));
        }
    }
    pub async fn finish(mut self, world: &WorldHandle) {
        if self.path.is_some() {
            self.export(world.profile_snapshot(), "shutdown");
        }
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            warn!(%error,"World profile writer stopped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_groups_accept_simultaneous_world_and_plugin_captures() {
        use clap::{CommandFactory, Parser};
        crate::Cli::command().debug_assert();
        assert!(
            crate::Cli::try_parse_from([
                "sculith",
                "run",
                "--profile-world",
                "1",
                "--world-profile-output",
                "world.json",
                "--profile-plugins",
                "1",
                "--profile-output",
                "plugins.json"
            ])
            .is_ok()
        );
    }
    #[test]
    fn validates_duration_paths_and_both_profile_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.json");
        let options = Options {
            monitor_world: false,
            profile_world: Some(1),
            world_profile_output: Some(path.clone()),
        };
        options.validate(None).unwrap();
        assert!(
            options
                .validate(Some(&dir.path().join(".").join("profile.json")))
                .is_err()
        );
        options
            .validate(Some(&dir.path().join("other.json")))
            .unwrap();
        for seconds in [0, 301] {
            assert!(
                Options {
                    profile_world: Some(seconds),
                    world_profile_output: Some(path.clone()),
                    ..Default::default()
                }
                .validate(None)
                .is_err()
            );
        }
        std::fs::write(&path, b"existing").unwrap();
        assert!(options.validate(None).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"existing");
    }
}
