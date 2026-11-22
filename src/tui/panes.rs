use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ratatui::widgets::TableState;

use crate::api::BaiduClient;
use crate::api::group::{GroupInfo, GroupShareEntry};
use crate::api::models::{RemoteItem, SortMode, remote_parent};
use crate::api::share::ShareSession;

#[derive(Debug, Clone)]
pub struct LocalEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub is_symlink: bool,
}

pub struct LocalPane {
    pub cwd: PathBuf,
    pub entries: Vec<LocalEntry>,
    pub selected: usize,
    pub filter: String,
    pub show_hidden: bool,
    pub error: Option<String>,
    pub table_state: TableState,
}

impl LocalPane {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            entries: Vec::new(),
            selected: 0,
            filter: String::new(),
            show_hidden: false,
            error: None,
            table_state: TableState::default(),
        }
    }

    pub fn reload(&mut self) {
        self.error = None;
        let read = match std::fs::read_dir(&self.cwd) {
            Ok(read) => read,
            Err(err) => {
                self.entries.clear();
                self.error = Some(format!("读取本地目录失败: {err}"));
                return;
            }
        };
        let mut entries = Vec::new();
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            let is_symlink = metadata.file_type().is_symlink();
            let followed = if is_symlink {
                std::fs::metadata(&path).ok()
            } else {
                Some(metadata.clone())
            };
            let is_dir = followed.as_ref().map(|meta| meta.is_dir()).unwrap_or(false);
            let size = followed.as_ref().map(|meta| meta.len()).unwrap_or(0);
            let modified = metadata.modified().ok();
            entries.push(LocalEntry {
                path,
                name,
                is_dir,
                size,
                modified,
                is_symlink,
            });
        }
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        self.entries = entries;
        self.clamp_selection();
    }

    pub fn visible_indices(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                (self.show_hidden || !entry.name.starts_with('.'))
                    && (needle.is_empty() || entry.name.to_lowercase().contains(&needle))
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub fn visible_len(&self) -> usize {
        self.visible_indices().len()
    }

    pub fn current(&self) -> Option<&LocalEntry> {
        let index = *self.visible_indices().get(self.selected)?;
        self.entries.get(index)
    }

    pub fn marked_or_current(&self, marked: &HashSet<PathBuf>) -> Vec<LocalEntry> {
        if marked.is_empty() {
            return self.current().cloned().into_iter().collect();
        }
        self.entries
            .iter()
            .filter(|entry| marked.contains(&entry.path))
            .cloned()
            .collect()
    }

    pub fn clamp_selection(&mut self) {
        let len = self.visible_len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub fn parent(&self) -> Option<PathBuf> {
        self.cwd.parent().map(Path::to_path_buf)
    }
}

#[derive(Debug, Clone)]
pub struct ShareSessionState {
    pub session: ShareSession,
    pub dir: String,
}

#[derive(Debug, Clone)]
pub struct GroupDirLevel {
    pub name: String,
    pub dir: String,
    pub msg_id: String,
    pub from_uk: u64,
    pub fs_id: u64,
}

#[derive(Debug, Clone)]
pub struct GroupSessionState {
    pub group: GroupInfo,
    pub shares: Vec<GroupShareEntry>,
    pub path_stack: Vec<GroupDirLevel>,
}

impl GroupSessionState {
    pub fn current_dir_display(&self) -> String {
        if self.path_stack.is_empty() {
            "/".to_string()
        } else {
            let joined = self
                .path_stack
                .iter()
                .map(|lvl| lvl.name.as_str())
                .collect::<Vec<_>>()
                .join("/");
            format!("/{joined}")
        }
    }

    pub fn active_dir_context(&self) -> Option<&GroupDirLevel> {
        self.path_stack.last()
    }
}

#[derive(Debug, Clone)]
pub enum RemoteMode {
    Browse,
    Search { keyword: String, base: String },
    Share(ShareSessionState),
    GroupList(Vec<GroupInfo>),
    GroupShare(GroupSessionState),
}

pub struct RemotePane {
    pub cwd: String,
    pub entries: Vec<RemoteItem>,
    pub selected: usize,
    pub filter: String,
    pub sort: SortMode,
    pub loading: bool,
    pub error: Option<String>,
    pub mode: RemoteMode,
    pub table_state: TableState,
}

impl RemotePane {
    pub fn new(cwd: String) -> Self {
        Self {
            cwd,
            entries: Vec::new(),
            selected: 0,
            filter: String::new(),
            sort: SortMode::NameAsc,
            loading: false,
            error: None,
            mode: RemoteMode::Browse,
            table_state: TableState::default(),
        }
    }

    pub fn visible_indices(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| needle.is_empty() || entry.name.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect()
    }

    pub fn visible_len(&self) -> usize {
        self.visible_indices().len()
    }

    pub fn current(&self) -> Option<&RemoteItem> {
        let index = *self.visible_indices().get(self.selected)?;
        self.entries.get(index)
    }

    pub fn marked_or_current(&self, marked: &HashMap<String, RemoteRef>) -> Vec<RemoteItem> {
        if marked.is_empty() {
            return self.current().cloned().into_iter().collect();
        }
        self.entries
            .iter()
            .filter(|item| marked.contains_key(&item.absolute_path(&self.cwd)))
            .cloned()
            .collect()
    }

    pub fn sort_entries(&mut self) {
        self.sort.sort(&mut self.entries);
    }

    pub fn clamp_selection(&mut self) {
        let len = self.visible_len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub fn is_read_only(&self) -> bool {
        matches!(
            self.mode,
            RemoteMode::Share(_) | RemoteMode::GroupList(_) | RemoteMode::GroupShare(_)
        )
    }

    pub fn title_path(&self) -> String {
        match &self.mode {
            RemoteMode::Browse => self.cwd.clone(),
            RemoteMode::Search { keyword, base } => format!("搜索 “{keyword}” @ {base}"),
            RemoteMode::Share(share) => format!("分享 [{}] {}", share.session.title, share.dir),
            RemoteMode::GroupList(_) => "群组列表".to_string(),
            RemoteMode::GroupShare(group) => {
                format!("群组 [{}] {}", group.group.name, group.current_dir_display())
            }
        }
    }
}

/// Stable identity for a remote item while it is selected for a destructive
/// action. Paths alone are not identity: the server may reuse a path for a
/// different file between the listing and the confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRef {
    pub fs_id: u64,
    pub path: String,
    pub name: String,
    pub is_dir: bool,
}

impl RemoteRef {
    pub fn from_item(item: &RemoteItem, cwd: &str) -> Self {
        Self {
            fs_id: item.fs_id,
            path: item.absolute_path(cwd),
            name: item.name.clone(),
            is_dir: item.is_dir,
        }
    }
}

pub fn remote_ref_matches(reference: &RemoteRef, current: &RemoteItem) -> bool {
    reference.fs_id != 0
        && current.fs_id == reference.fs_id
        && current.is_dir == reference.is_dir
        && current.name == reference.name
}

/// Re-check every selected item immediately before a destructive remote call,
/// listing each unique parent directory at most once. If any item changed or
/// vanished, the whole batch is aborted.
pub async fn verify_remote_refs(client: &BaiduClient, refs: &[RemoteRef]) -> Result<(), String> {
    let mut parents: HashMap<String, Vec<RemoteItem>> = HashMap::new();
    for reference in refs {
        if reference.fs_id == 0 {
            return Err(format!(
                "无法确认远程项目身份（缺少 fs_id）：{}；请刷新后重试",
                reference.path
            ));
        }
        let parent = remote_parent(&reference.path);
        if !parents.contains_key(&parent) {
            let items = client
                .list_dir(&parent)
                .await
                .map_err(|err| format!("无法确认远程项目 {}: {err}", reference.path))?;
            parents.insert(parent.clone(), items);
        }
        let items = &parents[&parent];
        let Some(current) = items
            .iter()
            .find(|item| item.name == reference.name || item.path == reference.path)
        else {
            return Err(format!("远程项目已不存在: {}", reference.path));
        };
        if !remote_ref_matches(reference, current) {
            return Err(format!(
                "远程项目已变化：{}（原 fs_id {}，当前 fs_id {}）",
                reference.path, reference.fs_id, current.fs_id
            ));
        }
    }
    Ok(())
}
