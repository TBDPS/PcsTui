pub mod download;
pub mod queue;
pub mod types;
pub mod upload;

pub use download::{
    DownloadPlan, PlannedFile, download_one, download_plan, md5_file, plan_download, unique_path,
};
pub use queue::{QueueStore, QueuedTask, QueuedTaskKind};
pub use types::{
    Direction, DownloadOptions, ProgressFn, ProgressReporter, TransferEvent, TransferProgress,
    TransferStatus, UploadOptions,
};
pub use upload::{
    FileHashes, UploadRequest, UploadState, ensure_remote_dir, hash_file, plan_upload, upload_one,
};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::api::BaiduClient;
use crate::api::models::{RemoteItem, format_bytes, sanitize_component};
use crate::config::RuntimeConfig;

/// Owns the transfer queue, cancellation tokens and the event channel. All
/// network work happens in independent Tokio tasks; the TUI never blocks on a
/// transfer.
pub struct TransferEngine {
    client: BaiduClient,
    config: RuntimeConfig,
    tx: mpsc::UnboundedSender<TransferEvent>,
    cancels: Arc<Mutex<HashMap<u64, CancellationToken>>>,
    download_sem: Arc<Semaphore>,
    upload_sem: Arc<Semaphore>,
    next_id: AtomicU64,
}

impl TransferEngine {
    pub fn new(
        client: BaiduClient,
        config: RuntimeConfig,
    ) -> (Self, mpsc::UnboundedReceiver<TransferEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let download_sem = Arc::new(Semaphore::new(config.file.transfer.max_active_downloads));
        let upload_sem = Arc::new(Semaphore::new(config.file.transfer.upload_connections));
        (
            Self {
                client,
                config,
                tx,
                cancels: Arc::new(Mutex::new(HashMap::new())),
                download_sem,
                upload_sem,
                next_id: AtomicU64::new(1),
            },
            rx,
        )
    }

    pub fn spawn_download(
        &self,
        item: RemoteItem,
        cwd: String,
        download_root: PathBuf,
        options: DownloadOptions,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        self.register(id, cancel.clone());
        let name = if item.name.is_empty() {
            item.path
                .rsplit('/')
                .next()
                .unwrap_or("download")
                .to_string()
        } else {
            item.name.clone()
        };
        let base = TransferProgress {
            id,
            direction: Direction::Download,
            name: name.clone(),
            remote_path: item.absolute_path(&cwd),
            local_path: download_root.join(sanitize_component(&name)),
            total: item.size,
            done: 0,
            status: TransferStatus::Queued,
            speed_bps: 0.0,
            message: "排队".to_string(),
            files_done: 0,
            files_total: if item.is_dir { 0 } else { 1 },
        };
        let _ = self.tx.send(TransferEvent::Added(base.clone()));
        let client = self.client.clone();
        let tx = self.tx.clone();
        let cancels = self.cancels.clone();
        let semaphore = self.download_sem.clone();
        tokio::spawn(async move {
            let callback: ProgressFn = {
                let tx = tx.clone();
                Arc::new(move |progress| {
                    let _ = tx.send(TransferEvent::Updated(progress));
                })
            };
            let mut reporter = ProgressReporter::new(base, callback);
            let permit = tokio::select! {
                _ = cancel.cancelled() => None,
                permit = semaphore.acquire_owned() => permit.ok(),
            };
            let Some(_permit) = permit else {
                finish_job(reporter, Err(anyhow::anyhow!("下载已取消")), &tx, id);
                if let Ok(mut guard) = cancels.lock() {
                    guard.remove(&id);
                }
                return;
            };
            let result = async {
                reporter.set_status(TransferStatus::Running, "正在规划下载");
                let plan = plan_download(&client, &item, &cwd, &download_root).await?;
                if plan.is_empty() {
                    reporter.set_totals(0, 0);
                    return Ok::<(), anyhow::Error>(());
                }
                download_plan(&client, &plan, &options, &cancel, &mut reporter).await?;
                Ok(())
            }
            .await;
            finish_job(reporter, result, &tx, id);
            if let Ok(mut guard) = cancels.lock() {
                guard.remove(&id);
            }
        });
        id
    }

    pub fn spawn_upload(&self, local: PathBuf, remote_dir: String, options: UploadOptions) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        self.register(id, cancel.clone());
        let name = local
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| "upload".to_string());
        let base = TransferProgress {
            id,
            direction: Direction::Upload,
            name,
            remote_path: remote_dir.clone(),
            local_path: local.clone(),
            total: std::fs::metadata(&local)
                .map(|meta| meta.len())
                .unwrap_or(0),
            done: 0,
            status: TransferStatus::Queued,
            speed_bps: 0.0,
            message: "排队".to_string(),
            files_done: 0,
            files_total: 1,
        };
        let _ = self.tx.send(TransferEvent::Added(base.clone()));
        let client = self.client.clone();
        let tx = self.tx.clone();
        let cancels = self.cancels.clone();
        let semaphore = self.upload_sem.clone();
        let state_dir = self.config.paths.upload_state_dir.clone();
        tokio::spawn(async move {
            let callback: ProgressFn = {
                let tx = tx.clone();
                Arc::new(move |progress| {
                    let _ = tx.send(TransferEvent::Updated(progress));
                })
            };
            let mut reporter = ProgressReporter::new(base, callback);
            let permit = tokio::select! {
                _ = cancel.cancelled() => None,
                permit = semaphore.acquire_owned() => permit.ok(),
            };
            let Some(_permit) = permit else {
                finish_job(reporter, Err(anyhow::anyhow!("上传已取消")), &tx, id);
                if let Ok(mut guard) = cancels.lock() {
                    guard.remove(&id);
                }
                return;
            };
            let result = async {
                let plan = plan_upload(&local, &remote_dir)?;
                let files = plan
                    .iter()
                    .filter(|(path, _, _)| !path.as_os_str().is_empty())
                    .count();
                let total_bytes: u64 = plan
                    .iter()
                    .filter(|(path, _, _)| !path.as_os_str().is_empty())
                    .filter_map(|(path, _, _)| std::fs::metadata(path).ok())
                    .map(|meta| meta.len())
                    .sum();
                reporter.set_totals(total_bytes, files);
                let mut base_done = 0_u64;
                let mut file_index = 0usize;
                for (path, dir, _) in &plan {
                    if cancel.is_cancelled() {
                        return Err(anyhow::anyhow!("上传已取消"));
                    }
                    if path.as_os_str().is_empty() {
                        ensure_remote_dir(&client, dir).await?;
                        continue;
                    }
                    let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
                    reporter.begin_file(base_done, file_index);
                    upload_one(
                        &client,
                        UploadRequest {
                            local: path,
                            remote_dir: dir,
                            state_dir: &state_dir,
                        },
                        &options,
                        &cancel,
                        &mut reporter,
                    )
                    .await?;
                    base_done = base_done.saturating_add(size);
                    file_index += 1;
                    reporter.update_files(
                        base_done,
                        file_index,
                        format!(
                            "已完成 {}/{}（{}）",
                            file_index,
                            files,
                            format_bytes(base_done)
                        ),
                    );
                }
                Ok(())
            }
            .await;
            finish_job(reporter, result, &tx, id);
            if let Ok(mut guard) = cancels.lock() {
                guard.remove(&id);
            }
        });
        id
    }

    pub fn cancel(&self, id: u64) -> bool {
        if let Ok(guard) = self.cancels.lock() {
            if let Some(token) = guard.get(&id) {
                token.cancel();
                return true;
            }
        }
        false
    }

    pub fn cancel_all(&self) {
        if let Ok(guard) = self.cancels.lock() {
            for token in guard.values() {
                token.cancel();
            }
        }
    }

    pub fn active_count(&self) -> usize {
        self.cancels.lock().map(|guard| guard.len()).unwrap_or(0)
    }

    fn register(&self, id: u64, cancel: CancellationToken) {
        if let Ok(mut guard) = self.cancels.lock() {
            guard.insert(id, cancel);
        }
    }
}

fn finish_job(
    reporter: ProgressReporter,
    result: Result<(), anyhow::Error>,
    tx: &mpsc::UnboundedSender<TransferEvent>,
    id: u64,
) {
    match result {
        Ok(()) => {
            let progress = reporter.snapshot().clone();
            reporter.finish(TransferStatus::Completed, "完成");
            let mut finished = progress;
            finished.status = TransferStatus::Completed;
            finished.message = "完成".to_string();
            finished.done = finished.total.max(finished.done);
            finished.files_done = finished.files_total.max(finished.files_done);
            let _ = tx.send(TransferEvent::Finished(finished));
        }
        Err(err) => {
            let text = err.to_string();
            let status = if text.contains("取消") {
                TransferStatus::Cancelled
            } else {
                TransferStatus::Failed
            };
            let mut progress = reporter.snapshot().clone();
            reporter.finish(status, text.clone());
            progress.status = status;
            progress.message = text;
            let _ = tx.send(TransferEvent::Finished(progress));
        }
    }
    let _ = id;
}
