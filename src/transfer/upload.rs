use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use crc32fast::Hasher as Crc32;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::api::models::{format_bytes, remote_join, remote_parent};
use crate::api::{ApiError, BaiduClient};
use crate::transfer::download::wait_for_retry;
use crate::transfer::types::{ProgressReporter, TransferStatus, UploadOptions};

pub const SLICE_MD5_SIZE: u64 = 256 * 1024;
pub const EMPTY_MD5: &str = "d41d8cd98f00b204e9800998ecf8427e";

#[derive(Debug, Clone)]
pub struct FileHashes {
    pub size: u64,
    pub md5: String,
    pub slice_md5: String,
    pub crc32: u32,
    pub block_size: u64,
    pub blocks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadState {
    pub version: u32,
    pub local_path: String,
    pub local_size: u64,
    pub local_mtime: i64,
    /// Whole-file MD5. Prevents resuming blocks from a different file that
    /// happens to have the same size and mtime.
    #[serde(default)]
    pub local_md5: String,
    pub remote_path: String,
    pub upload_id: String,
    pub block_size: u64,
    pub block_count: usize,
    /// `None` means the block still needs to be uploaded.
    pub block_md5: Vec<Option<String>>,
}

impl UploadState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        local: &Path,
        local_size: u64,
        local_mtime: i64,
        local_md5: &str,
        remote: &str,
        upload_id: String,
        block_size: u64,
        block_count: usize,
    ) -> Self {
        Self {
            version: 2,
            local_path: local.to_string_lossy().to_string(),
            local_size,
            local_mtime,
            local_md5: local_md5.to_string(),
            remote_path: remote.to_string(),
            upload_id,
            block_size,
            block_count,
            block_md5: vec![None; block_count],
        }
    }

    pub fn matches(
        &self,
        local: &Path,
        size: u64,
        mtime: i64,
        local_md5: &str,
        remote: &str,
    ) -> bool {
        self.version == 2
            && self.local_path == local.to_string_lossy()
            && self.local_size == size
            && self.local_mtime == mtime
            && self.local_md5 == local_md5
            && self.remote_path == remote
            && self.block_md5.len() == self.block_count
    }

    pub fn complete_blocks(&self) -> usize {
        self.block_md5
            .iter()
            .filter(|value| value.is_some())
            .count()
    }
}

pub fn block_size_for(size: u64, chunk_mib: u64) -> u64 {
    if chunk_mib > 0 {
        return chunk_mib.clamp(1, 64) * 1024 * 1024;
    }
    if size >= 32 * 1024 * 1024 * 1024 {
        64 * 1024 * 1024
    } else if size >= 8 * 1024 * 1024 * 1024 {
        16 * 1024 * 1024
    } else {
        4 * 1024 * 1024
    }
}

/// Compute every checksum needed by the upload handshake in one sequential
/// pass. Block MD5s are the block list used by `/api/precreate` and
/// `/api/create`.
pub fn hash_file(path: &Path, chunk_mib: u64) -> Result<FileHashes> {
    let mut file = File::open(path).with_context(|| format!("打开文件失败: {}", path.display()))?;
    let size = file.metadata()?.len();
    let block_size = block_size_for(size, chunk_mib);
    let mut whole = Md5::new();
    let mut slice = Md5::new();
    let mut crc = Crc32::new();
    let mut blocks = Vec::new();
    let mut slice_remaining = SLICE_MD5_SIZE;
    let mut buffer = vec![0_u8; block_size as usize];

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
        let block = &buffer[..filled];
        whole.update(block);
        crc.update(block);
        if slice_remaining > 0 {
            let take = (slice_remaining as usize).min(block.len());
            slice.update(&block[..take]);
            slice_remaining -= take as u64;
        }
        blocks.push(hex::encode(Md5::digest(block)));
        if filled < buffer.len() {
            break;
        }
    }

    if size == 0 {
        // `/api/create` rejects an empty block list for empty files but
        // accepts the MD5 of the empty string as the single block.
        blocks.push(EMPTY_MD5.to_string());
    }

    Ok(FileHashes {
        size,
        md5: if size == 0 {
            EMPTY_MD5.to_string()
        } else {
            hex::encode(whole.finalize())
        },
        slice_md5: if size == 0 {
            EMPTY_MD5.to_string()
        } else {
            hex::encode(slice.finalize())
        },
        crc32: crc.finalize(),
        block_size,
        blocks,
    })
}

pub fn state_path(dir: &Path, local: &Path, remote: &str, size: u64, mtime: i64) -> PathBuf {
    let mut hasher = Md5::new();
    hasher.update(local.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(remote.as_bytes());
    hasher.update(b"\0");
    hasher.update(size.to_le_bytes());
    hasher.update(mtime.to_le_bytes());
    dir.join(format!("{}.json", hex::encode(hasher.finalize())))
}

pub fn load_state(path: &Path) -> Option<UploadState> {
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

pub fn save_state(path: &Path, state: &UploadState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(state)?;
    {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&data)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn remove_state(path: &Path) {
    let _ = std::fs::remove_file(path);
}

pub struct UploadRequest<'a> {
    pub local: &'a Path,
    pub remote_dir: &'a str,
    pub state_dir: &'a Path,
}

/// Upload one file, resuming from `upload-state` when the previous run was
/// interrupted. Returns the final remote path.
pub async fn upload_one(
    client: &BaiduClient,
    request: UploadRequest<'_>,
    options: &UploadOptions,
    cancel: &CancellationToken,
    reporter: &mut ProgressReporter,
) -> Result<String> {
    let metadata = tokio::fs::metadata(request.local)
        .await
        .with_context(|| format!("读取本地文件失败: {}", request.local.display()))?;
    if !metadata.is_file() {
        bail!("不是普通文件: {}", request.local.display());
    }
    let size = metadata.len();
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let file_name = request
        .local
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "upload.bin".to_string());
    let remote_path = remote_join(request.remote_dir, &file_name);

    reporter.ensure_total(size, 1);
    reporter.set_status(TransferStatus::Hashing, "正在计算 MD5 / CRC32");
    let local = request.local.to_path_buf();
    let chunk_mib = options.chunk_mib;
    let hashes = tokio::task::spawn_blocking(move || hash_file(&local, chunk_mib))
        .await
        .context("校验和任务失败")??;
    if cancel.is_cancelled() {
        bail!("上传已取消");
    }

    reporter.set_status(
        TransferStatus::Running,
        format!(
            "{}，{} 个分片",
            format_bytes(hashes.size),
            hashes.blocks.len()
        ),
    );

    let state_file = state_path(request.state_dir, request.local, &remote_path, size, mtime);
    let mut state = load_state(&state_file)
        .filter(|state| state.matches(request.local, size, mtime, &hashes.md5, &remote_path));
    if let Some(existing) = state.as_ref() {
        if existing.block_size != hashes.block_size || existing.block_count != hashes.blocks.len() {
            state = None;
        }
    }

    let mut remote_created = false;
    let upload_id = match state.as_ref().map(|state| state.upload_id.clone()) {
        Some(upload_id) if !upload_id.is_empty() => upload_id,
        _ => {
            ensure_remote_dir(client, request.remote_dir).await?;
            remote_created = true;
            let block_list = json_block_list(&hashes.blocks);
            let response = client
                .precreate(
                    &remote_path,
                    hashes.size,
                    &hashes.md5,
                    &hashes.slice_md5,
                    hashes.crc32,
                    &block_list,
                )
                .await
                .map_err(|err| anyhow!(describe_upload_error(&err)))?;
            if response.return_type == 2 {
                remove_state(&state_file);
                reporter.update(size, "秒传成功");
                return Ok(remote_path);
            }
            if response.upload_id.is_empty() {
                bail!(
                    "precreate 未返回 uploadid（return_type={}）",
                    response.return_type
                );
            }
            let new_state = UploadState::new(
                request.local,
                size,
                mtime,
                &hashes.md5,
                &remote_path,
                response.upload_id.clone(),
                hashes.block_size,
                hashes.blocks.len(),
            );
            save_state(&state_file, &new_state)?;
            state = Some(new_state);
            response.upload_id
        }
    };

    if !remote_created {
        ensure_remote_dir(client, request.remote_dir).await?;
    }

    let mut state = state.unwrap_or_else(|| {
        UploadState::new(
            request.local,
            size,
            mtime,
            &hashes.md5,
            &remote_path,
            upload_id.clone(),
            hashes.block_size,
            hashes.blocks.len(),
        )
    });
    state.upload_id = upload_id.clone();

    // Blocks that were confirmed in an earlier run count as progress.
    let mut done: u64 = 0;
    for (index, md5) in state.block_md5.iter().enumerate() {
        if md5.is_some() {
            done = done.saturating_add(block_len(size, hashes.block_size, index));
        }
    }
    if done > 0 {
        reporter.update(
            done,
            format!("断点续传：已完成 {} 个分片", state.complete_blocks()),
        );
    }

    for (index, expected) in hashes.blocks.iter().enumerate() {
        if state.block_md5[index].is_some() {
            continue;
        }
        if cancel.is_cancelled() {
            bail!("上传已取消");
        }
        let offset = index as u64 * hashes.block_size;
        let length = block_len(size, hashes.block_size, index);
        if length == 0 {
            state.block_md5[index] = Some(expected.clone());
            continue;
        }
        let mut attempt = 0;
        loop {
            if cancel.is_cancelled() {
                bail!("上传已取消");
            }
            let uploaded = client
                .upload_part(
                    &upload_id,
                    &remote_path,
                    index,
                    offset,
                    request.local,
                    length,
                )
                .await;
            let md5 = match uploaded {
                Ok(md5) => md5,
                Err(err) => {
                    if err.is_upload_id_expired() {
                        remove_state(&state_file);
                        return Err(anyhow!(
                            "上传会话已在服务端失效或过期（已清理旧断点，重新运行上传即可新建会话）: {}",
                            describe_upload_error(&err)
                        ));
                    }
                    if attempt >= options.retries {
                        return Err(anyhow!(
                            "上传分片 {} 失败: {}",
                            index + 1,
                            describe_upload_error(&err)
                        ));
                    }
                    reporter.set_status(
                        TransferStatus::Running,
                        format!(
                            "分片 {} 重试 {}/{}: {err}",
                            index + 1,
                            attempt + 1,
                            options.retries
                        ),
                    );
                    wait_for_retry(attempt).await;
                    attempt += 1;
                    continue;
                }
            };
            // The server returns the MD5 it computed for this part. Treat a
            // mismatch as a failed part instead of baking a corrupt block into
            // the final file.
            if !md5.eq_ignore_ascii_case(expected) {
                if attempt >= options.retries {
                    bail!(
                        "上传分片 {} 校验不一致（本地 {expected}，服务端 {md5}）",
                        index + 1
                    );
                }
                reporter.set_status(
                    TransferStatus::Running,
                    format!(
                        "分片 {} 校验不一致，重试 {}/{}",
                        index + 1,
                        attempt + 1,
                        options.retries
                    ),
                );
                wait_for_retry(attempt).await;
                attempt += 1;
                continue;
            }
            state.block_md5[index] = Some(md5);
            save_state(&state_file, &state)?;
            done = done.saturating_add(length);
            reporter.update(
                done,
                format!("正在上传分片 {}/{}", index + 1, hashes.blocks.len()),
            );
            break;
        }
    }

    let block_list = state
        .block_md5
        .iter()
        .map(|value| value.clone().unwrap_or_default())
        .collect::<Vec<_>>();
    if block_list.iter().any(String::is_empty) {
        bail!("仍有分片未完成，断点已保存，可重新运行上传");
    }
    reporter.set_status(TransferStatus::Running, "正在合并分片");
    if let Err(err) = client
        .create_file(
            &upload_id,
            &remote_path,
            size,
            &json_block_list(&block_list),
        )
        .await
    {
        if err.is_upload_id_expired() {
            remove_state(&state_file);
            return Err(anyhow!(
                "合并分片失败，上传会话已在服务端失效或过期（已清理旧断点，重新运行上传即可新建会话）: {}",
                describe_upload_error(&err)
            ));
        }
        return Err(anyhow!(describe_upload_error(&err)));
    }
    verify_uploaded_content(client, &remote_path, &hashes.md5).await?;
    // Keep the completed block list until content verification passes, so a
    // retry can merge again without re-uploading every block.
    remove_state(&state_file);
    reporter.update(size, "上传完成");
    Ok(remote_path)
}

/// The web merge endpoint records a block-list MD5 in directory metadata, so
/// re-read the download host's authoritative `Content-MD5` header. A missing
/// header is tolerated (some hosts omit it), but a definite mismatch means the
/// merge produced the wrong bytes and must not be reported as success.
async fn verify_uploaded_content(
    client: &BaiduClient,
    remote_path: &str,
    expected: &str,
) -> Result<()> {
    let mut last = None;
    for attempt in 0..3 {
        match client.content_md5(remote_path).await {
            Ok(Some(server)) if server == expected => return Ok(()),
            Ok(Some(server)) => last = Some(format!("服务器内容 MD5 {server}，本地 {expected}")),
            Ok(None) => last = Some("服务器未返回内容 MD5".to_string()),
            Err(err) => last = Some(err.to_string()),
        }
        if attempt < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        }
    }
    if let Some(message) = last {
        if message.starts_with("服务器内容 MD5") {
            bail!("上传后内容校验失败：{message}");
        }
    }
    Ok(())
}

pub async fn ensure_remote_dir(client: &BaiduClient, path: &str) -> Result<()> {
    let path = if path.trim().is_empty() { "/" } else { path };
    let mut current = String::new();
    for component in path.split('/').filter(|part| !part.is_empty()) {
        current.push('/');
        current.push_str(component);
        client
            .ensure_dir(&current)
            .await
            .map_err(|err| anyhow!(describe_upload_error(&err)))?;
    }
    Ok(())
}

fn block_len(size: u64, block_size: u64, index: usize) -> u64 {
    let offset = index as u64 * block_size;
    if offset >= size {
        0
    } else {
        (size - offset).min(block_size)
    }
}

/// Baidu's precreate/create expect a JSON array. Upstream's `mergeStringList`
/// turns an empty block list into `[""]`; for empty files we use the MD5 of the
/// empty string, which both endpoints accept and `/api/create` requires.
pub fn json_block_list(blocks: &[String]) -> String {
    if blocks.is_empty() {
        return format!("[\"{EMPTY_MD5}\"]");
    }
    serde_json::to_string(blocks).unwrap_or_else(|_| format!("[\"{EMPTY_MD5}\"]"))
}

pub fn describe_upload_error(err: &ApiError) -> String {
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

/// Discover files under a local path and their intended remote paths.
pub fn plan_upload(local: &Path, remote_dir: &str) -> Result<Vec<(PathBuf, String, String)>> {
    let mut output = Vec::new();
    if local.is_file() {
        let name = local
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .ok_or_else(|| anyhow!("无法读取文件名: {}", local.display()))?;
        output.push((local.to_path_buf(), remote_dir.to_string(), name));
        return Ok(output);
    }
    if !local.is_dir() {
        bail!("路径不存在: {}", local.display());
    }
    let root_name = local
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .ok_or_else(|| anyhow!("无法读取目录名: {}", local.display()))?;
    let remote_root = remote_join(remote_dir, &root_name);
    let mut directories = vec![remote_root.clone()];
    for entry in walkdir::WalkDir::new(local).follow_links(false) {
        let entry = entry.with_context(|| format!("遍历目录失败: {}", local.display()))?;
        if entry.file_type().is_symlink() {
            continue;
        }
        let path = entry.path();
        if entry.file_type().is_dir() {
            if path != local {
                directories.push(relative_remote(&remote_root, local, path)?);
            }
        } else if entry.file_type().is_file() {
            let relative = relative_remote(&remote_root, local, path)?;
            let dir = remote_parent(&relative);
            output.push((path.to_path_buf(), dir, relative));
        }
    }
    // Keep the remote directory list in a deterministic order for callers
    // that create directories before uploading files.
    directories.sort();
    for dir in directories {
        output.push((PathBuf::new(), dir, String::new()));
    }
    output.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.cmp(&b.2)));
    Ok(output)
}

fn relative_remote(remote_root: &str, local_root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(local_root)
        .with_context(|| format!("计算相对路径失败: {}", path.display()))?;
    let mut remote = remote_root.to_string();
    for component in relative.components() {
        let text = component.as_os_str().to_string_lossy();
        remote = remote_join(&remote, &text);
    }
    Ok(remote)
}

/// A small self-check used by `pcstui doctor` and unit tests.
pub fn verify_hashes(path: &Path, chunk_mib: u64) -> Result<FileHashes> {
    hash_file(path, chunk_mib)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn hashes_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        let mut file = File::create(&path).unwrap();
        file.write_all(b"hello").unwrap();
        drop(file);
        let hashes = hash_file(&path, 0).unwrap();
        assert_eq!(hashes.size, 5);
        assert_eq!(hashes.md5, "5d41402abc4b2a76b9719d911017c592");
        assert_eq!(hashes.slice_md5, "5d41402abc4b2a76b9719d911017c592");
        assert_eq!(hashes.blocks.len(), 1);
        assert_eq!(hashes.block_size, 4 * 1024 * 1024);
    }

    #[test]
    fn empty_file_has_known_md5() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty");
        File::create(&path).unwrap();
        let hashes = hash_file(&path, 0).unwrap();
        assert_eq!(hashes.size, 0);
        assert_eq!(hashes.md5, EMPTY_MD5);
        assert_eq!(hashes.blocks, vec![EMPTY_MD5.to_string()]);
    }

    #[test]
    fn upload_state_requires_same_content() {
        let path = Path::new("/tmp/pcstui-upload-test.bin");
        let state = UploadState::new(path, 10, 123, "abc", "/remote/x.bin", "uid".into(), 4, 3);
        assert!(state.matches(path, 10, 123, "abc", "/remote/x.bin"));
        assert!(!state.matches(path, 10, 123, "def", "/remote/x.bin"));
        assert!(!state.matches(path, 10, 124, "abc", "/remote/x.bin"));
        assert!(!state.matches(path, 10, 123, "abc", "/remote/y.bin"));
    }

    #[test]
    fn upload_state_round_trips_after_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let local = Path::new("/tmp/pcstui-upload-test.bin");
        let mut state =
            UploadState::new(local, 10, 123, "abc", "/remote/x.bin", "uid".into(), 4, 3);
        state.block_md5[0] = Some("block".to_string());
        save_state(&path, &state).unwrap();
        let loaded = load_state(&path).unwrap();
        assert!(loaded.matches(local, 10, 123, "abc", "/remote/x.bin"));
        assert_eq!(loaded.complete_blocks(), 1);
    }

    #[test]
    fn empty_block_list_matches_upstream_shape() {
        assert_eq!(json_block_list(&[]), format!("[\"{EMPTY_MD5}\"]"));
        assert_eq!(json_block_list(&["abc".to_string()]), "[\"abc\"]");
    }

    #[test]
    fn splits_blocks_at_requested_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chunk.bin");
        std::fs::write(&path, vec![7_u8; 5 * 1024 * 1024]).unwrap();
        let hashes = hash_file(&path, 1).unwrap();
        assert_eq!(hashes.block_size, 1024 * 1024);
        assert_eq!(hashes.blocks.len(), 5);
        assert_eq!(hashes.blocks[4], hashes.blocks[0]);
    }

    #[test]
    fn plans_directory_upload() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("album");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("sub/b.txt"), b"b").unwrap();
        let plan = plan_upload(&root, "/apps/bypy/saved").unwrap();
        assert!(
            plan.iter()
                .any(|(_, dir, _)| dir == "/apps/bypy/saved/album/sub")
        );
        assert!(plan.iter().any(|(path, _, _)| path.ends_with("a.txt")));
    }
}
