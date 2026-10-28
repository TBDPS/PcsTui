//! Single-instance advisory lock for pcstui.
//!
//! Prevents multiple TUI instances or mutating CLI operations from concurrently
//! writing to `config/pcstui.json`, `config/queue.json`, and upload states.
//!
//! Uses kernel-level file locks (`flock` on Unix, `LockFileEx` on Windows) via
//! `fd-lock`. If a process crashes or is killed with SIGKILL, the operating system
//! automatically releases the lock, avoiding stale lockfile bugs.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fd_lock::{RwLock, RwLockWriteGuard};

/// An acquired single-instance lock. The lock is held until this struct is dropped.
pub struct InstanceLock {
    _guard: RwLockWriteGuard<'static, File>,
    path: PathBuf,
}

impl std::fmt::Debug for InstanceLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceLock")
            .field("path", &self.path)
            .finish()
    }
}

impl InstanceLock {
    /// Attempt to acquire an exclusive lock in `config_dir` with the given `mode` (e.g. "tui", "cli-download").
    ///
    /// If another instance currently holds the lock, this function returns an error
    /// describing the existing process (PID and mode).
    pub fn try_acquire(config_dir: &Path, mode: &str) -> Result<Self> {
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("创建配置目录失败: {}", config_dir.display()))?;

        let lock_path = config_dir.join("pcstui.lock");

        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let file = options
            .open(&lock_path)
            .with_context(|| format!("打开锁文件失败: {}", lock_path.display()))?;

        // Box::leak the RwLock so the guard can have 'static lifetime while keeping the file open.
        let rwlock = Box::leak(Box::new(RwLock::new(file)));

        match rwlock.try_write() {
            Ok(mut guard) => {
                let pid = std::process::id();
                let now = chrono::Local::now().to_rfc3339();
                let info = format!("pid={pid}\nmode={mode}\nstarted_at={now}\n");

                guard.set_len(0).ok();
                guard.seek(SeekFrom::Start(0)).ok();
                let _ = guard.write_all(info.as_bytes());
                let _ = guard.sync_all();

                Ok(Self {
                    _guard: guard,
                    path: lock_path,
                })
            }
            Err(_) => {
                let owner_info = read_lock_owner(&lock_path);
                let message = if let Some((pid, owner_mode)) = owner_info {
                    format!(
                        "已有另一个 pcstui 实例在运行（PID: {pid}，模式: {owner_mode}）。\n\
                         为避免配置、传输队列与断点状态冲突损坏，同一配置目录仅允许运行一个实例。"
                    )
                } else {
                    "已有另一个 pcstui 实例在运行中。\n\
                     为避免配置、传输队列与断点状态冲突损坏，同一配置目录仅允许运行一个实例。"
                        .to_string()
                };
                bail!("{message}");
            }
        }
    }

    /// Path to the lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn read_lock_owner(path: &Path) -> Option<(u32, String)> {
    let mut file = File::open(path).ok()?;
    let mut content = String::new();
    file.read_to_string(&mut content).ok()?;

    let mut pid: Option<u32> = None;
    let mut mode = "unknown".to_string();

    for line in content.lines() {
        if let Some(val) = line.strip_prefix("pid=") {
            pid = val.trim().parse().ok();
        } else if let Some(val) = line.strip_prefix("mode=") {
            mode = val.trim().to_string();
        }
    }

    pid.map(|p| (p, mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_instance_lock_blocks_concurrent_acquisition() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");

        let lock1 = InstanceLock::try_acquire(&config_dir, "tui").unwrap();
        assert!(lock1.path().is_file());

        // Second acquisition on the same directory must fail
        let lock2_err = InstanceLock::try_acquire(&config_dir, "tui").unwrap_err();
        let err_str = lock2_err.to_string();
        assert!(
            err_str.contains("已有另一个 pcstui 实例在运行"),
            "Unexpected error message: {err_str}"
        );
        assert!(err_str.contains("模式: tui"));

        // Dropping lock1 releases the lock
        drop(lock1);

        // Third acquisition must now succeed
        let lock3 = InstanceLock::try_acquire(&config_dir, "cli-download").unwrap();
        drop(lock3);
    }
}
