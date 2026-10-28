use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};

use crate::api::sign::decrypt_md5;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteItem {
    #[serde(default)]
    pub fs_id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub is_dir: bool,
    #[serde(default)]
    pub mtime: i64,
    #[serde(default)]
    pub ctime: i64,
    #[serde(default)]
    pub md5: String,
    #[serde(default)]
    pub block_list: Vec<String>,
}

impl RemoteItem {
    pub fn display_size(&self) -> String {
        if self.is_dir {
            "-".to_string()
        } else {
            format_bytes(self.size)
        }
    }

    pub fn modified(&self) -> String {
        format_timestamp(self.mtime)
    }

    pub fn kind_label(&self) -> &'static str {
        if self.is_dir { "DIR" } else { "FILE" }
    }

    pub fn extension(&self) -> String {
        Path::new(&self.name)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
    }

    /// Build the absolute remote path for this item. `cwd` is only used when
    /// the API returned a relative name (some search responses do).
    pub fn absolute_path(&self, cwd: &str) -> String {
        if self.path.starts_with('/') {
            self.path.clone()
        } else {
            remote_join(cwd, &self.path)
        }
    }

    pub fn parent_dir(&self, cwd: &str) -> String {
        remote_parent(&self.absolute_path(cwd))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Quota {
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

impl Quota {
    pub fn used_ratio(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.used as f64 / self.total as f64).clamp(0.0, 1.0)
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{} / {} 已用（剩余 {}）",
            format_bytes(self.used),
            format_bytes(self.total),
            format_bytes(self.free)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserInfo {
    pub baidu_name: String,
    pub netdisk_name: String,
    pub uk: u64,
    pub vip_type: u32,
    pub avatar_url: String,
}

impl UserInfo {
    pub fn best_name(&self) -> &str {
        if !self.netdisk_name.is_empty() {
            &self.netdisk_name
        } else if !self.baidu_name.is_empty() {
            &self.baidu_name
        } else {
            "（未知账号）"
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiListItem {
    #[serde(default)]
    pub fs_id: u64,
    #[serde(default)]
    pub server_filename: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub isdir: u8,
    #[serde(default)]
    pub server_mtime: i64,
    #[serde(default)]
    pub server_ctime: i64,
    #[serde(default)]
    pub md5: String,
    #[serde(default)]
    pub block_list: Vec<String>,
}

impl From<ApiListItem> for RemoteItem {
    fn from(value: ApiListItem) -> Self {
        Self {
            fs_id: value.fs_id,
            name: value.server_filename,
            path: value.path,
            size: value.size,
            is_dir: value.isdir == 1,
            mtime: value.server_mtime,
            ctime: value.server_ctime,
            md5: decrypt_md5(&value.md5),
            block_list: value.block_list,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    NameAsc,
    NameDesc,
    SizeAsc,
    SizeDesc,
    TimeAsc,
    TimeDesc,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            SortMode::NameAsc => "名称 ↑",
            SortMode::NameDesc => "名称 ↓",
            SortMode::SizeAsc => "大小 ↑",
            SortMode::SizeDesc => "大小 ↓",
            SortMode::TimeAsc => "时间 ↑",
            SortMode::TimeDesc => "时间 ↓",
        }
    }

    pub fn next(self) -> Self {
        match self {
            SortMode::NameAsc => SortMode::NameDesc,
            SortMode::NameDesc => SortMode::SizeAsc,
            SortMode::SizeAsc => SortMode::SizeDesc,
            SortMode::SizeDesc => SortMode::TimeAsc,
            SortMode::TimeAsc => SortMode::TimeDesc,
            SortMode::TimeDesc => SortMode::NameAsc,
        }
    }

    pub fn sort(&self, items: &mut [RemoteItem]) {
        items.sort_by(|a, b| {
            let dir_order = b.is_dir.cmp(&a.is_dir);
            if dir_order != std::cmp::Ordering::Equal {
                return dir_order;
            }
            match self {
                SortMode::NameAsc => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortMode::NameDesc => b.name.to_lowercase().cmp(&a.name.to_lowercase()),
                SortMode::SizeAsc => a.size.cmp(&b.size),
                SortMode::SizeDesc => b.size.cmp(&a.size),
                SortMode::TimeAsc => a.mtime.cmp(&b.mtime),
                SortMode::TimeDesc => b.mtime.cmp(&a.mtime),
            }
            .then_with(|| a.name.cmp(&b.name))
        });
    }
}

pub fn remote_join(base: &str, child: &str) -> String {
    if child.starts_with('/') {
        return normalize_remote(child);
    }
    let base = base.trim_end_matches('/');
    let child = child.trim_start_matches('/');
    if base.is_empty() {
        normalize_remote(&format!("/{child}"))
    } else {
        normalize_remote(&format!("{base}/{child}"))
    }
}

pub fn remote_parent(path: &str) -> String {
    let path = path.trim_end_matches('/');
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => path[..index].to_string(),
    }
}

pub fn normalize_remote(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// Convert a remote name/path into a safe local path below `base`.
pub fn safe_local_path(base: &Path, remote_relative: &str) -> PathBuf {
    let mut output = base.to_path_buf();
    for component in remote_relative.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            continue;
        }
        let component = sanitize_component(component);
        if component.is_empty() {
            continue;
        }
        output.push(component);
    }
    output
}

pub fn sanitize_component(name: &str) -> String {
    let trimmed = name.trim();
    let mut value: String = trimmed
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | '\0' => '_',
            _ => ch,
        })
        .collect();
    if value.is_empty() || value == "." || value == ".." {
        value = "_".to_string();
    }
    // Windows compatibility: trailing dots/spaces are invalid there.
    while value.ends_with(' ') || value.ends_with('.') {
        value.pop();
    }
    if value.is_empty() {
        value = "_".to_string();
    }
    value
}

pub fn ensure_under(base: &Path, candidate: &Path) -> bool {
    let Ok(base) = base.canonicalize() else {
        return false;
    };
    let mut existing = candidate.to_path_buf();
    while !existing.exists() {
        let Some(parent) = existing.parent() else {
            return false;
        };
        existing = parent.to_path_buf();
    }
    let Ok(existing) = existing.canonicalize() else {
        return false;
    };
    existing.starts_with(base)
}

pub fn has_parent_component(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 5] = [
        ("PB", 1_u64 << 50),
        ("TB", 1_u64 << 40),
        ("GB", 1_u64 << 30),
        ("MB", 1_u64 << 20),
        ("KB", 1_u64 << 10),
    ];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    for (unit, factor) in UNITS {
        if bytes >= factor {
            let value = bytes as f64 / factor as f64;
            let mut number = if value >= 100.0 {
                format!("{value:.0}")
            } else if value >= 10.0 {
                format!("{value:.1}")
            } else {
                format!("{value:.2}")
            };
            while number.ends_with('0') {
                number.pop();
            }
            if number.ends_with('.') {
                number.pop();
            }
            return format!("{number}{unit}");
        }
    }
    format!("{bytes}B")
}

pub fn format_speed(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return "--".to_string();
    }
    format!("{}/s", format_bytes(bytes_per_second.round() as u64))
}

pub fn format_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

pub fn format_timestamp(timestamp: i64) -> String {
    if timestamp <= 0 {
        return "-".to_string();
    }
    match Local.timestamp_opt(timestamp, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => "-".to_string(),
    }
}

pub fn format_elapsed(started: std::time::Instant) -> String {
    format_duration(started.elapsed().as_secs())
}

pub fn parse_datetime_for_display(timestamp: i64) -> String {
    let dt: DateTime<Local> = Local
        .timestamp_opt(timestamp, 0)
        .single()
        .unwrap_or_else(|| Local.timestamp_opt(0, 0).single().unwrap());
    dt.to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_and_normalizes_remote_paths() {
        assert_eq!(remote_join("/apps/bypy", "saved"), "/apps/bypy/saved");
        assert_eq!(remote_join("/apps/bypy/", "/root"), "/root");
        assert_eq!(remote_join("/apps/bypy", "../x"), "/apps/x");
        assert_eq!(remote_parent("/apps/bypy/saved"), "/apps/bypy");
        assert_eq!(remote_parent("/file"), "/");
    }

    #[test]
    fn sanitizes_local_components() {
        assert_eq!(sanitize_component("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitize_component("正常 文件.pdf"), "正常 文件.pdf");
        assert_eq!(sanitize_component(".."), "_");
        assert_eq!(
            safe_local_path(Path::new("/tmp/base"), "a/../b"),
            PathBuf::from("/tmp/base/a/b")
        );
    }

    #[test]
    fn sorts_directories_first() {
        let mut items = vec![
            RemoteItem {
                name: "b".into(),
                size: 2,
                ..Default::default()
            },
            RemoteItem {
                name: "a".into(),
                size: 1,
                is_dir: true,
                ..Default::default()
            },
            RemoteItem {
                name: "c".into(),
                size: 3,
                ..Default::default()
            },
        ];
        SortMode::NameAsc.sort(&mut items);
        assert!(items[0].is_dir);
        assert_eq!(items[1].name, "b");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(1024), "1KB");
        assert_eq!(format_bytes(188 * 1024 * 1024), "188MB");
    }
}
