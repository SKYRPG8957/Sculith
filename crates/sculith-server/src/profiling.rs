//! Optional plugin profiling reports, independent of a management UI.
use anyhow::{Context, Result, ensure};
use clap::Args;
use sculith_plugin_host::{Plugins, profiling::Snapshot};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tracing::{info, warn};

#[derive(Debug, Default, Args)]
pub struct Options {
    /// Print bounded plugin IPC wall-time statistics every 30 seconds.
    #[arg(long, conflicts_with = "profile_plugins")]
    monitor_plugins: bool,
    /// Capture detailed plugin IPC wall timings for 1..300 seconds.
    #[arg(long,requires="profile_output",value_parser=clap::value_parser!(u64).range(1..=300))]
    profile_plugins: Option<u64>,
    /// Write a new JSON report; existing files are never overwritten.
    #[arg(long, requires = "profile_plugins")]
    profile_output: Option<PathBuf>,
}
impl Options {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !(self.monitor_plugins && self.profile_plugins.is_some()),
            "choose monitor or detailed plugin profiling"
        );
        ensure!(
            self.profile_plugins.is_some() == self.profile_output.is_some(),
            "plugin profiling requires a duration and output path"
        );
        if let Some(seconds) = self.profile_plugins {
            ensure!(
                (1..=300).contains(&seconds),
                "plugin profile duration must be 1..300 seconds"
            );
        }
        if let Some(path) = &self.profile_output {
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
                Ok(_) => anyhow::bail!("profile output already exists: {}", path.display()),
            }
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            ensure!(
                parent.is_dir(),
                "profile output parent directory must exist"
            );
        }
        Ok(())
    }
    pub fn start(self, plugins: &mut Plugins) -> Result<Reporter> {
        let deadline = if let Some(seconds) = self.profile_plugins {
            let duration = Duration::from_secs(seconds);
            plugins.start_profile(duration)?;
            info!(seconds, "Plugin IPC profile started");
            Some(Instant::now() + duration)
        } else {
            plugins.monitor(self.monitor_plugins);
            if self.monitor_plugins {
                info!("Plugin IPC monitoring enabled");
            }
            None
        };
        Ok(Reporter {
            monitor: self.monitor_plugins,
            next_monitor: Instant::now() + Duration::from_secs(30),
            deadline,
            path: self.profile_output,
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
    pub fn poll(&mut self, plugins: &mut Plugins) {
        if !self.monitor && self.deadline.is_none() {
            return;
        }
        let now = Instant::now();
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.deadline = None;
            self.export(plugins.profile_snapshot(), "duration");
            plugins.monitor(false);
        }
        if self.monitor && now >= self.next_monitor {
            self.next_monitor = now + Duration::from_secs(30);
            for plugin in plugins.profile_snapshot().plugins {
                info!(plugin=%plugin.name,calls=plugin.ipc.calls,failures=plugin.ipc.failures,p95_ns=plugin.ipc.recent_p95_ns,"Plugin IPC");
            }
        }
    }
    fn export(&mut self, metrics: Snapshot, reason: &'static str) {
        if let Some(path) = self.path.take() {
            self.task = Some(tokio::task::spawn_blocking(move || {
                match save(
                    &path,
                    &Report {
                        finished_reason: reason,
                        metrics,
                    },
                ) {
                    Ok(()) => info!(path=%path.display(),"Plugin IPC profile saved"),
                    Err(error) => {
                        warn!(path=%path.display(),error=%format!("{error:#}"),"Plugin IPC profile could not be saved")
                    }
                }
            }));
        }
    }
    pub async fn finish(mut self, plugins: &mut Plugins) {
        if self.path.is_some() {
            self.export(plugins.profile_snapshot(), "shutdown");
        }
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            warn!(%error,"Plugin profile writer stopped");
        }
    }
}
fn save(path: &Path, report: &Report) -> Result<()> {
    use std::io::Write;
    let data = serde_json::to_vec_pretty(report)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .context("cannot create new profile output")?;
    output.write_all(&data)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn profile_writer_refuses_existing_files_and_validates_window() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.json");
        let status = sculith_core::ServerStatus::new("private motd".into(), 1).unwrap();
        let plugins = Plugins::load(&[], &status).await.unwrap();
        let report = Report {
            finished_reason: "duration",
            metrics: plugins.profile_snapshot(),
        };
        save(&path, &report).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(save(&path, &report).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["scope"], "plugin_ipc_wall_time");
        assert!(
            !String::from_utf8(original)
                .unwrap()
                .contains("private motd")
        );
        assert!(
            Options {
                monitor_plugins: false,
                profile_plugins: Some(1),
                profile_output: Some(path)
            }
            .validate()
            .is_err()
        );
        for seconds in [0, 301] {
            assert!(
                Options {
                    monitor_plugins: false,
                    profile_plugins: Some(seconds),
                    profile_output: Some(directory.path().join("new.json"))
                }
                .validate()
                .is_err()
            );
        }
    }
}
