use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Download,
    Upload,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Direction::Download => "下载",
            Direction::Upload => "上传",
        }
    }

    pub fn arrow(self) -> &'static str {
        match self {
            Direction::Download => "↓",
            Direction::Upload => "↑",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferStatus {
    Queued,
    Running,
    Hashing,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

impl TransferStatus {
    pub fn label(self) -> &'static str {
        match self {
            TransferStatus::Queued => "排队",
            TransferStatus::Running => "传输中",
            TransferStatus::Hashing => "计算校验",
            TransferStatus::Verifying => "校验中",
            TransferStatus::Completed => "完成",
            TransferStatus::Failed => "失败",
            TransferStatus::Cancelled => "已取消",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TransferStatus::Completed | TransferStatus::Failed | TransferStatus::Cancelled
        )
    }
}

#[derive(Debug, Clone)]
pub struct TransferProgress {
    pub id: u64,
    pub direction: Direction,
    pub name: String,
    pub remote_path: String,
    pub local_path: PathBuf,
    pub total: u64,
    pub done: u64,
    pub status: TransferStatus,
    pub speed_bps: f64,
    pub message: String,
    pub files_done: usize,
    pub files_total: usize,
}

impl TransferProgress {
    pub fn ratio(&self) -> f64 {
        if self.total == 0 {
            if self.status == TransferStatus::Completed {
                1.0
            } else {
                0.0
            }
        } else {
            (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
        }
    }

    pub fn percent(&self) -> u16 {
        (self.ratio() * 100.0).round() as u16
    }
}

#[derive(Debug, Clone)]
pub enum TransferEvent {
    Added(TransferProgress),
    Updated(TransferProgress),
    Finished(TransferProgress),
}

pub type ProgressFn = Arc<dyn Fn(TransferProgress) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadOptions {
    pub overwrite: bool,
    pub rename_existing: bool,
    pub verify_md5: bool,
    /// Range connections for one large file. 1 disables multi-connection.
    #[serde(default = "default_connections")]
    pub connections: usize,
    #[serde(default = "default_retries")]
    pub retries: usize,
}

fn default_connections() -> usize {
    4
}

fn default_retries() -> usize {
    3
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            overwrite: false,
            rename_existing: false,
            verify_md5: true,
            connections: default_connections(),
            retries: default_retries(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadOptions {
    pub chunk_mib: u64,
    pub retries: usize,
}

impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            chunk_mib: 0,
            retries: 3,
        }
    }
}

/// Emits at most five progress updates per second while keeping the final
/// update exact. This keeps the TUI responsive even on very fast links.
pub struct ProgressReporter {
    base: TransferProgress,
    callback: ProgressFn,
    last_emit: Instant,
    /// Bytes in files that were completed before the current file.
    base_done: u64,
    /// Current file's raw byte count, used for accurate speed calculation.
    raw_done: u64,
    last_raw: u64,
    speed_bps: f64,
}

impl ProgressReporter {
    pub fn new(base: TransferProgress, callback: ProgressFn) -> Self {
        let mut reporter = Self {
            base,
            callback,
            last_emit: Instant::now(),
            base_done: 0,
            raw_done: 0,
            last_raw: 0,
            speed_bps: 0.0,
        };
        reporter.emit(true);
        reporter
    }

    pub fn set_status(&mut self, status: TransferStatus, message: impl Into<String>) {
        self.base.status = status;
        self.base.message = message.into();
        self.emit(true);
    }

    pub fn set_totals(&mut self, total: u64, files_total: usize) {
        self.base.total = total;
        self.base.files_total = files_total;
        self.emit(true);
    }

    /// Start a new file inside a multi-file transfer. `base_done` is the
    /// aggregate byte count of all previously completed files.
    pub fn begin_file(&mut self, base_done: u64, files_done: usize) {
        self.base_done = base_done;
        self.raw_done = 0;
        self.last_raw = 0;
        self.base.done = base_done;
        self.base.files_done = files_done;
        self.emit(true);
    }

    /// Keep the aggregate total when a single file does not know its size.
    pub fn ensure_total(&mut self, total: u64, files_total: usize) {
        if self.base.total == 0 && total > 0 {
            self.base.total = total;
        }
        if self.base.files_total == 0 && files_total > 0 {
            self.base.files_total = files_total;
        }
        self.emit(true);
    }

    pub fn update(&mut self, done: u64, message: impl Into<String>) {
        self.raw_done = done;
        self.base.done = self.base_done.saturating_add(done);
        self.base.message = message.into();
        self.base.status = TransferStatus::Running;
        self.emit(false);
    }

    pub fn update_files(&mut self, done: u64, files_done: usize, message: impl Into<String>) {
        self.base.done = done;
        self.base.files_done = files_done;
        self.base.message = message.into();
        self.base.status = TransferStatus::Running;
        self.emit(true);
    }

    pub fn finish(mut self, status: TransferStatus, message: impl Into<String>) {
        self.base.status = status;
        self.base.message = message.into();
        if status == TransferStatus::Completed {
            self.base.done = self.base.total.max(self.base.done);
            if self.base.files_total > 0 {
                self.base.files_done = self.base.files_total;
            }
        }
        self.base.speed_bps = 0.0;
        (self.callback)(self.base);
    }

    pub fn snapshot(&self) -> &TransferProgress {
        &self.base
    }

    fn emit(&mut self, force: bool) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_emit).as_secs_f64();
        if force || elapsed >= 0.2 {
            let delta = self.raw_done.saturating_sub(self.last_raw);
            if elapsed > 0.0 && delta > 0 {
                let instant = delta as f64 / elapsed;
                self.speed_bps = if self.speed_bps <= 0.0 {
                    instant
                } else {
                    self.speed_bps * 0.7 + instant * 0.3
                };
            }
            self.base.speed_bps = self.speed_bps;
            self.last_emit = now;
            self.last_raw = self.raw_done;
            (self.callback)(self.base.clone());
        }
    }
}
