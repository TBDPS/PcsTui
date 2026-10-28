use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::{StreamExt, TryStreamExt};
use md5::{Digest, Md5};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::api::models::{
    RemoteItem, format_bytes, remote_join, safe_local_path, sanitize_component,
};
use crate::api::{ApiError, BaiduClient};
use crate::transfer::types::{DownloadOptions, ProgressReporter, TransferStatus};

#[derive(Debug, Clone)]
pub struct PlannedFile {
    pub item: RemoteItem,
    pub local: PathBuf,
    pub relative: String,
}

#[derive(Debug, Default)]
pub struct DownloadPlan {
    pub directories: Vec<PathBuf>,
    pub files: Vec<PlannedFile>,
    pub total_bytes: u64,
}

impl DownloadPlan {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.directories.is_empty()
    }
}

/// Build a recursive plan without downloading file contents. Empty remote
/// directories are preserved locally, which is a small but frequently missed
/// detail in index-style clients.
pub async fn plan_download(
    client: &BaiduClient,
    item: &RemoteItem,
    cwd: &str,
    base: &Path,
) -> Result<DownloadPlan> {
    let mut plan = DownloadPlan::default();
    let root_name = sanitize_component(if item.name.is_empty() {
        "download"
    } else {
        &item.name
    });
    let root_local = base.join(root_name);
    if !item.is_dir {
        plan.files.push(PlannedFile {
            item: item.clone(),
            local: root_local,
            relative: item.name.clone(),
        });
        plan.total_bytes = item.size;
        return Ok(plan);
    }

    plan.directories.push(root_local.clone());
    let remote_root = item.absolute_path(cwd);
    let mut queue: VecDeque<(String, PathBuf, String)> = VecDeque::new();
    queue.push_back((remote_root, root_local, String::new()));
    let mut seen_dirs = std::collections::HashSet::new();
    while let Some((remote_dir, local_dir, relative)) = queue.pop_front() {
        if !seen_dirs.insert(remote_dir.clone()) {
            continue;
        }
        let entries = client
            .list_dir(&remote_dir)
            .await
            .with_context(|| format!("列出远程目录失败: {remote_dir}"))?;
        for entry in entries {
            let name = sanitize_component(if entry.name.is_empty() {
                entry.path.rsplit('/').next().unwrap_or("unnamed")
            } else {
                &entry.name
            });
            let child_local = local_dir.join(&name);
            let child_relative = if relative.is_empty() {
                name.clone()
            } else {
                format!("{relative}/{name}")
            };
            if entry.is_dir {
                plan.directories.push(child_local.clone());
                let child_remote = entry.absolute_path(&remote_dir);
                queue.push_back((child_remote, child_local, child_relative));
            } else {
                plan.total_bytes = plan.total_bytes.saturating_add(entry.size);
                plan.files.push(PlannedFile {
                    item: entry,
                    local: child_local,
                    relative: child_relative,
                });
            }
        }
    }
    Ok(plan)
}

const MULTI_THRESHOLD: u64 = 64 * 1024 * 1024;
// Baidu rejects closed ranges larger than ~5 MiB for large files.
const RANGE_CHUNK: u64 = 4 * 1024 * 1024;

const RANGE_STATE_VERSION: u32 = 2;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RangeState {
    version: u32,
    total: u64,
    chunk_size: u64,
    completed: Vec<bool>,
    /// MD5 of each completed chunk. Used to re-validate a `.part` whose
    /// fingerprint changed (for example after a crash or external truncation).
    #[serde(default)]
    chunk_md5: Vec<Option<String>>,
    /// Size and mtime of the `.part` when the state was last saved. A matching
    /// fingerprint lets resume skip re-reading every completed chunk.
    #[serde(default)]
    part_len: u64,
    #[serde(default)]
    part_mtime_ns: u64,
}

impl RangeState {
    fn new(total: u64) -> Self {
        let chunks = total.div_ceil(RANGE_CHUNK) as usize;
        Self {
            version: RANGE_STATE_VERSION,
            total,
            chunk_size: RANGE_CHUNK,
            completed: vec![false; chunks],
            chunk_md5: vec![None; chunks],
            part_len: 0,
            part_mtime_ns: 0,
        }
    }

    fn valid_for(&self, total: u64) -> bool {
        self.version == RANGE_STATE_VERSION
            && self.total == total
            && self.chunk_size == RANGE_CHUNK
            && self.completed.len() == total.div_ceil(RANGE_CHUNK) as usize
            && self.chunk_md5.len() == self.completed.len()
    }

    fn completed_bytes(&self) -> u64 {
        self.completed
            .iter()
            .enumerate()
            .filter(|(_, done)| **done)
            .map(|(index, _)| {
                let start = index as u64 * self.chunk_size;
                (self.total - start).min(self.chunk_size)
            })
            .sum()
    }

    fn all_done(&self) -> bool {
        self.completed.iter().all(|done| *done)
    }
}

fn mtime_ns(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

async fn part_fingerprint(path: &Path) -> Option<(u64, u64)> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    Some((metadata.len(), mtime_ns(&metadata)))
}

async fn md5_range(file: &mut tokio::fs::File, start: u64, length: u64) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return None;
    }
    let mut hasher = Md5::new();
    let mut remaining = length;
    let mut buffer = vec![0_u8; 1024 * 1024];
    while remaining > 0 {
        let take = (buffer.len() as u64).min(remaining) as usize;
        match file.read(&mut buffer[..take]).await {
            Ok(0) => return None,
            Ok(read) => {
                hasher.update(&buffer[..read]);
                remaining -= read as u64;
            }
            Err(_) => return None,
        }
    }
    Some(hex::encode(hasher.finalize()))
}

/// A state file is only trustworthy if the `.part` still has the expected
/// size. If the fingerprint changed, completed chunks are re-hashed; corrupt
/// chunks are marked incomplete instead of being silently kept.
async fn validate_range_state(part: &Path, state: &mut RangeState) -> bool {
    let Some((part_len, part_mtime_ns)) = part_fingerprint(part).await else {
        return false;
    };
    if part_len != state.total {
        return false;
    }
    if state.part_len == part_len
        && state.part_mtime_ns != 0
        && state.part_mtime_ns == part_mtime_ns
    {
        return true;
    }
    let Ok(mut file) = tokio::fs::File::open(part).await else {
        return false;
    };
    for index in 0..state.completed.len() {
        if !state.completed[index] {
            continue;
        }
        let start = index as u64 * state.chunk_size;
        let length = (state.total - start).min(state.chunk_size);
        let actual = md5_range(&mut file, start, length).await;
        if state.chunk_md5[index].as_deref() != actual.as_deref() {
            state.completed[index] = false;
            state.chunk_md5[index] = None;
        }
    }
    state.part_len = part_len;
    state.part_mtime_ns = part_mtime_ns;
    true
}

#[derive(Debug, thiserror::Error)]
enum MultiFailure {
    #[error("服务器不支持 Range 多连接")]
    Unsupported,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

enum MultiOutcome {
    Done,
    Unsupported,
}

fn range_state_path(part: &Path) -> PathBuf {
    PathBuf::from(format!("{}.state.json", part.display()))
}

async fn load_range_state(path: &Path, total: u64) -> Option<RangeState> {
    let raw = tokio::fs::read(path).await.ok()?;
    let state: RangeState = serde_json::from_slice(&raw).ok()?;
    state.valid_for(total).then_some(state)
}

async fn save_range_state(path: &Path, state: &RangeState) -> Result<()> {
    let data = serde_json::to_vec(state)?;
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    {
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("写入下载断点状态失败: {}", tmp.display()))?;
        file.write_all(&data)
            .await
            .with_context(|| format!("写入下载断点状态失败: {}", tmp.display()))?;
        file.sync_all()
            .await
            .with_context(|| format!("同步下载断点状态失败: {}", tmp.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await;
    }
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("替换下载断点状态失败: {}", path.display()))?;
    Ok(())
}

async fn remove_range_state(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

async fn preflight_range(client: &BaiduClient, url: &url::Url) -> bool {
    match client.download_get_range(url, 0, 0).await {
        Ok(response) => response.status() == reqwest::StatusCode::PARTIAL_CONTENT,
        Err(_) => false,
    }
}

/// Download a single file with HTTP range resume, optional multi-connection
/// ranges, retries and MD5 checking.
pub async fn download_one(
    client: &BaiduClient,
    item: &RemoteItem,
    target: &Path,
    options: &DownloadOptions,
    cancel: &CancellationToken,
    mut reporter: Option<&mut ProgressReporter>,
) -> Result<u64> {
    if item.is_dir {
        bail!("download_one 不接受目录: {}", item.path);
    }
    let target = resolve_target(target, item.size, options)?;
    if !options.overwrite
        && item.size > 0
        && target
            .metadata()
            .map(|meta| meta.is_file() && meta.len() == item.size)
            .unwrap_or(false)
    {
        if options.verify_md5 && is_md5(&item.md5) {
            let expected = item.md5.to_ascii_lowercase();
            let path = target.clone();
            let actual = tokio::task::spawn_blocking(move || md5_file(&path))
                .await
                .context("本地文件 MD5 校验任务失败")??;
            if actual != expected {
                // Directory metadata may be the block-list MD5 for files
                // assembled from uploaded parts. Ask the download host for
                // the authoritative content MD5 before rejecting the file.
                let accepted = match client.content_md5(&item.path).await {
                    Ok(Some(server)) => server == actual,
                    _ => {
                        let path = target.clone();
                        let candidates =
                            tokio::task::spawn_blocking(move || block_list_md5_candidates(&path))
                                .await
                                .context("本地分片列表 MD5 校验任务失败")??;
                        candidates.iter().any(|value| value == &expected)
                    }
                };
                if !accepted {
                    bail!(
                        "本地同名文件 MD5 不一致: {}（使用 --overwrite 或 --rename）",
                        target.display()
                    );
                }
            }
        }
        if let Some(reporter) = reporter.as_deref_mut() {
            reporter.update(item.size, "本地已存在且校验一致，跳过");
        }
        return Ok(item.size);
    }
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("创建本地目录失败: {}", parent.display()))?;
    }
    let part = part_path(&target, item);
    let state_path = range_state_path(&part);
    let had_state = tokio::fs::try_exists(&state_path).await.unwrap_or(false);
    let mut state = if had_state {
        match load_range_state(&state_path, item.size).await {
            Some(mut current) => {
                if validate_range_state(&part, &mut current).await {
                    // Persist any chunks invalidated by re-hashing, together
                    // with the refreshed part fingerprint.
                    save_range_state(&state_path, &current).await?;
                    Some(current)
                } else {
                    None
                }
            }
            None => None,
        }
    } else {
        None
    };
    if had_state && state.is_none() {
        // Corrupt, truncated, or missing `.part`: never trust a possibly
        // sparse partial file, start from a clean slate.
        let _ = tokio::fs::remove_file(&part).await;
        remove_range_state(&state_path).await;
    }

    let mut part_len = tokio::fs::metadata(&part)
        .await
        .map(|meta| meta.len())
        .unwrap_or(0);
    if item.size > 0 && part_len > item.size {
        let _ = tokio::fs::remove_file(&part).await;
        remove_range_state(&state_path).await;
        part_len = 0;
        state = None;
    }

    let mut multi_blocked = false;
    if let Some(current) = state.take() {
        if current.all_done() && part_len == item.size {
            remove_range_state(&state_path).await;
            finalize_download(
                client,
                &part,
                &target,
                item,
                options,
                reporter.as_deref_mut(),
            )
            .await?;
            return Ok(item.size);
        }
        if current.all_done() {
            let _ = tokio::fs::remove_file(&part).await;
            remove_range_state(&state_path).await;
            state = None;
            part_len = 0;
        } else {
            if let Some(reporter) = reporter.as_deref_mut() {
                reporter.ensure_total(item.size, 1);
                reporter.set_status(TransferStatus::Running, "正在恢复多连接断点");
            }
            let url = client.download_link(&item.path).await?;
            match download_multi(
                client,
                item,
                &url,
                &part,
                &state_path,
                current,
                options,
                cancel,
                reporter.as_deref_mut(),
            )
            .await?
            {
                MultiOutcome::Done => {
                    remove_range_state(&state_path).await;
                    finalize_download(
                        client,
                        &part,
                        &target,
                        item,
                        options,
                        reporter.as_deref_mut(),
                    )
                    .await?;
                    return Ok(item.size);
                }
                MultiOutcome::Unsupported => {
                    multi_blocked = true;
                    let _ = tokio::fs::remove_file(&part).await;
                    remove_range_state(&state_path).await;
                    part_len = 0;
                }
            }
        }
    }

    if !multi_blocked
        && state.is_none()
        && item.size >= MULTI_THRESHOLD
        && options.connections > 1
        && part_len == 0
    {
        let url = client.download_link(&item.path).await?;
        if preflight_range(client, &url).await {
            let current = RangeState::new(item.size);
            save_range_state(&state_path, &current).await?;
            if let Some(reporter) = reporter.as_deref_mut() {
                reporter.ensure_total(item.size, 1);
                reporter.set_status(
                    TransferStatus::Running,
                    format!("多连接下载（{} 连接）", options.connections),
                );
            }
            match download_multi(
                client,
                item,
                &url,
                &part,
                &state_path,
                current,
                options,
                cancel,
                reporter.as_deref_mut(),
            )
            .await?
            {
                MultiOutcome::Done => {
                    remove_range_state(&state_path).await;
                    finalize_download(
                        client,
                        &part,
                        &target,
                        item,
                        options,
                        reporter.as_deref_mut(),
                    )
                    .await?;
                    return Ok(item.size);
                }
                MultiOutcome::Unsupported => {
                    let _ = tokio::fs::remove_file(&part).await;
                    remove_range_state(&state_path).await;
                }
            }
        }
    }

    download_single(
        client,
        item,
        &part,
        &target,
        options,
        cancel,
        reporter.as_deref_mut(),
    )
    .await
}

async fn download_multi(
    client: &BaiduClient,
    item: &RemoteItem,
    url: &url::Url,
    part: &Path,
    state_path: &Path,
    state: RangeState,
    options: &DownloadOptions,
    cancel: &CancellationToken,
    reporter: Option<&mut ProgressReporter>,
) -> Result<MultiOutcome> {
    let total = item.size;
    let state = std::sync::Arc::new(tokio::sync::Mutex::new(state));
    let initial_done = state.lock().await.completed_bytes();
    let progress = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(initial_done));
    let reported = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(initial_done));
    let reporter = reporter.map(|reporter| std::sync::Arc::new(tokio::sync::Mutex::new(reporter)));
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(part)
        .await
        .with_context(|| format!("打开临时文件失败: {}", part.display()))?;
    file.set_len(total)
        .await
        .with_context(|| format!("预分配临时文件失败: {}", part.display()))?;
    drop(file);

    let pending: Vec<usize> = {
        let guard = state.lock().await;
        guard
            .completed
            .iter()
            .enumerate()
            .filter(|(_, done)| !**done)
            .map(|(index, _)| index)
            .collect()
    };
    let workers = options.connections.max(1).min(pending.len().max(1));
    let remote_path = item.path.clone();
    let client = client.clone();
    let url = url.clone();
    let part = part.to_path_buf();
    let state_path = state_path.to_path_buf();
    let cancel = cancel.clone();
    let retries = options.retries;

    let stream = futures::stream::iter(pending)
        .map(|index| {
            let client = client.clone();
            let url = url.clone();
            let part = part.clone();
            let state_path = state_path.clone();
            let state = state.clone();
            let progress = progress.clone();
            let reported = reported.clone();
            let reporter = reporter.clone();
            let cancel = cancel.clone();
            let remote_path = remote_path.clone();
            async move {
                let start = index as u64 * RANGE_CHUNK;
                let length = (total - start).min(RANGE_CHUNK);
                let end = start + length - 1;
                for attempt in 0..=retries {
                    if cancel.is_cancelled() {
                        return Err(MultiFailure::Other(anyhow!("下载已取消")));
                    }
                    let attempt_url = if attempt == 0 {
                        url.clone()
                    } else {
                        client
                            .download_link(&remote_path)
                            .await
                            .map_err(|err| MultiFailure::Other(anyhow!(err)))?
                    };
                    let response = match client.download_get_range(&attempt_url, start, end).await {
                        Ok(response) => response,
                        Err(err) => {
                            if attempt >= retries {
                                return Err(MultiFailure::Other(anyhow!(
                                    "下载分片 {} 请求失败: {err}",
                                    index + 1
                                )));
                            }
                            wait_for_retry(attempt).await;
                            continue;
                        }
                    };
                    let status = response.status();
                    // Some accounts/sessions are not allowed to use multiple
                    // Range requests even though the signed URL accepts a
                    // probe. Treat that as "fall back to a single stream"
                    // instead of failing the whole transfer.
                    if status == reqwest::StatusCode::OK
                        || status == reqwest::StatusCode::FORBIDDEN
                        || status == reqwest::StatusCode::UNAUTHORIZED
                    {
                        return Err(MultiFailure::Unsupported);
                    }
                    if status != reqwest::StatusCode::PARTIAL_CONTENT {
                        let body = response.text().await.unwrap_or_default();
                        if attempt >= retries || !is_retryable_status(status) {
                            return Err(MultiFailure::Other(anyhow!(
                                "下载分片 {} 失败（HTTP {}）: {}",
                                index + 1,
                                status.as_u16(),
                                truncate(&body, 160)
                            )));
                        }
                        wait_for_retry(attempt).await;
                        continue;
                    }
                    let mut file = match tokio::fs::OpenOptions::new().write(true).open(&part).await
                    {
                        Ok(file) => file,
                        Err(err) => {
                            return Err(MultiFailure::Other(anyhow!(
                                "打开临时文件失败: {}: {err}",
                                part.display()
                            )));
                        }
                    };
                    if let Err(err) =
                        tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(start))
                            .await
                    {
                        return Err(MultiFailure::Other(anyhow!(
                            "定位临时文件失败: {}: {err}",
                            part.display()
                        )));
                    }
                    let mut stream = response.bytes_stream();
                    let mut got = 0_u64;
                    let mut chunk_hasher = Md5::new();
                    let mut stream_error = None;
                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                let _ = file.flush().await;
                                return Err(MultiFailure::Other(anyhow!("下载已取消")));
                            }
                            chunk = stream.next() => {
                                match chunk {
                                    Some(Ok(chunk)) => {
                                        if let Err(err) = file.write_all(&chunk).await {
                                            return Err(MultiFailure::Other(anyhow!(
                                                "写入本地文件失败: {}: {err}",
                                                part.display()
                                            )));
                                        }
                                        chunk_hasher.update(&chunk);
                                        got = got.saturating_add(chunk.len() as u64);
                                    }
                                    Some(Err(err)) => {
                                        stream_error = Some(err.to_string());
                                        break;
                                    }
                                    None => break,
                                }
                            }
                        }
                    }
                    if stream_error.is_some() || got != length {
                        if attempt >= retries {
                            return Err(MultiFailure::Other(anyhow!(
                                "下载分片 {} 不完整（{}/{}）{}",
                                index + 1,
                                got,
                                length,
                                stream_error
                                    .map(|err| format!(": {err}"))
                                    .unwrap_or_default()
                            )));
                        }
                        wait_for_retry(attempt).await;
                        continue;
                    }
                    if let Err(err) = file.flush().await {
                        return Err(MultiFailure::Other(anyhow!(
                            "刷新临时文件失败: {}: {err}",
                            part.display()
                        )));
                    }
                    // Make the bytes durable before the state file claims
                    // this chunk is complete.
                    if let Err(err) = file.sync_data().await {
                        return Err(MultiFailure::Other(anyhow!(
                            "同步临时文件失败: {}: {err}",
                            part.display()
                        )));
                    }
                    drop(file);
                    let fingerprint = part_fingerprint(&part).await;
                    let chunk_md5 = hex::encode(chunk_hasher.finalize());
                    {
                        let mut guard = state.lock().await;
                        guard.completed[index] = true;
                        guard.chunk_md5[index] = Some(chunk_md5);
                        if let Some((len, mtime)) = fingerprint {
                            guard.part_len = len;
                            guard.part_mtime_ns = mtime;
                        }
                        if let Err(err) = save_range_state(&state_path, &guard).await {
                            return Err(MultiFailure::Other(err));
                        }
                    }
                    let done =
                        progress.fetch_add(length, std::sync::atomic::Ordering::Relaxed) + length;
                    let previous = reported.fetch_max(done, std::sync::atomic::Ordering::Relaxed);
                    if done > previous {
                        if let Some(reporter) = &reporter {
                            let mut reporter = reporter.lock().await;
                            let chunks = state.lock().await.completed.len();
                            reporter
                                .update(done, format!("多连接下载分片 {}/{}", index + 1, chunks));
                        }
                    }
                    return Ok(length);
                }
                Err(MultiFailure::Other(anyhow!(
                    "下载分片 {} 重试次数耗尽",
                    index + 1
                )))
            }
        })
        .buffer_unordered(workers);

    let result = stream
        .try_collect::<Vec<u64>>()
        .await
        .map(|_| MultiOutcome::Done)
        .or_else(|err| match err {
            MultiFailure::Unsupported => Ok(MultiOutcome::Unsupported),
            MultiFailure::Other(err) => Err(err),
        });
    result
}

async fn download_single(
    client: &BaiduClient,
    item: &RemoteItem,
    part: &Path,
    target: &Path,
    options: &DownloadOptions,
    cancel: &CancellationToken,
    mut reporter: Option<&mut ProgressReporter>,
) -> Result<u64> {
    let max_failures = options.retries.saturating_add(1);
    let mut failures = 0usize;
    loop {
        if cancel.is_cancelled() {
            bail!("下载已取消");
        }
        let mut offset = tokio::fs::metadata(part)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        if item.size > 0 && offset > item.size {
            let _ = tokio::fs::remove_file(part).await;
            offset = 0;
        }
        if item.size > 0 && offset == item.size {
            finalize_download(client, part, target, item, options, reporter.as_deref_mut()).await?;
            return Ok(item.size);
        }
        let url = match client.download_link(&item.path).await {
            Ok(url) => url,
            Err(err) => {
                failures += 1;
                if failures >= max_failures {
                    return Err(anyhow!("获取下载链接失败: {err}"));
                }
                if let Some(reporter) = reporter.as_deref_mut() {
                    reporter.set_status(
                        TransferStatus::Running,
                        format!("获取下载链接重试 {}/{}", failures, options.retries),
                    );
                }
                wait_for_retry(failures - 1).await;
                continue;
            }
        };

        // Large files cannot be fetched with an open-ended Range (or no
        // Range) on Baidu's signed hosts. Ask for a closed chunk at a time;
        // this also keeps byte-level resume simple.
        let range_end = (item.size > 0).then(|| (offset + RANGE_CHUNK - 1).min(item.size - 1));
        let response = if let Some(end) = range_end {
            match client.download_get_range(&url, offset, end).await {
                Ok(response) => response,
                Err(err) => {
                    failures += 1;
                    if failures >= max_failures {
                        return Err(anyhow!("下载请求失败: {err}"));
                    }
                    if let Some(reporter) = reporter.as_deref_mut() {
                        reporter.set_status(
                            TransferStatus::Running,
                            format!("下载请求重试 {}/{}", failures, options.retries),
                        );
                    }
                    wait_for_retry(failures - 1).await;
                    continue;
                }
            }
        } else {
            match client.download_get(&url, None).await {
                Ok(response) => response,
                Err(err) => {
                    failures += 1;
                    if failures >= max_failures {
                        return Err(anyhow!("下载请求失败: {err}"));
                    }
                    if let Some(reporter) = reporter.as_deref_mut() {
                        reporter.set_status(
                            TransferStatus::Running,
                            format!("下载请求重试 {}/{}", failures, options.retries),
                        );
                    }
                    wait_for_retry(failures - 1).await;
                    continue;
                }
            }
        };
        let status = response.status();
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE
            && item.size > 0
            && offset == item.size
        {
            finalize_download(client, part, target, item, options, reporter.as_deref_mut()).await?;
            return Ok(item.size);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            failures += 1;
            if failures < max_failures && is_retryable_status(status) {
                if let Some(reporter) = reporter.as_deref_mut() {
                    reporter.set_status(
                        TransferStatus::Running,
                        format!(
                            "HTTP {} 重试 {}/{}",
                            status.as_u16(),
                            failures,
                            options.retries
                        ),
                    );
                }
                wait_for_retry(failures - 1).await;
                continue;
            }
            bail!(
                "下载请求失败（HTTP {}）: {}",
                status.as_u16(),
                truncate(&body, 200)
            );
        }

        let content_length = response.content_length().unwrap_or(0);
        let (write_offset, truncate, expected_end, total) = if item.size == 0 {
            (0, true, 0, content_length)
        } else if status == reqwest::StatusCode::PARTIAL_CONTENT {
            let end = range_end.unwrap_or(item.size - 1);
            (offset, false, end + 1, item.size)
        } else {
            // The host ignored the Range header and sent the whole file.
            (0, true, item.size, item.size)
        };
        let (done, stream_error) = match stream_to_file(
            response,
            part,
            write_offset,
            truncate,
            total,
            cancel,
            reporter.as_deref_mut(),
        )
        .await
        {
            Ok(result) => result,
            Err(err) => return Err(err),
        };
        let incomplete = item.size > 0 && done != expected_end;
        if stream_error.is_some() || incomplete {
            failures += 1;
            if failures < max_failures {
                if let Some(reporter) = reporter.as_deref_mut() {
                    reporter.set_status(
                        TransferStatus::Running,
                        format!(
                            "下载中断，重试 {}/{}（已保存 {}）",
                            failures,
                            options.retries,
                            format_bytes(done)
                        ),
                    );
                }
                wait_for_retry(failures - 1).await;
                continue;
            }
            bail!(
                "下载不完整：已收到 {}，期望 {}（保留断点，可重试）{}",
                format_bytes(done),
                format_bytes(total),
                stream_error
                    .map(|err| format!(": {err}"))
                    .unwrap_or_default()
            );
        }
        failures = 0;
        if item.size == 0 || done >= item.size {
            finalize_download(client, part, target, item, options, reporter.as_deref_mut()).await?;
            return Ok(done.max(item.size));
        }
    }
}

async fn stream_to_file(
    response: reqwest::Response,
    part: &Path,
    write_offset: u64,
    truncate: bool,
    total: u64,
    cancel: &CancellationToken,
    mut reporter: Option<&mut ProgressReporter>,
) -> Result<(u64, Option<String>)> {
    if let Some(reporter) = reporter.as_deref_mut() {
        reporter.ensure_total(total, 1);
        reporter.update(write_offset, "正在下载");
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(truncate)
        .open(part)
        .await
        .with_context(|| format!("打开临时文件失败: {}", part.display()))?;
    if !truncate && write_offset > 0 {
        file.set_len(write_offset)
            .await
            .with_context(|| format!("截断临时文件失败: {}", part.display()))?;
        tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(write_offset))
            .await
            .with_context(|| format!("定位临时文件失败: {}", part.display()))?;
    }
    let mut done = write_offset;
    let mut stream = response.bytes_stream();
    let mut stream_error = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = file.flush().await;
                bail!("下载已取消");
            }
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(chunk)) => {
                        file.write_all(&chunk).await.with_context(|| {
                            format!("写入本地文件失败: {}", part.display())
                        })?;
                        done = done.saturating_add(chunk.len() as u64);
                        if let Some(reporter) = reporter.as_deref_mut() {
                            reporter.update(done, "正在下载");
                        }
                    }
                    Some(Err(err)) => {
                        stream_error = Some(err.to_string());
                        break;
                    }
                    None => break,
                }
            }
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok((done, stream_error))
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

async fn finalize_download(
    client: &BaiduClient,
    part: &Path,
    target: &Path,
    item: &RemoteItem,
    options: &DownloadOptions,
    mut reporter: Option<&mut ProgressReporter>,
) -> Result<()> {
    if options.verify_md5 && is_md5(&item.md5) {
        if let Some(reporter) = reporter.as_deref_mut() {
            reporter.set_status(TransferStatus::Verifying, "正在校验 MD5");
        }
        let expected = item.md5.to_ascii_lowercase();
        let path = part.to_path_buf();
        let actual = tokio::task::spawn_blocking(move || md5_file(&path))
            .await
            .context("MD5 校验任务失败")??;
        if actual != expected {
            // Baidu's directory listing reports the MD5 of the block-list
            // JSON for files assembled from uploaded parts. The download
            // host exposes the real content MD5 in a one-byte Range reply,
            // so prefer that value before declaring the file corrupt.
            let mut accepted = false;
            match client.content_md5(&item.path).await {
                Ok(Some(server)) => {
                    if server != actual {
                        let _ = tokio::fs::remove_file(part).await;
                        bail!(
                            "MD5 校验失败：服务器内容 MD5 {server}，实际 {actual}；已删除损坏的断点"
                        );
                    }
                    accepted = true;
                }
                _ => {
                    // Fallback for offline/legacy cases: recognise the known
                    // "MD5 of the compact block-list JSON" metadata shape.
                    let path = part.to_path_buf();
                    let candidates =
                        tokio::task::spawn_blocking(move || block_list_md5_candidates(&path))
                            .await
                            .context("分片列表 MD5 校验任务失败")??;
                    if candidates.iter().any(|value| value == &expected) {
                        accepted = true;
                    }
                }
            }
            if !accepted {
                let _ = tokio::fs::remove_file(part).await;
                bail!("MD5 校验失败：期望 {expected}，实际 {actual}；已删除损坏的断点");
            }
            if let Some(reporter) = reporter.as_deref_mut() {
                reporter.set_status(
                    TransferStatus::Verifying,
                    "已按服务器内容 MD5 校验通过（目录元数据为分片列表 MD5）",
                );
            }
        }
    }
    if let Some(reporter) = reporter.as_deref_mut() {
        reporter.set_status(TransferStatus::Verifying, "正在同步文件");
    }
    {
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(part)
            .await
            .with_context(|| format!("打开临时文件失败: {}", part.display()))?;
        file.sync_all()
            .await
            .with_context(|| format!("同步临时文件失败: {}", part.display()))?;
    }
    if tokio::fs::try_exists(target).await.unwrap_or(false) {
        if options.overwrite {
            tokio::fs::remove_file(target)
                .await
                .with_context(|| format!("删除旧文件失败: {}", target.display()))?;
        } else {
            bail!("目标文件已存在: {}", target.display());
        }
    }
    tokio::fs::rename(part, target)
        .await
        .with_context(|| format!("保存文件失败: {}", target.display()))?;
    if let Some(parent) = target.parent() {
        let parent = parent.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            std::fs::File::open(&parent).and_then(|dir| dir.sync_all())
        })
        .await;
    }
    if item.mtime > 0 {
        let path = target.to_path_buf();
        let mtime = item.mtime;
        let _ = tokio::task::spawn_blocking(move || {
            let seconds = filetime::FileTime::from_unix_time(mtime, 0);
            let _ = filetime::set_file_mtime(&path, seconds);
        })
        .await;
    }
    Ok(())
}

fn resolve_target(target: &Path, size: u64, options: &DownloadOptions) -> Result<PathBuf> {
    if !target.exists() {
        return Ok(target.to_path_buf());
    }
    if options.overwrite {
        return Ok(target.to_path_buf());
    }
    if options.rename_existing {
        return Ok(unique_path(target));
    }
    if size > 0
        && std::fs::metadata(target)
            .map(|meta| meta.len() == size)
            .unwrap_or(false)
    {
        return Ok(target.to_path_buf());
    }
    bail!("目标文件已存在: {}", target.display())
}

pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());
    let (stem, extension) = match name.rfind('.') {
        Some(index) if index > 0 => (&name[..index], &name[index..]),
        _ => (name.as_str(), ""),
    };
    for index in 1..=9999 {
        let candidate = parent.join(format!("{stem} ({index}){extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{stem}-{}", uuid::Uuid::new_v4()))
}

pub fn part_path(target: &Path, item: &RemoteItem) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());
    // The final target can be shared by different remote files with the same
    // name. Include the remote identity so their partial files never collide.
    let identity = if item.fs_id != 0 {
        format!("{:x}", item.fs_id)
    } else {
        let mut hasher = Md5::new();
        hasher.update(item.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(item.name.as_bytes());
        hex::encode(hasher.finalize())[..12].to_string()
    };
    parent.join(format!(".{name}.{identity}.pcstui.part"))
}

pub fn is_md5(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn md5_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("打开文件失败: {}", path.display()))?;
    let mut hasher = Md5::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// The metadata MD5 produced by Baidu's `/api/create` for a file assembled
/// from uploaded parts is `MD5(compact JSON of the block MD5 list)`. The
/// upload block size is 4 MiB below 8 GiB, 16 MiB below 32 GiB, and 64 MiB
/// otherwise; try the sizes that can apply to this file.
fn block_list_md5_candidates(path: &Path) -> Result<Vec<String>> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("读取文件大小失败: {}", path.display()))?
        .len();
    let sizes: &[u64] = if size >= 32 * 1024 * 1024 * 1024 {
        &[4, 16, 64]
    } else if size >= 8 * 1024 * 1024 * 1024 {
        &[4, 16]
    } else {
        &[4]
    };
    let mut candidates = Vec::with_capacity(sizes.len());
    for block_mib in sizes {
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("打开文件失败: {}", path.display()))?;
        let block_size = (*block_mib * 1024 * 1024) as usize;
        let mut buffer = vec![0_u8; block_size];
        let mut blocks = Vec::new();
        loop {
            let mut filled = 0usize;
            while filled < buffer.len() {
                let read = file.read(&mut buffer[filled..])?;
                if read == 0 {
                    break;
                }
                filled += read;
            }
            if filled == 0 {
                break;
            }
            blocks.push(hex::encode(Md5::digest(&buffer[..filled])));
        }
        let json = serde_json::to_string(&blocks)?;
        candidates.push(hex::encode(Md5::digest(json.as_bytes())));
    }
    Ok(candidates)
}

/// Download a whole plan while reporting one aggregate progress stream.
pub async fn download_plan(
    client: &BaiduClient,
    plan: &DownloadPlan,
    options: &DownloadOptions,
    cancel: &CancellationToken,
    reporter: &mut ProgressReporter,
) -> Result<()> {
    for dir in &plan.directories {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("创建本地目录失败: {}", dir.display()))?;
    }
    reporter.set_totals(plan.total_bytes, plan.files.len());
    let mut base_done = 0_u64;
    for (index, file) in plan.files.iter().enumerate() {
        if cancel.is_cancelled() {
            bail!("下载已取消");
        }
        reporter.begin_file(base_done, index);
        if let Err(err) = download_one(
            client,
            &file.item,
            &file.local,
            options,
            cancel,
            Some(reporter),
        )
        .await
        {
            return Err(anyhow!("{}: {err}", file.relative));
        }
        base_done = base_done.saturating_add(if file.item.size > 0 {
            file.item.size
        } else {
            file.local.metadata().map(|meta| meta.len()).unwrap_or(0)
        });
    }
    Ok(())
}

pub async fn wait_for_retry(attempt: usize) {
    let delay = Duration::from_millis(300 * (1_u64 << attempt.min(5)));
    tokio::time::sleep(delay).await;
}

pub fn describe_api_error(err: &ApiError) -> String {
    match err {
        ApiError::Remote {
            errno,
            message,
            context,
            ..
        } => format!("{context}失败（errno={errno}）: {message}"),
        other => other.to_string(),
    }
}

pub fn remote_file_path(dir: &str, name: &str) -> String {
    remote_join(dir, name)
}

pub fn local_name_for_remote(item: &RemoteItem) -> String {
    sanitize_component(if item.name.is_empty() {
        item.path.rsplit('/').next().unwrap_or("download")
    } else {
        &item.name
    })
}

pub fn safe_relative(base: &Path, relative: &str) -> PathBuf {
    safe_local_path(base, relative)
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_paths_include_remote_identity() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("same.bin");
        let first = RemoteItem {
            fs_id: 1,
            name: "same.bin".to_string(),
            path: "/a/same.bin".to_string(),
            ..Default::default()
        };
        let second = RemoteItem {
            fs_id: 2,
            name: "same.bin".to_string(),
            path: "/b/same.bin".to_string(),
            ..Default::default()
        };
        assert_eq!(part_path(&target, &first), part_path(&target, &first));
        assert_ne!(part_path(&target, &first), part_path(&target, &second));

        // Without an fs_id, fall back to a stable remote-path fingerprint.
        let unknown_a = RemoteItem {
            fs_id: 0,
            name: "same.bin".to_string(),
            path: "/a/same.bin".to_string(),
            ..Default::default()
        };
        let unknown_b = RemoteItem {
            fs_id: 0,
            name: "same.bin".to_string(),
            path: "/b/same.bin".to_string(),
            ..Default::default()
        };
        assert_eq!(
            part_path(&target, &unknown_a),
            part_path(&target, &unknown_a)
        );
        assert_ne!(
            part_path(&target, &unknown_a),
            part_path(&target, &unknown_b)
        );
    }

    #[tokio::test]
    async fn range_state_rejects_missing_or_truncated_part() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("part");
        let total = RANGE_CHUNK + 10;
        let mut state = RangeState::new(total);

        assert!(!validate_range_state(&part, &mut state).await);

        std::fs::write(&part, vec![3_u8; total as usize]).unwrap();
        state.completed = vec![true, true];
        state.chunk_md5 = vec![Some("a".to_string()), Some("b".to_string())];
        let metadata = std::fs::metadata(&part).unwrap();
        state.part_len = total;
        state.part_mtime_ns = mtime_ns(&metadata);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&part)
            .unwrap()
            .set_len(total - 1)
            .unwrap();
        assert!(!validate_range_state(&part, &mut state).await);
    }

    #[tokio::test]
    async fn range_state_rehashes_corrupt_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("part");
        let total = RANGE_CHUNK + 10;
        let data = vec![7_u8; total as usize];
        std::fs::write(&part, &data).unwrap();
        let first_len = RANGE_CHUNK as usize;
        let first_md5 = hex::encode(Md5::digest(&data[..first_len]));

        let mut state = RangeState::new(total);
        state.completed = vec![true, true];
        state.chunk_md5 = vec![Some("deadbeef".to_string()), Some("deadbeef".to_string())];
        assert!(validate_range_state(&part, &mut state).await);
        assert!(!state.completed[0]);
        assert!(!state.completed[1]);

        state.completed[0] = true;
        state.chunk_md5[0] = Some(first_md5);
        state.part_len = 0;
        state.part_mtime_ns = 0;
        assert!(validate_range_state(&part, &mut state).await);
        assert!(state.completed[0]);
        assert!(!state.completed[1]);
    }

    #[test]
    fn range_state_rejects_old_versions() {
        let mut state = RangeState::new(RANGE_CHUNK);
        assert!(state.valid_for(RANGE_CHUNK));
        state.version = 1;
        assert!(!state.valid_for(RANGE_CHUNK));
    }
}
