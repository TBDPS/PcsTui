use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;
use tokio::sync::mpsc::UnboundedSender;

use crate::api::BaiduClient;
use crate::api::models::{
    Quota, RemoteItem, SortMode, UserInfo, format_bytes, format_timestamp, normalize_remote,
    remote_join, remote_parent, sanitize_component,
};
use crate::config::{RuntimeConfig, expand_tilde, home_dir};
use crate::transfer::types::{
    Direction, DownloadOptions, TransferEvent, TransferProgress, TransferStatus, UploadOptions,
};
use crate::transfer::{QueueStore, QueuedTask, QueuedTaskKind, TransferEngine};
use crate::tui::config_panel::ConfigPanel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Local,
    Remote,
    Transfers,
}

impl Focus {
    fn next(self, transfers_visible: bool) -> Self {
        match self {
            Focus::Local if transfers_visible => Focus::Remote,
            Focus::Local => Focus::Remote,
            Focus::Remote if transfers_visible => Focus::Transfers,
            Focus::Remote => Focus::Local,
            Focus::Transfers => Focus::Local,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    None,
    Local,
    Remote,
}

#[derive(Debug)]
pub enum AppEvent {
    RemoteLoaded {
        generation: u64,
        items: Vec<RemoteItem>,
        error: Option<String>,
    },
    SearchLoaded {
        generation: u64,
        keyword: String,
        base: String,
        items: Vec<RemoteItem>,
        error: Option<String>,
    },
    Quota(Result<Quota, String>),
    User(Result<UserInfo, String>),
    TaskDone {
        message: String,
        error: bool,
        refresh: Refresh,
    },
    Completions {
        token: u64,
        parent: String,
        prefix: String,
        items: Vec<RemoteItem>,
        error: Option<String>,
    },
    ConfigImportReady {
        candidate: Box<RuntimeConfig>,
        source: PathBuf,
        summary: String,
    },
    ConfigImportFailed {
        message: String,
    },
}

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
pub enum RemoteMode {
    Browse,
    Search { keyword: String, base: String },
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

    pub fn title_path(&self) -> String {
        match &self.mode {
            RemoteMode::Browse => self.cwd.clone(),
            RemoteMode::Search { keyword, base } => format!("搜索 “{keyword}” @ {base}"),
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

fn remote_ref_matches(reference: &RemoteRef, current: &RemoteItem) -> bool {
    reference.fs_id != 0
        && current.fs_id == reference.fs_id
        && current.is_dir == reference.is_dir
        && current.name == reference.name
}

/// Re-check every selected item immediately before a destructive remote call.
/// If any item changed or vanished, the whole batch is aborted. Partial
/// deletion based on stale paths is worse than making the user refresh.
async fn verify_remote_refs(client: &BaiduClient, refs: &[RemoteRef]) -> Result<(), String> {
    for reference in refs {
        if reference.fs_id == 0 {
            return Err(format!(
                "无法确认远程项目身份（缺少 fs_id）：{}；请刷新后重试",
                reference.path
            ));
        }
        let current = client
            .stat(&reference.path)
            .await
            .map_err(|err| format!("无法确认远程项目 {}: {err}", reference.path))?;
        if !remote_ref_matches(reference, &current) {
            return Err(format!(
                "远程项目已变化：{}（原 fs_id {}，当前 fs_id {}）",
                reference.path, reference.fs_id, current.fs_id
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum InputKind {
    FilterLocal,
    FilterRemote,
    Search,
    Mkdir,
    Goto,
    Rename { from: RemoteRef },
    MoveDest { sources: Vec<RemoteRef> },
    CopyDest { sources: Vec<RemoteRef> },
    ExportConfig { redacted: bool },
    ImportConfig,
}

pub struct InputState {
    pub title: String,
    pub prompt: String,
    pub value: String,
    /// Cursor position in Unicode scalar values (not bytes).
    pub cursor: usize,
    pub kind: InputKind,
}

impl InputState {
    pub fn new(
        title: impl Into<String>,
        prompt: impl Into<String>,
        value: String,
        kind: InputKind,
    ) -> Self {
        let cursor = value.chars().count();
        Self {
            title: title.into(),
            prompt: prompt.into(),
            value,
            cursor,
            kind,
        }
    }

    fn set_value(&mut self, value: String) {
        self.cursor = value.chars().count();
        self.value = value;
    }

    fn char_count(&self) -> usize {
        self.value.chars().count()
    }

    fn insert(&mut self, ch: char) {
        let byte = char_to_byte(&self.value, self.cursor);
        self.value.insert(byte, ch);
        self.cursor += 1;
    }

    fn insert_text(&mut self, text: &str) {
        for ch in text.chars().filter(|ch| !ch.is_control() || *ch == '\t') {
            self.insert(ch);
        }
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = char_to_byte(&self.value, self.cursor);
        let start = char_to_byte(&self.value, self.cursor - 1);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        if self.cursor >= self.char_count() {
            return;
        }
        let start = char_to_byte(&self.value, self.cursor);
        let end = char_to_byte(&self.value, self.cursor + 1);
        self.value.replace_range(start..end, "");
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_count());
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.char_count();
    }

    fn move_word_left(&mut self) {
        let chars: Vec<char> = self.value.chars().collect();
        let mut index = self.cursor;
        while index > 0 && chars[index - 1].is_whitespace() {
            index -= 1;
        }
        while index > 0 && !chars[index - 1].is_whitespace() {
            index -= 1;
        }
        self.cursor = index;
    }

    fn move_word_right(&mut self) {
        let chars: Vec<char> = self.value.chars().collect();
        let mut index = self.cursor;
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        while index < chars.len() && !chars[index].is_whitespace() {
            index += 1;
        }
        self.cursor = index;
    }

    fn delete_word_left(&mut self) {
        let old = self.cursor;
        self.move_word_left();
        let start = char_to_byte(&self.value, self.cursor);
        let end = char_to_byte(&self.value, old);
        self.value.replace_range(start..end, "");
    }

    fn delete_to_start(&mut self) {
        let end = char_to_byte(&self.value, self.cursor);
        self.value.replace_range(..end, "");
        self.cursor = 0;
    }

    fn delete_to_end(&mut self) {
        let start = char_to_byte(&self.value, self.cursor);
        self.value.truncate(start);
    }
}

fn char_to_byte(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(value.len())
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    DeleteRemote(Vec<RemoteRef>),
    DeleteLocal(Vec<PathBuf>),
    ImportConfig {
        candidate: Box<RuntimeConfig>,
        source: PathBuf,
    },
    Relogin,
    Quit,
}

pub struct ConfirmState {
    pub title: String,
    pub message: String,
    pub action: ConfirmAction,
}

pub struct StatusMessage {
    pub text: String,
    pub error: bool,
    pub at: std::time::Instant,
}

impl StatusMessage {
    fn new(text: impl Into<String>, error: bool) -> Self {
        Self {
            text: text.into(),
            error,
            at: std::time::Instant::now(),
        }
    }
}

pub struct App {
    pub config: RuntimeConfig,
    pub client: BaiduClient,
    pub engine: TransferEngine,
    pub tx: UnboundedSender<AppEvent>,
    pub local: LocalPane,
    pub remote: RemotePane,
    pub focus: Focus,
    pub transfers: Vec<TransferProgress>,
    pub transfer_selected: usize,
    pub transfer_state: TableState,
    pub show_transfers: bool,
    pub quota: Option<Quota>,
    pub user: Option<UserInfo>,
    pub status: StatusMessage,
    pub input: Option<InputState>,
    pub confirm: Option<ConfirmState>,
    pub config_panel: Option<ConfigPanel>,
    pub help: bool,
    pub marked_local: HashSet<PathBuf>,
    pub marked_remote: HashMap<String, RemoteRef>,
    pub should_quit: bool,
    pub restart_requested: bool,
    pub relogin_requested: bool,
    pub request_yazi: bool,
    pub dirty_config: bool,
    pub generation: u64,
    pub tick: u64,
    pub queue: QueueStore,
    pub queue_ids: HashMap<u64, String>,
    pub queue_error: Option<String>,
    pub quick_index: String,
    pub quick_at: Option<Instant>,
    pub completion_token: u64,
}

impl App {
    pub fn new(
        config: RuntimeConfig,
        client: BaiduClient,
        engine: TransferEngine,
        tx: UnboundedSender<AppEvent>,
    ) -> Self {
        let local_cwd = std::env::current_dir().unwrap_or_else(|_| config.paths.root.clone());
        let mut local = LocalPane::new(local_cwd);
        local.reload();
        let remote = RemotePane::new(config.file.last_remote_dir.clone());
        let (queue, queue_error) = match QueueStore::load(config.paths.queue_file.clone()) {
            Ok(queue) => (queue, None),
            Err(err) => (
                // A corrupt queue must not block the UI. Keep the file for
                // inspection and start with an empty in-memory queue.
                QueueStore::empty(config.paths.queue_file.clone()),
                Some(format!("待办队列读取失败: {err}")),
            ),
        };
        Self {
            config,
            client,
            engine,
            tx,
            local,
            remote,
            focus: Focus::Local,
            transfers: Vec::new(),
            transfer_selected: 0,
            transfer_state: TableState::default(),
            show_transfers: false,
            quota: None,
            user: None,
            status: StatusMessage::new("就绪 · Tab 切换 · ? 帮助", false),
            input: None,
            confirm: None,
            config_panel: None,
            help: false,
            marked_local: HashSet::new(),
            marked_remote: HashMap::new(),
            should_quit: false,
            restart_requested: false,
            relogin_requested: false,
            request_yazi: false,
            dirty_config: false,
            generation: 0,
            tick: 0,
            queue,
            queue_ids: HashMap::new(),
            queue_error,
            quick_index: String::new(),
            quick_at: None,
            completion_token: 0,
        }
    }

    pub fn start(&mut self) {
        self.load_remote();
        self.load_quota();
        self.load_user();
        self.resume_queue();
        if let Some(error) = self.queue_error.take() {
            self.set_status(error, true);
        }
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if self
            .quick_at
            .map(|at| at.elapsed() >= std::time::Duration::from_millis(450))
            .unwrap_or(false)
        {
            self.apply_quick_index();
        }
    }

    pub fn save_if_dirty(&mut self) {
        if self.dirty_config {
            if let Err(err) = self.config.save() {
                self.set_status(format!("保存配置失败: {err}"), true);
            } else {
                self.dirty_config = false;
            }
        }
    }

    fn save_queue(&mut self) {
        if let Err(err) = self.queue.save() {
            let message = format!("待办队列保存失败: {err}");
            self.queue_error = Some(message.clone());
            self.set_status(message, true);
        }
    }

    fn enqueue_download(
        &mut self,
        item: RemoteItem,
        cwd: String,
        dest: PathBuf,
        options: DownloadOptions,
    ) -> bool {
        let task = QueuedTask::download(item.clone(), cwd.clone(), dest.clone(), options.clone());
        let queue_id = task.id.clone();
        if !self.queue.add(task) {
            return false;
        }
        self.save_queue();
        let id = self.engine.spawn_download(item, cwd, dest, options);
        self.queue_ids.insert(id, queue_id);
        true
    }

    fn enqueue_upload(
        &mut self,
        local: PathBuf,
        remote_dir: String,
        options: UploadOptions,
    ) -> bool {
        let task = QueuedTask::upload(local.clone(), remote_dir.clone(), options.clone());
        let queue_id = task.id.clone();
        if !self.queue.add(task) {
            return false;
        }
        self.save_queue();
        let id = self.engine.spawn_upload(local, remote_dir, options);
        self.queue_ids.insert(id, queue_id);
        true
    }

    fn resume_queue(&mut self) {
        if self.queue.tasks.is_empty() {
            return;
        }
        let removed = self.queue.dedupe();
        if removed > 0 {
            self.save_queue();
        }
        let tasks = self.queue.tasks.clone();
        let count = tasks.len();
        for task in tasks {
            let id = match &task.task {
                QueuedTaskKind::Download {
                    item,
                    cwd,
                    download_root,
                    options,
                } => self.engine.spawn_download(
                    item.clone(),
                    cwd.clone(),
                    download_root.clone(),
                    options.clone(),
                ),
                QueuedTaskKind::Upload {
                    local,
                    remote_dir,
                    options,
                } => self
                    .engine
                    .spawn_upload(local.clone(), remote_dir.clone(), options.clone()),
            };
            self.queue_ids.insert(id, task.id.clone());
        }
        if removed > 0 {
            self.set_status(
                format!("已恢复 {count} 个待办传输任务（移除 {removed} 个重复项）"),
                false,
            );
        } else {
            self.set_status(format!("已恢复 {count} 个待办传输任务"), false);
        }
    }

    fn push_quick_index(&mut self, ch: char) {
        if self.quick_index.is_empty() && ch == '0' {
            return;
        }
        if self.quick_index.len() >= 2 {
            self.quick_index.clear();
        }
        self.quick_index.push(ch);
        self.quick_at = Some(Instant::now());
        if self.quick_index.len() == 2 {
            self.apply_quick_index();
        }
    }

    fn apply_quick_index(&mut self) {
        self.quick_at = None;
        let digits = std::mem::take(&mut self.quick_index);
        let Ok(number) = digits.parse::<usize>() else {
            return;
        };
        if number == 0 {
            return;
        }
        let len = match self.focus {
            Focus::Local => self.local.visible_len(),
            Focus::Remote => self.remote.visible_len(),
            Focus::Transfers => self.transfers.len(),
        };
        let target = number - 1;
        if target >= len {
            self.set_status(format!("序号 {number} 超出范围（共 {len} 项）"), true);
            return;
        }
        match self.focus {
            Focus::Local => self.local.selected = target,
            Focus::Remote => self.remote.selected = target,
            Focus::Transfers => self.transfer_selected = target,
        }
        self.set_status(format!("跳转到第 {number} 项"), false);
    }

    pub fn selected_metadata(&self) -> String {
        match self.focus {
            Focus::Local => match self.local.current() {
                Some(entry) => format!(
                    "本地 {} · {} · 修改 {}",
                    entry.path.display(),
                    if entry.is_dir {
                        "目录".to_string()
                    } else {
                        format_bytes(entry.size)
                    },
                    local_time(entry.modified)
                ),
                None => "本地：无选中项".to_string(),
            },
            Focus::Remote => match self.remote.current() {
                Some(item) => {
                    let path = item.absolute_path(&self.remote.cwd);
                    let md5 = if item.md5.is_empty() {
                        "-"
                    } else {
                        item.md5.as_str()
                    };
                    format!(
                        "远程 fs_id {} · MD5 {} · {} · 创建 {} · 修改 {} · {}",
                        item.fs_id,
                        md5,
                        item.display_size(),
                        format_timestamp(item.ctime),
                        format_timestamp(item.mtime),
                        path
                    )
                }
                None => "远程：无选中项".to_string(),
            },
            Focus::Transfers => match self.transfers.get(self.transfer_selected) {
                Some(progress) => format!(
                    "{} #{} {} · {} → {} · {} / {} · {}",
                    progress.direction.arrow(),
                    progress.id,
                    progress.name,
                    progress.remote_path,
                    progress.local_path.display(),
                    format_bytes(progress.done),
                    format_bytes(progress.total),
                    progress.status.label()
                ),
                None => "传输：无选中项".to_string(),
            },
        }
    }

    pub fn take_yazi_request(&mut self) -> bool {
        let requested = self.request_yazi;
        self.request_yazi = false;
        requested
    }

    pub fn on_yazi_done(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.local.reload();
                self.set_status("Yazi 已退出，本地目录已刷新", false);
            }
            Err(err) => self.set_status(err, true),
        }
    }

    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::RemoteLoaded {
                generation,
                items,
                error,
            } => {
                if generation != self.generation {
                    return;
                }
                self.remote.loading = false;
                if let Some(error) = error {
                    self.remote.error = Some(error.clone());
                    self.remote.entries.clear();
                    self.set_status(error, true);
                } else {
                    self.remote.error = None;
                    self.remote.entries = items;
                    self.remote.sort_entries();
                    self.remote.clamp_selection();
                }
            }
            AppEvent::SearchLoaded {
                generation,
                keyword,
                base,
                items,
                error,
            } => {
                if generation != self.generation {
                    return;
                }
                self.remote.loading = false;
                if let Some(error) = error {
                    self.remote.error = Some(error.clone());
                    self.remote.entries.clear();
                    self.set_status(error, true);
                } else {
                    self.remote.error = None;
                    self.remote.mode = RemoteMode::Search { keyword, base };
                    self.remote.entries = items;
                    self.remote.sort_entries();
                    self.remote.selected = 0;
                    self.remote.clamp_selection();
                    self.set_status(format!("搜索完成：{} 项", self.remote.entries.len()), false);
                }
            }
            AppEvent::Quota(result) => match result {
                Ok(quota) => self.quota = Some(quota),
                Err(err) => self.set_status(format!("容量查询失败: {err}"), true),
            },
            AppEvent::User(result) => match result {
                Ok(user) => self.user = Some(user),
                Err(err) => self.set_status(format!("账号查询失败: {err}"), true),
            },
            AppEvent::TaskDone {
                message,
                error,
                refresh,
            } => {
                self.set_status(message, error);
                match refresh {
                    Refresh::None => {}
                    Refresh::Local => self.local.reload(),
                    Refresh::Remote => self.refresh_remote(),
                }
            }
            AppEvent::Completions {
                token,
                parent,
                prefix,
                items,
                error,
            } => {
                if token != self.completion_token {
                    return;
                }
                if let Some(error) = error {
                    self.set_status(format!("补全失败: {error}"), true);
                    return;
                }
                self.apply_completions(&parent, &prefix, items);
            }
            AppEvent::ConfigImportReady {
                candidate,
                source,
                summary,
            } => {
                if self.config_panel.is_none() {
                    self.set_status("导入验证已完成，但配置面板已关闭；未修改配置", false);
                    return;
                }
                if let Some(panel) = self.config_panel.as_mut() {
                    panel.busy = false;
                    panel.set_message(format!("已验证导入账号: {summary}"), false);
                }
                self.confirm = Some(ConfirmState {
                    title: "导入配置".to_string(),
                    message: format!(
                        "已验证账号：{summary}\n来源：{}\n导入会替换当前配置，并自动备份旧配置。确认导入？",
                        source.display()
                    ),
                    action: ConfirmAction::ImportConfig { candidate, source },
                });
            }
            AppEvent::ConfigImportFailed { message } => {
                if let Some(panel) = self.config_panel.as_mut() {
                    panel.busy = false;
                    panel.set_message(message.clone(), true);
                }
                self.set_status(message, true);
            }
        }
    }

    pub fn handle_transfer(&mut self, event: TransferEvent) {
        match event {
            TransferEvent::Added(progress) => {
                if let Some(existing) = self
                    .transfers
                    .iter_mut()
                    .find(|item| item.id == progress.id)
                {
                    *existing = progress;
                } else {
                    self.transfers.push(progress);
                }
                self.show_transfers = true;
                self.transfer_selected = self.transfers.len().saturating_sub(1);
                self.set_status("已加入传输队列", false);
            }
            TransferEvent::Updated(progress) => {
                if let Some(existing) = self
                    .transfers
                    .iter_mut()
                    .find(|item| item.id == progress.id)
                {
                    *existing = progress;
                }
            }
            TransferEvent::Finished(progress) => {
                let direction = progress.direction;
                let transfer_id = progress.id;
                let completed = progress.status == TransferStatus::Completed;
                let message = format!(
                    "{} {}: {}",
                    direction.label(),
                    if completed { "完成" } else { "结束" },
                    progress.name
                );
                if let Some(existing) = self
                    .transfers
                    .iter_mut()
                    .find(|item| item.id == transfer_id)
                {
                    *existing = progress;
                }
                if let Some(queue_id) = self.queue_ids.remove(&transfer_id) {
                    if self.queue.remove(&queue_id) {
                        self.save_queue();
                    }
                }
                if completed {
                    match direction {
                        Direction::Download => self.local.reload(),
                        Direction::Upload => self.refresh_remote(),
                    }
                }
                self.set_status(message, !completed);
            }
        }
        self.transfer_selected = self
            .transfer_selected
            .min(self.transfers.len().saturating_sub(1));
    }

    pub fn handle_paste(&mut self, text: &str) {
        if let Some(input) = self.input.as_mut() {
            input.insert_text(text);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.request_quit();
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if self.confirm.is_some() {
            self.handle_confirm_key(key);
            return;
        }
        if self.input.is_some() {
            self.handle_input_key(key);
            return;
        }
        if self.config_panel.is_some() {
            self.handle_config_panel_key(key);
            return;
        }
        if let KeyCode::Char(ch) = key.code {
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
                && ch.is_ascii_digit()
            {
                self.push_quick_index(ch);
                return;
            }
        }
        // Any other key ends a pending numeric jump.
        self.quick_index.clear();
        self.quick_at = None;
        match key.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('C') => self.open_config_panel(),
            KeyCode::Char('p') => {
                self.show_transfers = !self.show_transfers;
                if self.show_transfers {
                    self.focus = Focus::Transfers;
                } else if self.focus == Focus::Transfers {
                    self.focus = Focus::Remote;
                }
            }
            KeyCode::Tab => {
                self.focus = self.focus.next(self.show_transfers);
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Local if self.show_transfers => Focus::Transfers,
                    Focus::Local => Focus::Remote,
                    Focus::Remote => Focus::Local,
                    Focus::Transfers => Focus::Remote,
                };
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Home => self.select_first(),
            KeyCode::End => self.select_last(),
            KeyCode::Enter | KeyCode::Right => self.enter(),
            KeyCode::Backspace | KeyCode::Left => self.go_parent(),
            KeyCode::Esc => {
                if self.focus != Focus::Transfers {
                    self.clear_search();
                }
            }
            KeyCode::Char('r') => {
                if self.focus != Focus::Transfers {
                    self.refresh_focused();
                }
            }
            KeyCode::Char('R') => {
                if self.focus == Focus::Remote {
                    self.start_rename();
                } else {
                    self.wrong_focus("重命名", "远程");
                }
            }
            KeyCode::Char('n') => {
                if self.focus == Focus::Remote {
                    self.start_mkdir();
                } else {
                    self.wrong_focus("新建远程目录", "远程");
                }
            }
            KeyCode::Char('g') => {
                if self.focus == Focus::Remote {
                    self.start_goto();
                } else {
                    self.wrong_focus("跳转远程目录", "远程");
                }
            }
            KeyCode::Char('f') => {
                if self.focus == Focus::Remote {
                    self.start_search();
                } else {
                    self.wrong_focus("搜索网盘", "远程");
                }
            }
            KeyCode::Char('/') => {
                if self.focus == Focus::Transfers {
                    self.wrong_focus("过滤", "本地或远程");
                } else {
                    self.start_filter();
                }
            }
            KeyCode::Char('s') => match self.focus {
                Focus::Remote => {
                    self.remote.sort = self.remote.sort.next();
                    self.remote.sort_entries();
                    self.set_status(format!("远程排序: {}", self.remote.sort.label()), false);
                }
                Focus::Local => {
                    self.local.show_hidden = !self.local.show_hidden;
                    self.local.clamp_selection();
                }
                Focus::Transfers => self.wrong_focus("排序", "远程"),
            },
            KeyCode::Char('h') => {
                if self.focus == Focus::Local {
                    self.local.show_hidden = !self.local.show_hidden;
                    self.local.clamp_selection();
                    self.set_status(
                        if self.local.show_hidden {
                            "显示隐藏文件"
                        } else {
                            "隐藏隐藏文件"
                        },
                        false,
                    );
                } else {
                    self.wrong_focus("隐藏文件开关", "本地");
                }
            }
            KeyCode::Char(' ') => {
                if self.focus == Focus::Transfers {
                    self.wrong_focus("标记", "本地或远程");
                } else {
                    self.toggle_mark();
                }
            }
            KeyCode::Char('d') => {
                if self.focus == Focus::Remote {
                    self.download_selected();
                } else {
                    self.wrong_focus("下载", "远程");
                }
            }
            KeyCode::Char('u') => {
                if self.focus == Focus::Local {
                    self.upload_selected();
                } else {
                    self.wrong_focus("上传", "本地");
                }
            }
            KeyCode::Char('x') => {
                if self.focus == Focus::Transfers {
                    self.wrong_focus("删除", "本地或远程");
                } else {
                    self.delete_selected();
                }
            }
            KeyCode::Char('m') => {
                if self.focus == Focus::Remote {
                    self.start_move();
                } else {
                    self.wrong_focus("移动", "远程");
                }
            }
            KeyCode::Char('c') => {
                if self.focus == Focus::Remote {
                    self.start_copy();
                } else {
                    self.wrong_focus("复制", "远程");
                }
            }
            KeyCode::Char('K') => {
                if self.focus == Focus::Transfers {
                    self.cancel_selected_transfer();
                } else {
                    self.wrong_focus("取消传输", "传输");
                }
            }
            KeyCode::Char('y') => {
                if self.focus == Focus::Local {
                    self.request_yazi = true;
                } else {
                    self.wrong_focus("启动 Yazi", "本地");
                }
            }
            KeyCode::Char('o') => {
                if self.focus == Focus::Local {
                    self.open_local();
                } else {
                    self.wrong_focus("打开本地文件", "本地");
                }
            }
            _ => {}
        }
    }

    fn wrong_focus(&mut self, action: &str, expected: &str) {
        self.set_status(format!("{action}仅在{expected}面板可用"), true);
    }

    fn open_config_panel(&mut self) {
        self.config_panel = Some(ConfigPanel::open(&self.config, self.queue_error.as_deref()));
        self.set_status(
            "配置与诊断 · e 导出 · E 脱敏导出 · i 导入 · r 重新登录 · d 刷新",
            false,
        );
    }

    fn handle_config_panel_key(&mut self, key: KeyEvent) {
        let busy = self.config_panel.as_ref().is_some_and(|panel| panel.busy);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('C') => {
                self.config_panel = None;
                self.set_status("已关闭配置与诊断", false);
            }
            _ if busy => {
                self.set_panel_message("正在验证导入配置，请稍候…", false);
            }
            KeyCode::Char('e') => {
                let path = default_export_path(&self.config, false);
                self.open_input(InputState::new(
                    "导出配置（含凭据）",
                    "导出为明文 JSON，包含账号凭据；请妥善保管",
                    path.display().to_string(),
                    InputKind::ExportConfig { redacted: false },
                ));
            }
            KeyCode::Char('E') => {
                let path = default_export_path(&self.config, true);
                self.open_input(InputState::new(
                    "导出配置（脱敏）",
                    "导出为明文 JSON，不包含账号凭据，可安全分享",
                    path.display().to_string(),
                    InputKind::ExportConfig { redacted: true },
                ));
            }
            KeyCode::Char('i') => {
                self.open_input(InputState::new(
                    "导入配置",
                    "明文 pcstui.json 路径（导入前会在线验证账号）",
                    String::new(),
                    InputKind::ImportConfig,
                ));
            }
            KeyCode::Char('r') => {
                self.confirm = Some(ConfirmState {
                    title: "重新登录".to_string(),
                    message: "重新登录会重建客户端并停止当前传输；未完成任务会保留在队列中，重启后继续。确认？"
                        .to_string(),
                    action: ConfirmAction::Relogin,
                });
            }
            KeyCode::Char('d') => {
                if let Some(panel) = self.config_panel.as_mut() {
                    panel.refresh_local(&self.config, self.queue_error.as_deref());
                    panel.set_message("正在刷新在线诊断…", false);
                }
                self.load_user();
                self.load_quota();
                self.refresh_remote();
                self.set_status("正在刷新配置与在线诊断…", false);
            }
            _ => {}
        }
    }

    fn set_panel_message(&mut self, text: impl Into<String>, error: bool) {
        if let Some(panel) = self.config_panel.as_mut() {
            panel.set_message(text, error);
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(confirm) = self.confirm.take() {
                    self.execute_confirm(confirm.action);
                }
            }
            // Enter is deliberately a cancel key. A stray Enter after opening
            // a destructive confirmation must never delete anything.
            KeyCode::Enter
            | KeyCode::Char('n')
            | KeyCode::Char('N')
            | KeyCode::Esc
            | KeyCode::Char('q') => {
                self.confirm = None;
                self.set_status("已取消", false);
            }
            _ => {}
        }
    }

    fn execute_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::DeleteRemote(refs) => {
                let client = self.client.clone();
                let tx = self.tx.clone();
                let count = refs.len();
                tokio::spawn(async move {
                    let (message, error) = match verify_remote_refs(&client, &refs).await {
                        Ok(()) => {
                            let paths = refs
                                .iter()
                                .map(|reference| reference.path.clone())
                                .collect::<Vec<_>>();
                            match client.remove(&paths).await {
                                Ok(()) => (format!("已删除 {count} 项"), false),
                                Err(err) => (format!("删除失败: {err}"), true),
                            }
                        }
                        Err(err) => (format!("删除已取消: {err}"), true),
                    };
                    let _ = tx.send(AppEvent::TaskDone {
                        message,
                        error,
                        refresh: Refresh::Remote,
                    });
                });
                self.marked_remote.clear();
            }
            ConfirmAction::DeleteLocal(paths) => {
                let mut errors = Vec::new();
                for path in paths {
                    let result = if path.is_dir() && !path.is_symlink() {
                        std::fs::remove_dir_all(&path)
                    } else {
                        std::fs::remove_file(&path)
                    };
                    if let Err(err) = result {
                        errors.push(format!("{}: {err}", path.display()));
                    }
                }
                self.marked_local.clear();
                self.local.reload();
                if errors.is_empty() {
                    self.set_status("本地文件已删除", false);
                } else {
                    self.set_status(format!("部分删除失败: {}", errors.join("; ")), true);
                }
            }
            ConfirmAction::ImportConfig { candidate, source } => {
                match candidate.save_with_backup() {
                    Ok(backup) => {
                        self.config = *candidate;
                        self.restart_requested = true;
                        let backup = backup
                            .map(|path| format!("，旧配置备份到 {}", path.display()))
                            .unwrap_or_default();
                        let message =
                            format!("配置已导入（{}）{backup}，正在重启…", source.display());
                        self.set_panel_message(message.clone(), false);
                        self.set_status(message, false);
                    }
                    Err(err) => {
                        let message = format!("导入配置失败，当前配置未替换: {err:#}");
                        self.set_panel_message(message.clone(), true);
                        self.set_status(message, true);
                    }
                }
            }
            ConfirmAction::Relogin => {
                self.relogin_requested = true;
                self.set_status("正在重新登录…", false);
            }
            ConfirmAction::Quit => {
                self.should_quit = true;
            }
        }
    }

    fn open_input(&mut self, input: InputState) {
        self.completion_token = self.completion_token.wrapping_add(1);
        self.input = Some(input);
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.completion_token = self.completion_token.wrapping_add(1);
                self.input = None;
                self.set_status("已取消输入", false);
                return;
            }
            KeyCode::Enter => {
                self.completion_token = self.completion_token.wrapping_add(1);
                if let Some(input) = self.input.take() {
                    self.submit_input(input);
                }
                return;
            }
            KeyCode::Tab => {
                self.request_completion();
                return;
            }
            _ => {}
        }
        let Some(input) = self.input.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.move_word_left()
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.move_word_right()
            }
            KeyCode::Left => input.move_left(),
            KeyCode::Right => input.move_right(),
            KeyCode::Home => input.home(),
            KeyCode::End => input.end(),
            KeyCode::Delete => input.delete(),
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.delete_word_left()
            }
            KeyCode::Backspace => input.backspace(),
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => input.home(),
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => input.end(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.delete_to_start()
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.delete_to_end()
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.delete_word_left()
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                input.insert(ch)
            }
            _ => {}
        }
    }

    fn completion_supported(kind: &InputKind) -> bool {
        matches!(
            kind,
            InputKind::Goto
                | InputKind::Mkdir
                | InputKind::Rename { .. }
                | InputKind::MoveDest { .. }
                | InputKind::CopyDest { .. }
        )
    }

    fn request_completion(&mut self) {
        let Some(input) = self.input.as_ref() else {
            return;
        };
        if !Self::completion_supported(&input.kind) {
            return;
        }
        let cwd = self.remote.cwd.clone();
        let (parent, prefix) = completion_target(&input.value, &cwd);
        self.completion_token = self.completion_token.wrapping_add(1);
        let token = self.completion_token;
        let client = self.client.clone();
        let tx = self.tx.clone();
        let requested_parent = parent.clone();
        tokio::spawn(async move {
            let result = client.list_dir(&requested_parent).await;
            let (items, error) = match result {
                Ok(items) => (items, None),
                Err(err) => (Vec::new(), Some(err.to_string())),
            };
            let _ = tx.send(AppEvent::Completions {
                token,
                parent: requested_parent,
                prefix,
                items,
                error,
            });
        });
        self.set_status("正在获取补全候选…", false);
    }

    fn apply_completions(&mut self, parent: &str, prefix: &str, items: Vec<RemoteItem>) {
        let cwd = self.remote.cwd.clone();
        let Some(input) = self.input.as_ref() else {
            return;
        };
        if !Self::completion_supported(&input.kind) {
            return;
        }
        // Ignore a stale answer if the user has changed the target meanwhile.
        let (current_parent, current_prefix) = completion_target(&input.value, &cwd);
        if current_parent != parent || current_prefix != prefix {
            return;
        }
        let absolute_input = input.value.trim_start().starts_with('/');
        let needle = prefix.to_lowercase();
        let mut names = items
            .into_iter()
            .filter(|item| item.name.to_lowercase().starts_with(&needle))
            .map(|item| (item.name, item.is_dir))
            .collect::<Vec<_>>();
        names.sort_by_key(|a| a.0.to_lowercase());
        if names.is_empty() {
            self.set_status(format!("没有匹配 “{prefix}” 的远程项目"), true);
            return;
        }
        let common = if names.len() == 1 {
            let (name, is_dir) = &names[0];
            format!("{name}{}", if *is_dir { "/" } else { "" })
        } else {
            let refs = names
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>();
            common_prefix(&refs)
        };
        if names.len() > 1 && common.chars().count() <= prefix.chars().count() {
            let preview = names
                .iter()
                .take(6)
                .map(|(name, is_dir)| format!("{name}{}", if *is_dir { "/" } else { "" }))
                .collect::<Vec<_>>()
                .join("  ");
            self.set_status(format!("补全候选: {preview}"), false);
            return;
        }
        let new_value = render_completed_path(&cwd, parent, &common, absolute_input);
        let Some(input) = self.input.as_mut() else {
            return;
        };
        input.set_value(new_value);
        let count = names.len();
        self.set_status(
            if count == 1 {
                "已补全".to_string()
            } else {
                format!("已补全公共前缀（{count} 个候选）")
            },
            false,
        );
    }

    fn submit_input(&mut self, input: InputState) {
        let value = input.value.trim().to_string();
        match input.kind {
            InputKind::FilterLocal => {
                self.local.filter = value;
                self.local.selected = 0;
                self.local.clamp_selection();
                self.set_status(
                    if self.local.filter.is_empty() {
                        "已清除本地过滤".to_string()
                    } else {
                        format!("本地过滤: {}", self.local.filter)
                    },
                    false,
                );
            }
            InputKind::FilterRemote => {
                self.remote.filter = value;
                self.remote.selected = 0;
                self.remote.clamp_selection();
                self.set_status(
                    if self.remote.filter.is_empty() {
                        "已清除远程过滤".to_string()
                    } else {
                        format!("远程过滤: {}", self.remote.filter)
                    },
                    false,
                );
            }
            InputKind::Search => {
                if value.is_empty() {
                    return;
                }
                let base = self.remote.cwd.clone();
                self.remote.loading = true;
                self.remote.error = None;
                self.generation += 1;
                let generation = self.generation;
                let client = self.client.clone();
                let tx = self.tx.clone();
                let keyword = value.clone();
                tokio::spawn(async move {
                    let result = client.search(&keyword, &base, true).await;
                    let (items, error) = match result {
                        Ok(items) => (items, None),
                        Err(err) => (Vec::new(), Some(err.to_string())),
                    };
                    let _ = tx.send(AppEvent::SearchLoaded {
                        generation,
                        keyword,
                        base,
                        items,
                        error,
                    });
                });
            }
            InputKind::Mkdir => {
                if value.is_empty() {
                    return;
                }
                let path = remote_join(&self.remote.cwd, &value);
                let client = self.client.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = crate::transfer::upload::ensure_remote_dir(&client, &path).await;
                    let (message, error) = match result {
                        Ok(()) => (format!("已创建: {path}"), false),
                        Err(err) => (format!("创建目录失败: {err}"), true),
                    };
                    let _ = tx.send(AppEvent::TaskDone {
                        message,
                        error,
                        refresh: Refresh::Remote,
                    });
                });
            }
            InputKind::Goto => {
                if value.is_empty() {
                    return;
                }
                self.navigate_remote(normalize_remote(&value));
            }
            InputKind::Rename { from } => {
                if value.is_empty() {
                    return;
                }
                let to = if value.starts_with('/') {
                    normalize_remote(&value)
                } else {
                    remote_join(&remote_parent(&from.path), &value)
                };
                if to == from.path {
                    self.set_status("新名称与原名称相同", true);
                    return;
                }
                let client = self.client.clone();
                let tx = self.tx.clone();
                let reference = from;
                tokio::spawn(async move {
                    let (message, error) =
                        match verify_remote_refs(&client, std::slice::from_ref(&reference)).await {
                            Ok(()) => match client.rename(&reference.path, &to).await {
                                Ok(()) => (format!("已重命名: {} → {to}", reference.path), false),
                                Err(err) => (format!("重命名失败: {err}"), true),
                            },
                            Err(err) => (format!("重命名已取消: {err}"), true),
                        };
                    let _ = tx.send(AppEvent::TaskDone {
                        message,
                        error,
                        refresh: Refresh::Remote,
                    });
                });
            }
            InputKind::MoveDest { sources } => {
                if value.is_empty() || sources.is_empty() {
                    return;
                }
                let dest = normalize_remote(&value);
                let client = self.client.clone();
                let tx = self.tx.clone();
                let count = sources.len();
                tokio::spawn(async move {
                    let (message, error) = match verify_remote_refs(&client, &sources).await {
                        Ok(()) => {
                            let paths = sources
                                .iter()
                                .map(|reference| reference.path.clone())
                                .collect::<Vec<_>>();
                            match client.move_paths(&paths, &dest).await {
                                Ok(()) => (format!("已移动 {count} 项到 {dest}"), false),
                                Err(err) => (format!("移动失败: {err}"), true),
                            }
                        }
                        Err(err) => (format!("移动已取消: {err}"), true),
                    };
                    let _ = tx.send(AppEvent::TaskDone {
                        message,
                        error,
                        refresh: Refresh::Remote,
                    });
                });
                self.marked_remote.clear();
            }
            InputKind::CopyDest { sources } => {
                if value.is_empty() || sources.is_empty() {
                    return;
                }
                let dest = normalize_remote(&value);
                let client = self.client.clone();
                let tx = self.tx.clone();
                let count = sources.len();
                tokio::spawn(async move {
                    let (message, error) = match verify_remote_refs(&client, &sources).await {
                        Ok(()) => {
                            let paths = sources
                                .iter()
                                .map(|reference| reference.path.clone())
                                .collect::<Vec<_>>();
                            match client.copy_paths(&paths, &dest).await {
                                Ok(()) => (format!("已复制 {count} 项到 {dest}"), false),
                                Err(err) => (format!("复制失败: {err}"), true),
                            }
                        }
                        Err(err) => (format!("复制已取消: {err}"), true),
                    };
                    let _ = tx.send(AppEvent::TaskDone {
                        message,
                        error,
                        refresh: Refresh::Remote,
                    });
                });
                self.marked_remote.clear();
            }
            InputKind::ExportConfig { redacted } => {
                let path = if value.is_empty() {
                    default_export_path(&self.config, redacted)
                } else {
                    expand_tilde(Path::new(&value))
                };
                if path.exists() {
                    let message = format!("导出目标已存在，请换一个路径: {}", path.display());
                    self.set_panel_message(message.clone(), true);
                    self.set_status(message, true);
                    return;
                }
                match self.config.export_to(&path, redacted) {
                    Ok(()) => {
                        let message = if redacted {
                            format!("已导出脱敏配置: {}", path.display())
                        } else {
                            format!(
                                "已导出明文配置（含账号凭据）: {}；请妥善保管",
                                path.display()
                            )
                        };
                        self.set_panel_message(message.clone(), false);
                        self.set_status(message, false);
                    }
                    Err(err) => {
                        let message = format!("导出配置失败: {err:#}");
                        self.set_panel_message(message.clone(), true);
                        self.set_status(message, true);
                    }
                }
            }
            InputKind::ImportConfig => {
                if value.is_empty() {
                    return;
                }
                let path = expand_tilde(Path::new(&value));
                let mut candidate = self.config.clone();
                if let Err(err) = candidate.adopt_file(&path) {
                    let message = format!("读取导入配置失败: {err:#}");
                    self.set_panel_message(message.clone(), true);
                    self.set_status(message, true);
                    return;
                }
                if let Some(panel) = self.config_panel.as_mut() {
                    panel.busy = true;
                    panel.set_message("正在在线验证导入配置…", false);
                }
                self.set_status("正在在线验证导入配置…", false);
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = async {
                        let client = BaiduClient::new(&candidate).map_err(|err| err.to_string())?;
                        let user = client.whoami().await.map_err(|err| err.to_string())?;
                        if let Some(account) = candidate.file.account.as_mut() {
                            account.uid = user.uk;
                            if account.name.is_empty() {
                                account.name = user.best_name().to_string();
                            }
                        }
                        Ok::<String, String>(user.best_name().to_string())
                    }
                    .await;
                    match result {
                        Ok(summary) => {
                            let _ = tx.send(AppEvent::ConfigImportReady {
                                candidate: Box::new(candidate),
                                source: path,
                                summary,
                            });
                        }
                        Err(message) => {
                            let _ = tx.send(AppEvent::ConfigImportFailed {
                                message: format!("导入配置在线验证失败: {message}"),
                            });
                        }
                    }
                });
            }
        }
    }

    fn start_filter(&mut self) {
        match self.focus {
            Focus::Local => {
                self.open_input(InputState::new(
                    "过滤本地",
                    "输入关键字（留空清除）",
                    self.local.filter.clone(),
                    InputKind::FilterLocal,
                ));
            }
            Focus::Remote | Focus::Transfers => {
                self.open_input(InputState::new(
                    "过滤远程",
                    "输入关键字（留空清除）",
                    self.remote.filter.clone(),
                    InputKind::FilterRemote,
                ));
            }
        }
    }

    fn start_search(&mut self) {
        self.open_input(InputState::new(
            "搜索网盘",
            "关键字（当前目录及子目录）",
            String::new(),
            InputKind::Search,
        ));
    }

    fn start_mkdir(&mut self) {
        self.open_input(InputState::new(
            "新建远程目录",
            format!("在 {} 下创建（可含 /）", self.remote.cwd),
            String::new(),
            InputKind::Mkdir,
        ));
    }

    fn start_goto(&mut self) {
        self.open_input(InputState::new(
            "跳转远程目录",
            "绝对路径",
            self.remote.cwd.clone(),
            InputKind::Goto,
        ));
    }

    fn start_rename(&mut self) {
        let Some(item) = self.remote.current() else {
            self.set_status("没有选中的远程项目", true);
            return;
        };
        self.open_input(InputState::new(
            "重命名远程项目",
            "新名称（可输入完整路径）",
            item.name.clone(),
            InputKind::Rename {
                from: RemoteRef::from_item(item, &self.remote.cwd),
            },
        ));
    }

    fn start_move(&mut self) {
        let sources = self
            .remote
            .marked_or_current(&self.marked_remote)
            .iter()
            .map(|item| RemoteRef::from_item(item, &self.remote.cwd))
            .collect::<Vec<_>>();
        if sources.is_empty() {
            self.set_status("没有选中的远程项目", true);
            return;
        }
        self.open_input(InputState::new(
            "移动远程项目",
            "目标目录（绝对路径）",
            self.remote.cwd.clone(),
            InputKind::MoveDest { sources },
        ));
    }

    fn start_copy(&mut self) {
        let sources = self
            .remote
            .marked_or_current(&self.marked_remote)
            .iter()
            .map(|item| RemoteRef::from_item(item, &self.remote.cwd))
            .collect::<Vec<_>>();
        if sources.is_empty() {
            self.set_status("没有选中的远程项目", true);
            return;
        }
        self.open_input(InputState::new(
            "复制远程项目",
            "目标目录（绝对路径）",
            self.remote.cwd.clone(),
            InputKind::CopyDest { sources },
        ));
    }

    fn download_selected(&mut self) {
        let items = self.remote.marked_or_current(&self.marked_remote);
        if items.is_empty() {
            self.set_status("没有选中的远程项目", true);
            return;
        }
        let options = DownloadOptions {
            overwrite: false,
            rename_existing: true,
            verify_md5: self.config.file.transfer.verify_md5,
            connections: self.config.file.transfer.download_connections,
            retries: self.config.file.transfer.retries,
        };
        let dest = self.local.cwd.clone();
        let cwd = self.remote.cwd.clone();
        let total = items.len();
        let mut added = 0;
        for item in items {
            if self.enqueue_download(item, cwd.clone(), dest.clone(), options.clone()) {
                added += 1;
            }
        }
        self.marked_remote.clear();
        if added == 0 {
            self.set_status("相同下载任务已在队列中", false);
        } else if added < total {
            self.set_status(
                format!("已加入下载队列（{added}/{total} 项，其余重复已跳过）"),
                false,
            );
        } else {
            self.set_status(format!("已加入下载队列（{added} 项）"), false);
        }
    }

    fn upload_selected(&mut self) {
        let entries = self.local.marked_or_current(&self.marked_local);
        if entries.is_empty() {
            self.set_status("没有选中的本地项目", true);
            return;
        }
        let options = UploadOptions {
            chunk_mib: self.config.file.transfer.upload_chunk_mib,
            retries: self.config.file.transfer.retries,
        };
        let dest = self.remote.cwd.clone();
        let total = entries.len();
        let mut added = 0;
        for entry in entries {
            if self.enqueue_upload(entry.path, dest.clone(), options.clone()) {
                added += 1;
            }
        }
        self.marked_local.clear();
        if added == 0 {
            self.set_status("相同上传任务已在队列中", false);
        } else if added < total {
            self.set_status(
                format!("已加入上传队列（{added}/{total} 项，其余重复已跳过）"),
                false,
            );
        } else {
            self.set_status(format!("已加入上传队列（{added} 项）"), false);
        }
    }

    fn delete_selected(&mut self) {
        match self.focus {
            Focus::Local => {
                let entries = self.local.marked_or_current(&self.marked_local);
                if entries.is_empty() {
                    self.set_status("没有选中的本地项目", true);
                    return;
                }
                let paths = entries.iter().map(|entry| entry.path.clone()).collect();
                self.confirm = Some(ConfirmState {
                    title: "删除本地文件".to_string(),
                    message: format!("确定删除 {} 个本地项目？此操作不可恢复。", entries.len()),
                    action: ConfirmAction::DeleteLocal(paths),
                });
            }
            Focus::Remote => {
                let items = self.remote.marked_or_current(&self.marked_remote);
                if items.is_empty() {
                    self.set_status("没有选中的远程项目", true);
                    return;
                }
                let refs = items
                    .iter()
                    .map(|item| RemoteRef::from_item(item, &self.remote.cwd))
                    .collect::<Vec<_>>();
                self.confirm = Some(ConfirmState {
                    title: "删除远程文件".to_string(),
                    message: format!(
                        "确定删除 {} 个远程项目？此操作不可恢复。默认取消。",
                        refs.len()
                    ),
                    action: ConfirmAction::DeleteRemote(refs),
                });
            }
            Focus::Transfers => self.wrong_focus("删除", "本地或远程"),
        }
    }

    fn toggle_mark(&mut self) {
        match self.focus {
            Focus::Local => {
                if let Some(entry) = self.local.current() {
                    let path = entry.path.clone();
                    if !self.marked_local.insert(path.clone()) {
                        self.marked_local.remove(&path);
                    }
                }
            }
            Focus::Remote => {
                if let Some(item) = self.remote.current() {
                    let path = item.absolute_path(&self.remote.cwd);
                    if self.marked_remote.remove(&path).is_none() {
                        self.marked_remote
                            .insert(path, RemoteRef::from_item(item, &self.remote.cwd));
                    }
                }
            }
            Focus::Transfers => self.wrong_focus("标记", "本地或远程"),
        }
    }

    fn cancel_selected_transfer(&mut self) {
        let Some(progress) = self.transfers.get(self.transfer_selected) else {
            return;
        };
        if progress.status.is_terminal() {
            self.set_status("该传输已结束", false);
            return;
        }
        let id = progress.id;
        if self.engine.cancel(id) {
            self.set_status(format!("已请求取消传输 #{}", id), false);
        } else {
            self.set_status("传输已结束或无法取消", true);
        }
    }

    fn move_selection(&mut self, delta: isize) {
        match self.focus {
            Focus::Local => {
                let len = self.local.visible_len();
                self.local.selected = step(self.local.selected, delta, len);
            }
            Focus::Remote => {
                let len = self.remote.visible_len();
                self.remote.selected = step(self.remote.selected, delta, len);
            }
            Focus::Transfers => {
                let len = self.transfers.len();
                self.transfer_selected = step(self.transfer_selected, delta, len);
            }
        }
    }

    fn select_first(&mut self) {
        match self.focus {
            Focus::Local => self.local.selected = 0,
            Focus::Remote => self.remote.selected = 0,
            Focus::Transfers => self.transfer_selected = 0,
        }
    }

    fn select_last(&mut self) {
        match self.focus {
            Focus::Local => {
                self.local.selected = self.local.visible_len().saturating_sub(1);
            }
            Focus::Remote => {
                self.remote.selected = self.remote.visible_len().saturating_sub(1);
            }
            Focus::Transfers => {
                self.transfer_selected = self.transfers.len().saturating_sub(1);
            }
        }
    }

    fn enter(&mut self) {
        match self.focus {
            Focus::Local => {
                let Some(entry) = self.local.current().cloned() else {
                    return;
                };
                if entry.is_dir {
                    self.navigate_local(entry.path);
                } else {
                    self.upload_paths(vec![entry.path]);
                }
            }
            Focus::Remote => {
                let Some(item) = self.remote.current().cloned() else {
                    return;
                };
                if item.is_dir {
                    self.navigate_remote(item.absolute_path(&self.remote.cwd));
                } else {
                    self.download_selected();
                }
            }
            Focus::Transfers => self.cancel_selected_transfer(),
        }
    }

    fn upload_paths(&mut self, paths: Vec<PathBuf>) {
        let options = UploadOptions {
            chunk_mib: self.config.file.transfer.upload_chunk_mib,
            retries: self.config.file.transfer.retries,
        };
        let dest = self.remote.cwd.clone();
        let total = paths.len();
        let mut added = 0;
        for path in paths {
            if self.enqueue_upload(path, dest.clone(), options.clone()) {
                added += 1;
            }
        }
        if added == 0 {
            self.set_status("相同上传任务已在队列中", false);
        } else if added < total {
            self.set_status(
                format!("已加入上传队列（{added}/{total} 项，其余重复已跳过）"),
                false,
            );
        } else {
            self.set_status(format!("已加入上传队列（{added} 项）"), false);
        }
    }

    fn go_parent(&mut self) {
        match self.focus {
            Focus::Local => {
                if let Some(parent) = self.local.parent() {
                    self.navigate_local(parent);
                }
            }
            Focus::Remote => {
                if matches!(self.remote.mode, RemoteMode::Search { .. }) {
                    self.clear_search();
                    return;
                }
                if self.remote.cwd != "/" {
                    let parent = remote_parent(&self.remote.cwd);
                    self.navigate_remote(parent);
                }
            }
            Focus::Transfers => {}
        }
    }

    fn navigate_local(&mut self, path: PathBuf) {
        if !path.is_dir() {
            self.set_status(format!("不是目录: {}", path.display()), true);
            return;
        }
        self.local.cwd = path;
        self.local.filter.clear();
        self.local.selected = 0;
        self.marked_local.clear();
        self.local.reload();
    }

    pub fn navigate_remote(&mut self, path: String) {
        self.remote.cwd = normalize_remote(&path);
        self.remote.mode = RemoteMode::Browse;
        self.remote.filter.clear();
        self.remote.selected = 0;
        self.remote.entries.clear();
        self.marked_remote.clear();
        self.config.file.last_remote_dir = self.remote.cwd.clone();
        self.dirty_config = true;
        self.load_remote();
    }

    fn clear_search(&mut self) {
        if matches!(self.remote.mode, RemoteMode::Search { .. }) {
            let cwd = self.remote.cwd.clone();
            self.remote.mode = RemoteMode::Browse;
            self.remote.filter.clear();
            self.remote.selected = 0;
            self.remote.cwd = cwd;
            self.load_remote();
            self.set_status("已退出搜索结果", false);
        } else if !self.remote.filter.is_empty() {
            self.remote.filter.clear();
            self.remote.clamp_selection();
        } else if !self.local.filter.is_empty() {
            self.local.filter.clear();
            self.local.clamp_selection();
        }
    }

    fn refresh_focused(&mut self) {
        match self.focus {
            Focus::Local => {
                self.local.reload();
                self.set_status("本地目录已刷新", false);
            }
            Focus::Remote | Focus::Transfers => self.refresh_remote(),
        }
    }

    pub fn refresh_remote(&mut self) {
        match self.remote.mode.clone() {
            RemoteMode::Browse => self.load_remote(),
            RemoteMode::Search { keyword, base } => self.load_search(keyword, base),
        }
    }

    fn load_remote(&mut self) {
        self.remote.loading = true;
        self.remote.error = None;
        self.generation += 1;
        let generation = self.generation;
        let cwd = self.remote.cwd.clone();
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client.list_dir(&cwd).await;
            let (items, error) = match result {
                Ok(items) => (items, None),
                Err(err) => (Vec::new(), Some(err.to_string())),
            };
            let _ = tx.send(AppEvent::RemoteLoaded {
                generation,
                items,
                error,
            });
        });
    }

    fn load_search(&mut self, keyword: String, base: String) {
        self.remote.loading = true;
        self.remote.error = None;
        self.generation += 1;
        let generation = self.generation;
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client.search(&keyword, &base, true).await;
            let (items, error) = match result {
                Ok(items) => (items, None),
                Err(err) => (Vec::new(), Some(err.to_string())),
            };
            let _ = tx.send(AppEvent::SearchLoaded {
                generation,
                keyword,
                base,
                items,
                error,
            });
        });
    }

    fn load_quota(&mut self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client.quota().await.map_err(|err| err.to_string());
            let _ = tx.send(AppEvent::Quota(result));
        });
    }

    fn load_user(&mut self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client.whoami().await.map_err(|err| err.to_string());
            let _ = tx.send(AppEvent::User(result));
        });
    }

    fn open_local(&mut self) {
        let Some(entry) = self.local.current().cloned() else {
            return;
        };
        if entry.is_dir {
            self.navigate_local(entry.path);
            return;
        }
        match std::process::Command::new("xdg-open")
            .arg(&entry.path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => self.set_status(format!("已打开: {}", entry.name), false),
            Err(err) => self.set_status(format!("打开失败: {err}"), true),
        }
    }

    fn request_quit(&mut self) {
        if self.engine.active_count() > 0 {
            self.confirm = Some(ConfirmState {
                title: "退出 pcstui".to_string(),
                message: format!(
                    "仍有 {} 个传输任务在运行，退出会取消它们。确定退出？",
                    self.engine.active_count()
                ),
                action: ConfirmAction::Quit,
            });
        } else {
            self.should_quit = true;
        }
    }

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = StatusMessage::new(text, error);
    }

    pub fn quota_line(&self) -> String {
        match &self.quota {
            Some(quota) => format!(
                "{} / {}（{:.0}%）",
                format_bytes(quota.used),
                format_bytes(quota.total),
                quota.used_ratio() * 100.0
            ),
            None => "容量查询中…".to_string(),
        }
    }

    pub fn account_name(&self) -> String {
        if let Some(user) = &self.user {
            return user.best_name().to_string();
        }
        match &self.config.file.account {
            Some(account) => account.display_name().to_string(),
            None => "未登录".to_string(),
        }
    }
}

fn default_export_path(config: &RuntimeConfig, redacted: bool) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let name = if redacted {
        format!("pcstui-redacted-{stamp}.json")
    } else {
        format!("pcstui-portable-{stamp}.json")
    };
    let home = home_dir();
    let dir = if home == Path::new(".") {
        config.paths.root.clone()
    } else {
        home
    };
    dir.join(name)
}

fn completion_target(value: &str, cwd: &str) -> (String, String) {
    let value = value.trim();
    if value.is_empty() {
        return (cwd.to_string(), String::new());
    }
    let absolute = value.starts_with('/');
    let ends_slash = value.ends_with('/');
    let normalized = if absolute {
        normalize_remote(value)
    } else {
        remote_join(cwd, value)
    };
    if ends_slash {
        return (normalized, String::new());
    }
    let parent = remote_parent(&normalized);
    let prefix = normalized.rsplit('/').next().unwrap_or("").to_string();
    (parent, prefix)
}

fn join_completed(parent: &str, completed: &str) -> String {
    let trailing = completed.ends_with('/');
    let mut value = remote_join(parent, completed);
    if trailing && !value.ends_with('/') {
        value.push('/');
    }
    value
}

fn render_completed_path(cwd: &str, parent: &str, completed: &str, absolute_input: bool) -> String {
    if absolute_input {
        return join_completed(parent, completed);
    }
    if parent == cwd {
        return completed.to_string();
    }
    let cwd = cwd.trim_end_matches('/');
    if cwd.is_empty() {
        return join_completed(parent, completed);
    }
    let prefix = format!("{cwd}/");
    if let Some(rest) = parent.strip_prefix(&prefix) {
        if rest.is_empty() {
            completed.to_string()
        } else {
            format!("{rest}/{completed}")
        }
    } else {
        join_completed(parent, completed)
    }
}

fn common_prefix(values: &[&str]) -> String {
    let mut iter = values.iter();
    let Some(first) = iter.next() else {
        return String::new();
    };
    let mut prefix = first.chars().collect::<Vec<_>>();
    for value in iter {
        let chars = value.chars().collect::<Vec<_>>();
        let mut len = 0;
        while len < prefix.len() && len < chars.len() && prefix[len] == chars[len] {
            len += 1;
        }
        prefix.truncate(len);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

fn step(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let next = current as isize + delta;
    next.clamp(0, len as isize - 1) as usize
}

/// Small helper shared with tests: a name is safe for local creation if it
/// does not escape the current directory.
pub fn safe_name(name: &str) -> String {
    sanitize_component(name)
}

pub fn local_time(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return "-".to_string();
    };
    let datetime: chrono::DateTime<chrono::Local> = time.into();
    datetime.format("%Y-%m-%d %H:%M").to_string()
}

pub fn transfer_summary(transfers: &[TransferProgress]) -> String {
    let active = transfers
        .iter()
        .filter(|transfer| !transfer.status.is_terminal())
        .count();
    if active == 0 {
        format!("{} 项", transfers.len())
    } else {
        format!("{active} 进行中 / {} 项", transfers.len())
    }
}

/// Used by the TUI status line and tests.
pub fn parse_input_path(value: &str, cwd: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("路径不能为空");
    }
    if value.starts_with('/') {
        Ok(normalize_remote(value))
    } else {
        Ok(remote_join(cwd, value))
    }
}

pub fn ensure_local_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建本地目录失败: {}", parent.display()))?;
    }
    Ok(())
}

pub fn display_error(err: &anyhow::Error) -> String {
    format!("{err:#}")
}

pub fn empty_dir_message() -> &'static str {
    "（空目录）"
}

pub fn require_path(value: &str) -> Result<&str> {
    let value = value.trim();
    if value.is_empty() {
        Err(anyhow!("路径不能为空"))
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Account, ConfigFile, Paths};

    fn test_app() -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            root: dir.path().to_path_buf(),
            config_dir: dir.path().join("config"),
            config_file: dir.path().join("config/pcstui.json"),
            download_dir: dir.path().join("downloads"),
            tmp_dir: dir.path().join("tmp"),
            upload_state_dir: dir.path().join("config/upload-state"),
            queue_file: dir.path().join("config/queue.json"),
        };
        let file = ConfigFile {
            account: Some(Account {
                uid: 1,
                name: "test".to_string(),
                bduss: "bduss".to_string(),
                stoken: "stoken".to_string(),
                baiduid: String::new(),
                access_token: String::new(),
            }),
            ..Default::default()
        };
        let config = RuntimeConfig { paths, file };
        let client = BaiduClient::new(&config).unwrap();
        let (engine, _transfer_rx) = TransferEngine::new(client.clone(), config.clone());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(config, client, engine, tx);
        // Tests must not depend on the process working directory.
        app.local.entries.clear();
        app.remote.entries.clear();
        (app, dir)
    }

    fn remote_item() -> RemoteItem {
        RemoteItem {
            fs_id: 7,
            name: "file.txt".to_string(),
            path: "/docs/file.txt".to_string(),
            size: 10,
            is_dir: false,
            ..Default::default()
        }
    }

    #[test]
    fn remote_ref_captures_and_checks_fs_id() {
        let item = remote_item();
        let reference = RemoteRef::from_item(&item, "/");
        assert_eq!(reference.fs_id, 7);
        assert_eq!(reference.path, "/docs/file.txt");
        assert!(remote_ref_matches(&reference, &item));

        let mut changed = item.clone();
        changed.fs_id = 8;
        assert!(!remote_ref_matches(&reference, &changed));

        let mut missing_id = item.clone();
        missing_id.fs_id = 0;
        assert!(!remote_ref_matches(&reference, &missing_id));
        let no_id = RemoteRef {
            fs_id: 0,
            ..reference
        };
        assert!(!remote_ref_matches(&no_id, &missing_id));
    }

    #[test]
    fn enter_cancels_destructive_confirmation() {
        let (mut app, _dir) = test_app();
        app.confirm = Some(ConfirmState {
            title: "退出".to_string(),
            message: "test".to_string(),
            action: ConfirmAction::Quit,
        });
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.confirm.is_none());
        assert!(!app.should_quit);

        app.confirm = Some(ConfirmState {
            title: "退出".to_string(),
            message: "test".to_string(),
            action: ConfirmAction::Quit,
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.confirm.is_none());
        assert!(app.should_quit);
    }

    #[test]
    fn remote_actions_are_scoped_to_remote_focus() {
        let (mut app, _dir) = test_app();
        app.remote.entries = vec![remote_item()];
        app.remote.selected = 0;

        app.focus = Focus::Transfers;
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.confirm.is_none());

        app.focus = Focus::Local;
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(app.transfers.is_empty());
        for key in ['m', 'c', 'R', 'n', 'g', 'f'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
            assert!(app.input.is_none(), "{key} should not open remote input");
        }

        app.focus = Focus::Remote;
        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        assert!(matches!(
            app.input.as_ref().map(|input| &input.kind),
            Some(InputKind::Rename { from }) if from.fs_id == 7
        ));
    }

    #[test]
    fn marking_remote_items_keeps_identity() {
        let (mut app, _dir) = test_app();
        app.focus = Focus::Remote;
        app.remote.entries = vec![remote_item()];
        app.remote.selected = 0;
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        let reference = app.marked_remote.get("/docs/file.txt").unwrap();
        assert_eq!(reference.fs_id, 7);

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        match app.confirm.as_ref().map(|confirm| &confirm.action) {
            Some(ConfirmAction::DeleteRemote(refs)) => {
                assert_eq!(refs.len(), 1);
                assert_eq!(refs[0].fs_id, 7);
                assert_eq!(refs[0].path, "/docs/file.txt");
            }
            other => panic!("unexpected confirmation: {other:?}"),
        }
    }

    #[test]
    fn step_clamps() {
        assert_eq!(step(0, -1, 5), 0);
        assert_eq!(step(4, 10, 5), 4);
        assert_eq!(step(0, 0, 0), 0);
    }

    #[test]
    fn parses_input_paths() {
        assert_eq!(parse_input_path("sub", "/apps").unwrap(), "/apps/sub");
        assert_eq!(parse_input_path("/root", "/apps").unwrap(), "/root");
        assert!(parse_input_path("  ", "/apps").is_err());
    }

    #[test]
    fn input_cursor_supports_unicode_editing() {
        let mut input = InputState::new("t", "p", "远程文件.txt".to_string(), InputKind::Goto);
        assert_eq!(input.cursor, 8);
        input.move_left();
        input.backspace();
        assert_eq!(input.value, "远程文件.tt");
        input.insert('X');
        assert_eq!(input.value, "远程文件.tXt");
        input.home();
        input.delete();
        assert_eq!(input.value, "程文件.tXt");
        input.end();
        input.insert('!');
        assert_eq!(input.value, "程文件.tXt!");
        input.move_word_left();
        assert_eq!(input.cursor, 0);
        input.delete_to_end();
        assert_eq!(input.value, "");
    }

    #[test]
    fn input_word_deletion_handles_spaces() {
        let mut input = InputState::new("t", "p", "alpha beta".to_string(), InputKind::Goto);
        input.delete_word_left();
        assert_eq!(input.value, "alpha ");
        input.delete_word_left();
        assert_eq!(input.value, "");
    }

    #[test]
    fn input_paste_supports_multibyte_and_strips_control_chars() {
        let mut input = InputState::new("t", "p", "prefix_".to_string(), InputKind::Goto);
        input.insert_text("粘贴\x1b[31m文本\t2026\n");
        assert_eq!(input.value, "prefix_粘贴[31m文本\t2026");
        assert_eq!(input.cursor, "prefix_粘贴[31m文本\t2026".chars().count());
    }

    #[test]
    fn completion_targets_and_paths() {
        assert_eq!(
            completion_target("/apps/bypy/fi", "/"),
            ("/apps/bypy".to_string(), "fi".to_string())
        );
        assert_eq!(
            completion_target("fi", "/apps"),
            ("/apps".to_string(), "fi".to_string())
        );
        assert_eq!(
            completion_target("/apps/", "/"),
            ("/apps".to_string(), String::new())
        );
        assert_eq!(
            completion_target("", "/apps"),
            ("/apps".to_string(), String::new())
        );
        assert_eq!(
            render_completed_path("/apps", "/apps", "foo/", false),
            "foo/"
        );
        assert_eq!(
            render_completed_path("/apps", "/apps/bypy", "foo/", false),
            "bypy/foo/"
        );
        assert_eq!(
            render_completed_path("/apps", "/other", "foo/", false),
            "/other/foo/"
        );
        assert_eq!(common_prefix(&["foo", "foobar", "fob"]), "fo");
        assert_eq!(common_prefix(&["远程", "远程文件"]), "远程");
    }

    #[test]
    fn local_pane_filters_hidden() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.txt"), b"x").unwrap();
        std::fs::write(dir.path().join(".hidden"), b"x").unwrap();
        let mut pane = LocalPane::new(dir.path().to_path_buf());
        pane.reload();
        assert_eq!(pane.visible_len(), 1);
        pane.show_hidden = true;
        assert_eq!(pane.visible_len(), 2);
    }

    #[tokio::test]
    async fn config_panel_scopes_keys_and_exports() {
        let (mut app, _dir) = test_app();
        app.focus = Focus::Remote;
        app.remote.entries = vec![remote_item()];
        app.remote.selected = 0;

        app.handle_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE));
        assert!(app.config_panel.is_some());
        // Destructive and focus shortcuts must not leak through the overlay.
        for key in ['x', 'u', 'p', 'R'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
            assert!(app.confirm.is_none(), "{key} should be scoped");
            assert!(app.input.is_none(), "{key} should be scoped");
            assert!(!app.show_transfers, "{key} should be scoped");
        }
        assert!(app.transfers.is_empty());

        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(matches!(
            app.input.as_ref().map(|input| &input.kind),
            Some(InputKind::ExportConfig { redacted: false })
        ));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.input.is_none());
        assert!(app.config_panel.is_some());

        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(app.config_panel.is_none());
        assert!(!app.should_quit);
    }

    #[test]
    fn config_import_replaces_only_after_confirmation_and_backs_up() {
        let (mut app, dir) = test_app();
        app.config.save().unwrap();
        let old = app.config.file.account.as_ref().unwrap().bduss.clone();

        let candidate = |app: &App| {
            let mut candidate = app.config.clone();
            candidate.set_account(Account {
                uid: 2,
                name: "imported".to_string(),
                bduss: "NEW_BDUSS".to_string(),
                stoken: "NEW_STOKEN".to_string(),
                baiduid: String::new(),
                access_token: String::new(),
            });
            candidate
        };
        let source = dir.path().join("portable.json");

        app.confirm = Some(ConfirmState {
            title: "导入配置".to_string(),
            message: "test".to_string(),
            action: ConfirmAction::ImportConfig {
                candidate: Box::new(candidate(&app)),
                source: source.clone(),
            },
        });
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.restart_requested);
        assert_eq!(
            ConfigFile::read(&app.config.paths.config_file)
                .unwrap()
                .account
                .unwrap()
                .bduss,
            old
        );

        app.confirm = Some(ConfirmState {
            title: "导入配置".to_string(),
            message: "test".to_string(),
            action: ConfirmAction::ImportConfig {
                candidate: Box::new(candidate(&app)),
                source,
            },
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.restart_requested);
        let active = ConfigFile::read(&app.config.paths.config_file).unwrap();
        assert_eq!(active.account.unwrap().bduss, "NEW_BDUSS");
        let backups = std::fs::read_dir(dir.path().join("config"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pcstui.json.bak-")
            })
            .count();
        assert_eq!(backups, 1);
    }

    #[test]
    fn relogin_requires_explicit_confirmation() {
        let (mut app, _dir) = test_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(matches!(
            app.confirm.as_ref().map(|confirm| &confirm.action),
            Some(ConfirmAction::Relogin)
        ));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.relogin_requested);

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.relogin_requested);
    }
}
