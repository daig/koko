//! Narrow real-process adapters for terminal probes, platform paths, and bootstrap files.

use crate::bootstrap::{BootstrapFileSystem, PlatformPaths, TerminalProbe};
use directories::BaseDirs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Copy)]
pub struct RealProcess;

impl RealProcess {
    pub fn user_history_file(self) -> Option<PathBuf> {
        let base = BaseDirs::new()?;
        #[cfg(target_os = "macos")]
        let directory = base.data_local_dir().join("Koko");
        #[cfg(target_os = "windows")]
        let directory = base.data_local_dir().join("Koko");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let directory = base
            .state_dir()
            .map_or_else(|| base.home_dir().join(".local/state"), Path::to_path_buf)
            .join("koko");
        Some(directory.join("history"))
    }
}
impl TerminalProbe for RealProcess {
    fn stdin_is_terminal(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn stdout_is_terminal(&self) -> bool {
        std::io::stdout().is_terminal()
    }

    fn stderr_is_terminal(&self) -> bool {
        std::io::stderr().is_terminal()
    }
}

impl PlatformPaths for RealProcess {
    fn user_config_file(&self) -> Option<PathBuf> {
        let base = BaseDirs::new()?;
        #[cfg(target_os = "macos")]
        let directory = base.config_dir().join("Koko");
        #[cfg(target_os = "windows")]
        let directory = base.config_dir().join("Koko");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let directory = base.config_dir().join("koko");
        Some(directory.join("config.toml"))
    }

    fn home_directory(&self) -> Option<PathBuf> {
        BaseDirs::new().map(|base| base.home_dir().to_path_buf())
    }
}

impl BootstrapFileSystem for RealProcess {
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.try_exists().unwrap_or(false)
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn parent_is_directory(&self, path: &Path) -> bool {
        path.parent().unwrap_or_else(|| Path::new(".")).is_dir()
    }
}
