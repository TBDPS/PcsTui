//! Configuration and diagnostics overlay.
//!
//! The TUI is the primary configuration surface. This panel keeps the common
//! operations (diagnostics, plaintext export, safe import, re-login) inside
//! the terminal without expanding the CLI.

use std::path::Path;

use crate::config::RuntimeConfig;

#[derive(Debug, Clone)]
pub struct Check {
    pub label: String,
    pub ok: bool,
    pub hint: String,
}

pub struct ConfigPanel {
    pub checks: Vec<Check>,
    pub message: Option<(String, bool)>,
    pub busy: bool,
}

impl ConfigPanel {
    pub fn open(config: &RuntimeConfig, queue_error: Option<&str>) -> Self {
        let mut checks = vec![
            Check {
                label: "配置文件存在".to_string(),
                ok: config.paths.config_file.is_file(),
                hint: "启动 TUI 会进入登录页".to_string(),
            },
            Check {
                label: "配置文件权限".to_string(),
                ok: file_is_private(&config.paths.config_file),
                hint: "应为 0600".to_string(),
            },
            Check {
                label: "配置目录权限".to_string(),
                ok: dir_is_private(&config.paths.config_dir),
                hint: "应为 0700".to_string(),
            },
            Check {
                label: "下载目录可写".to_string(),
                ok: ensure_writable(&config.download_dir()),
                hint: "检查磁盘空间/权限".to_string(),
            },
            Check {
                label: "上传断点目录可写".to_string(),
                ok: ensure_writable(&config.paths.upload_state_dir),
                hint: "检查磁盘空间/权限".to_string(),
            },
            Check {
                label: "待办队列可读".to_string(),
                ok: queue_error.is_none(),
                hint: queue_error.unwrap_or("读取失败").to_string(),
            },
        ];
        if config.paths.queue_file.is_file() {
            checks.push(Check {
                label: "待办队列权限".to_string(),
                ok: file_is_private(&config.paths.queue_file),
                hint: "应为 0600".to_string(),
            });
        }
        Self {
            checks,
            message: None,
            busy: false,
        }
    }

    pub fn refresh_local(&mut self, config: &RuntimeConfig, queue_error: Option<&str>) {
        let message = self.message.clone();
        let busy = self.busy;
        *self = Self::open(config, queue_error);
        self.message = message;
        self.busy = busy;
    }

    pub fn set_message(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some((text.into(), error));
    }
}

#[cfg(unix)]
fn file_is_private(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.permissions().mode() & 0o077 == 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn file_is_private(_path: &Path) -> bool {
    true
}

#[cfg(unix)]
fn dir_is_private(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.permissions().mode() & 0o077 == 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn dir_is_private(_path: &Path) -> bool {
    true
}

fn ensure_writable(path: &Path) -> bool {
    if std::fs::create_dir_all(path).is_err() {
        return false;
    }
    let probe = path.join(".pcstui-write-test");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};

    fn test_paths(root: &Path) -> Paths {
        Paths {
            root: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/pcstui.json"),
            download_dir: root.join("downloads"),
            tmp_dir: root.join("tmp"),
            upload_state_dir: root.join("config/upload-state"),
            queue_file: root.join("config/queue.json"),
        }
    }

    #[test]
    fn reports_missing_config_and_creates_writable_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let config = RuntimeConfig {
            paths: test_paths(dir.path()),
            file: ConfigFile::default(),
        };
        let panel = ConfigPanel::open(&config, Some("bad queue"));
        let config_check = panel
            .checks
            .iter()
            .find(|check| check.label == "配置文件存在")
            .unwrap();
        assert!(!config_check.ok);
        let queue_check = panel
            .checks
            .iter()
            .find(|check| check.label == "待办队列可读")
            .unwrap();
        assert!(!queue_check.ok);
        assert!(dir.path().join("downloads").is_dir());
        assert!(dir.path().join("config/upload-state").is_dir());
    }
}
