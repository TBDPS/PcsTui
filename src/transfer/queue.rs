use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::api::models::RemoteItem;
use crate::transfer::types::{DownloadOptions, UploadOptions};

/// One unfinished transfer. The queue intentionally stores only the inputs
/// needed to re-create the task: download `.part` files and upload state files
/// remain the source of truth for byte-level resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedTask {
    pub id: String,
    pub task: QueuedTaskKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "direction", rename_all = "snake_case")]
pub enum QueuedTaskKind {
    Download {
        item: RemoteItem,
        cwd: String,
        download_root: PathBuf,
        options: DownloadOptions,
    },
    Upload {
        local: PathBuf,
        remote_dir: String,
        options: UploadOptions,
    },
}

impl QueuedTask {
    pub fn download(
        item: RemoteItem,
        cwd: String,
        download_root: PathBuf,
        options: DownloadOptions,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            task: QueuedTaskKind::Download {
                item,
                cwd,
                download_root,
                options,
            },
        }
    }

    pub fn upload(local: PathBuf, remote_dir: String, options: UploadOptions) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            task: QueuedTaskKind::Upload {
                local,
                remote_dir,
                options,
            },
        }
    }

    /// Identity of the work, independent of the random queue UUID. Two tasks
    /// with the same identity would write the same local file / remote path
    /// and must not run concurrently.
    pub fn identity(&self) -> TaskIdentity {
        match &self.task {
            QueuedTaskKind::Download {
                item,
                cwd,
                download_root,
                ..
            } => TaskIdentity::Download {
                fs_id: item.fs_id,
                remote_path: item.absolute_path(cwd),
                download_root: download_root.clone(),
            },
            QueuedTaskKind::Upload {
                local, remote_dir, ..
            } => TaskIdentity::Upload {
                local: local.clone(),
                remote_dir: remote_dir.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TaskIdentity {
    Download {
        fs_id: u64,
        remote_path: String,
        download_root: PathBuf,
    },
    Upload {
        local: PathBuf,
        remote_dir: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub tasks: Vec<QueuedTask>,
}

fn default_version() -> u32 {
    1
}

impl Default for QueueFile {
    fn default() -> Self {
        Self {
            version: 1,
            tasks: Vec::new(),
        }
    }
}

/// Small atomic JSON file under `config/queue.json`.
pub struct QueueStore {
    path: PathBuf,
    pub tasks: Vec<QueuedTask>,
}

impl QueueStore {
    pub fn empty(path: PathBuf) -> Self {
        Self {
            path,
            tasks: Vec::new(),
        }
    }

    pub fn load(path: PathBuf) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self::empty(path));
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("读取待办队列失败: {}", path.display()))?;
        let file: QueueFile = serde_json::from_str(&raw)
            .with_context(|| format!("解析待办队列失败: {}", path.display()))?;
        Ok(Self {
            path,
            tasks: file.tasks,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn add(&mut self, task: QueuedTask) -> bool {
        let identity = task.identity();
        if self
            .tasks
            .iter()
            .any(|existing| existing.identity() == identity)
        {
            return false;
        }
        self.tasks.push(task);
        true
    }

    /// Drop duplicate queue entries while keeping the first occurrence.
    /// Returns how many entries were removed.
    pub fn dedupe(&mut self) -> usize {
        let mut seen = std::collections::HashSet::new();
        let before = self.tasks.len();
        self.tasks.retain(|task| seen.insert(task.identity()));
        before - self.tasks.len()
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|task| task.id != id);
        before != self.tasks.len()
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
        }
        let file = QueueFile {
            version: 1,
            tasks: self.tasks.clone(),
        };
        let data = serde_json::to_vec_pretty(&file)?;
        let tmp = PathBuf::from(format!("{}.tmp", self.path.display()));
        {
            let mut handle = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)
                .with_context(|| format!("写入待办队列失败: {}", tmp.display()))?;
            handle.write_all(&data)?;
            handle.write_all(b"\n")?;
            handle.sync_all()?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        }
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("替换待办队列失败: {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn download_task(root: PathBuf, path: &str, fs_id: u64) -> QueuedTask {
        QueuedTask::download(
            RemoteItem {
                fs_id,
                name: path.rsplit('/').next().unwrap_or(path).to_string(),
                path: path.to_string(),
                size: 42,
                ..Default::default()
            },
            "/".to_string(),
            root,
            DownloadOptions::default(),
        )
    }

    #[test]
    fn queue_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let mut store = QueueStore::load(path.clone()).unwrap();
        let task = download_task(dir.path().join("downloads"), "/a.bin", 1);
        let id = task.id.clone();
        assert!(store.add(task));
        store.save().unwrap();
        let mut loaded = QueueStore::load(path).unwrap();
        assert_eq!(loaded.tasks.len(), 1);
        assert!(loaded.remove(&id));
        assert!(loaded.tasks.is_empty());
    }

    #[test]
    fn queue_rejects_duplicate_work() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("downloads");
        let mut store = QueueStore::empty(dir.path().join("queue.json"));

        assert!(store.add(download_task(root.clone(), "/a.bin", 1)));
        assert!(!store.add(download_task(root.clone(), "/a.bin", 1)));
        assert_eq!(store.tasks.len(), 1);

        // Same remote file to a different local root is different work.
        assert!(store.add(download_task(dir.path().join("other"), "/a.bin", 1)));
        // A different remote file with the same name is different work.
        assert!(store.add(download_task(root.clone(), "/sub/a.bin", 2)));
        assert_eq!(store.tasks.len(), 3);

        let local = dir.path().join("local.bin");
        assert!(store.add(QueuedTask::upload(
            local.clone(),
            "/remote".to_string(),
            UploadOptions::default(),
        )));
        assert!(!store.add(QueuedTask::upload(
            local,
            "/remote".to_string(),
            UploadOptions::default(),
        )));
        assert_eq!(store.tasks.len(), 4);
    }

    #[test]
    fn dedupe_keeps_first_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("downloads");
        let first = download_task(root.clone(), "/a.bin", 1);
        let first_id = first.id.clone();
        let duplicate = download_task(root, "/a.bin", 1);
        let duplicate_id = duplicate.id.clone();
        let mut store = QueueStore::empty(dir.path().join("queue.json"));
        store.tasks.push(first);
        store.tasks.push(duplicate);
        assert_eq!(store.dedupe(), 1);
        assert_eq!(store.tasks.len(), 1);
        assert_eq!(store.tasks[0].id, first_id);
        assert_ne!(store.tasks[0].id, duplicate_id);
    }
}
