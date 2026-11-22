use std::path::PathBuf;
use std::time::Instant;

use crate::api::models::RemoteItem;
use crate::api::share::{ShareInput, ShareSession};
use crate::config::RuntimeConfig;
use crate::tui::panes::RemoteRef;

#[derive(Debug, Clone)]
pub struct GroupTransferBatch {
    pub msg_id: String,
    pub from_uk: u64,
    pub fs_ids: Vec<u64>,
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
    OpenShare,
    SharePassword { input: ShareInput },
    SaveShareDest {
        session: ShareSession,
        items: Vec<RemoteItem>,
        download_after: bool,
    },
    SaveGroupDest {
        gid: String,
        batches: Vec<GroupTransferBatch>,
        items: Vec<RemoteItem>,
        download_after: bool,
    },
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

    pub fn set_value(&mut self, value: String) {
        self.cursor = value.chars().count();
        self.value = value;
    }

    pub fn char_count(&self) -> usize {
        self.value.chars().count()
    }

    pub fn insert(&mut self, ch: char) {
        let byte = char_to_byte(&self.value, self.cursor);
        self.value.insert(byte, ch);
        self.cursor += 1;
    }

    pub fn insert_text(&mut self, text: &str) {
        for ch in text.chars().filter(|ch| !ch.is_control() || *ch == '\t') {
            self.insert(ch);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = char_to_byte(&self.value, self.cursor);
        let start = char_to_byte(&self.value, self.cursor - 1);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.char_count() {
            return;
        }
        let start = char_to_byte(&self.value, self.cursor);
        let end = char_to_byte(&self.value, self.cursor + 1);
        self.value.replace_range(start..end, "");
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_count());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.char_count();
    }

    pub fn move_word_left(&mut self) {
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

    pub fn move_word_right(&mut self) {
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

    pub fn delete_word_left(&mut self) {
        let old = self.cursor;
        self.move_word_left();
        let start = char_to_byte(&self.value, self.cursor);
        let end = char_to_byte(&self.value, old);
        self.value.replace_range(start..end, "");
    }

    pub fn delete_to_start(&mut self) {
        let end = char_to_byte(&self.value, self.cursor);
        self.value.replace_range(..end, "");
        self.cursor = 0;
    }

    pub fn delete_to_end(&mut self) {
        let start = char_to_byte(&self.value, self.cursor);
        self.value.truncate(start);
    }
}

pub fn char_to_byte(value: &str, char_index: usize) -> usize {
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
    pub at: Instant,
}

impl StatusMessage {
    pub fn new(text: impl Into<String>, error: bool) -> Self {
        Self {
            text: text.into(),
            error,
            at: Instant::now(),
        }
    }
}
