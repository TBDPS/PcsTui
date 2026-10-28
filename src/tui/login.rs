//! First-run login screen.
//!
//! The TUI must be usable on a new machine without any pre-existing config.
//! This screen collects the browser cookie (BDUSS/STOKEN) or an existing
//! `pcstui.json`, validates it online, and only then writes the config.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

use crate::api::BaiduClient;
use crate::config::{Account, RuntimeConfig, expand_tilde, normalize_secret, parse_cookie_input};

const ACCENT: Color = Color::Rgb(90, 200, 250);
const OK: Color = Color::Rgb(110, 220, 150);
const ERR: Color = Color::Rgb(250, 110, 110);
const MUTED: Color = Color::Rgb(130, 140, 150);
const BG: Color = Color::Rgb(18, 22, 26);

const FIELD_COUNT: usize = 3;
const BDUSS: usize = 0;
const STOKEN: usize = 1;
const IMPORT: usize = 2;

enum LoginOutcome {
    Success(Box<RuntimeConfig>),
    Error(String),
}

#[derive(Default)]
struct FieldState {
    value: String,
    cursor: usize,
}

impl FieldState {
    fn char_count(&self) -> usize {
        self.value.chars().count()
    }

    fn byte_at(&self, char_index: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_index)
            .map(|(byte, _)| byte)
            .unwrap_or(self.value.len())
    }

    fn insert(&mut self, ch: char) {
        let byte = self.byte_at(self.cursor);
        self.value.insert(byte, ch);
        self.cursor += 1;
    }

    fn insert_text(&mut self, text: &str) {
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            self.insert(ch);
        }
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_at(self.cursor);
        self.cursor -= 1;
        let start = self.byte_at(self.cursor);
        self.value.replace_range(start..end, "");
    }

    fn delete(&mut self) {
        if self.cursor >= self.char_count() {
            return;
        }
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(self.cursor + 1);
        self.value.replace_range(start..end, "");
    }

    fn delete_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.value.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let start_byte = self.byte_at(start);
        let end_byte = self.byte_at(self.cursor);
        self.value.replace_range(start_byte..end_byte, "");
        self.cursor = start;
    }

    fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        if self.cursor < self.char_count() {
            self.cursor += 1;
        }
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.char_count();
    }
}

struct LoginApp {
    config: RuntimeConfig,
    fields: [FieldState; FIELD_COUNT],
    focus: usize,
    status: Option<(String, bool)>,
    busy: bool,
    quit: bool,
    done: Option<RuntimeConfig>,
    tx: mpsc::UnboundedSender<LoginOutcome>,
}

impl LoginApp {
    fn new(config: RuntimeConfig, tx: mpsc::UnboundedSender<LoginOutcome>) -> Self {
        Self {
            config,
            fields: Default::default(),
            focus: BDUSS,
            status: None,
            busy: false,
            quit: false,
            done: None,
            tx,
        }
    }

    fn field(&mut self) -> &mut FieldState {
        &mut self.fields[self.focus]
    }

    fn next_field(&mut self) {
        self.focus = (self.focus + 1) % FIELD_COUNT;
    }

    fn previous_field(&mut self) {
        self.focus = (self.focus + FIELD_COUNT - 1) % FIELD_COUNT;
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Always allow escape, even while an online check is in flight.
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.quit = true;
            return;
        }
        if self.busy {
            return;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down => self.next_field(),
            KeyCode::BackTab | KeyCode::Up => self.previous_field(),
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => self.field().backspace(),
            KeyCode::Delete => self.field().delete(),
            KeyCode::Left => self.field().left(),
            KeyCode::Right => self.field().right(),
            KeyCode::Home => self.field().home(),
            KeyCode::End => self.field().end(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.field().clear()
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.field().delete_word_left()
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.field().insert(ch)
            }
            _ => {}
        }
    }

    fn handle_paste(&mut self, text: &str) {
        if !self.busy {
            self.field().insert_text(text);
        }
    }

    fn submit(&mut self) {
        if self.busy {
            return;
        }
        let import = self.fields[IMPORT].value.trim();
        if !import.is_empty() {
            self.submit_import(import.to_string());
            return;
        }

        let (bduss, stoken) = parse_fields(&self.fields[BDUSS].value, &self.fields[STOKEN].value);
        let bduss = match normalize_secret(&bduss, "BDUSS") {
            Ok(value) => value,
            Err(err) => {
                self.status = Some((format!("{err:#}"), true));
                self.focus = BDUSS;
                return;
            }
        };
        let stoken = match normalize_secret(&stoken, "STOKEN") {
            Ok(value) => value,
            Err(err) => {
                self.status = Some((format!("{err:#}"), true));
                self.focus = STOKEN;
                return;
            }
        };
        let mut config = self.config.clone();
        config.set_account(Account {
            uid: 0,
            name: String::new(),
            bduss,
            stoken,
            baiduid: String::new(),
            access_token: String::new(),
        });
        self.spawn_validation(config);
    }

    fn submit_import(&mut self, import: String) {
        let path = expand_tilde(Path::new(&import));
        let mut config = self.config.clone();
        if let Err(err) = config.adopt_file(&path) {
            self.status = Some((format!("读取配置失败: {err:#}"), true));
            return;
        }
        self.spawn_validation(config);
    }

    fn spawn_validation(&mut self, mut config: RuntimeConfig) {
        self.busy = true;
        self.status = Some(("正在验证账号…".to_string(), false));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let outcome = match BaiduClient::new(&config) {
                Ok(client) => match client.whoami().await {
                    Ok(user) => {
                        if let Some(account) = config.file.account.as_mut() {
                            account.uid = user.uk;
                            if account.name.is_empty() {
                                account.name = user.best_name().to_string();
                            }
                        }
                        match config.save_with_backup() {
                            Ok(_) => LoginOutcome::Success(Box::new(config)),
                            Err(err) => LoginOutcome::Error(format!("保存配置失败: {err:#}")),
                        }
                    }
                    Err(err) => LoginOutcome::Error(format!("登录验证失败: {err}")),
                },
                Err(err) => LoginOutcome::Error(format!("客户端初始化失败: {err}")),
            };
            let _ = tx.send(outcome);
        });
    }

    fn handle_outcome(&mut self, outcome: LoginOutcome) {
        self.busy = false;
        match outcome {
            LoginOutcome::Success(config) => {
                self.status = Some((format!("登录成功: {}", config.redacted_summary()), false));
                self.done = Some(*config);
            }
            LoginOutcome::Error(message) => self.status = Some((message, true)),
        }
    }
}

fn parse_fields(bduss_field: &str, stoken_field: &str) -> (String, String) {
    let (bduss, cookie_stoken) = parse_cookie_input(bduss_field);
    let (stoken_value, _) = parse_cookie_input(stoken_field);
    (
        bduss.unwrap_or_default(),
        cookie_stoken.or(stoken_value).unwrap_or_default(),
    )
}

pub async fn run(config: RuntimeConfig) -> Result<Option<RuntimeConfig>> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = LoginApp::new(config, tx);

    let mut terminal = crate::tui::init_login_terminal().context("初始化登录界面失败")?;
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let result = loop {
        if let Err(err) = terminal.draw(|frame| draw(frame, &app)) {
            break Err(anyhow::anyhow!("渲染登录界面失败: {err}"));
        }
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => app.handle_key(key),
                Some(Ok(Event::Paste(text))) => app.handle_paste(&text),
                Some(Ok(_)) => {}
                Some(Err(err)) => break Err(anyhow::anyhow!("读取终端事件失败: {err}")),
                None => break Ok(()),
            },
            Some(outcome) = rx.recv() => app.handle_outcome(outcome),
            _ = ticker.tick() => {}
        }
        if app.quit || app.done.is_some() {
            break Ok(());
        }
    };

    let restore = crate::tui::restore_terminal(&mut terminal);
    restore?;
    result?;
    Ok(app.done)
}

fn draw(frame: &mut Frame, app: &LoginApp) {
    let area = frame.area();
    let width = area.width.saturating_sub(4).clamp(36, 84).min(area.width);
    let height = area.height.saturating_sub(2).clamp(12, 18).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(
                " pcstui 登录 ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(BG)),
        popup,
    );

    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(inner);

    let intro = Paragraph::new(vec![
        Line::from(Span::styled(
            "浏览器登录 pan.baidu.com 后，从开发者工具复制 Cookie。",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "可直接把完整 Cookie 粘贴到 BDUSS 字段，会自动拆出 STOKEN。",
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled(
            "没有凭据？把另一台机器的 pcstui.json 路径填到第三项即可。",
            Style::default().fg(MUTED),
        )),
    ])
    .wrap(Wrap { trim: false });
    frame.render_widget(intro, chunks[0]);

    frame.render_widget(
        field_paragraph(
            "BDUSS",
            &app.fields[BDUSS],
            app.focus == BDUSS,
            true,
            "粘贴 BDUSS 或完整 Cookie",
        ),
        chunks[1],
    );
    frame.render_widget(
        field_paragraph(
            "STOKEN",
            &app.fields[STOKEN],
            app.focus == STOKEN,
            true,
            "粘贴 STOKEN（完整 Cookie 时可留空）",
        ),
        chunks[2],
    );
    frame.render_widget(
        field_paragraph(
            "配置文件",
            &app.fields[IMPORT],
            app.focus == IMPORT,
            false,
            "可选：已有 pcstui.json 的路径",
        ),
        chunks[3],
    );

    let (message, error) = app
        .status
        .clone()
        .unwrap_or_else(|| ("输入凭据后按 Enter 登录".to_string(), false));
    let color = if error {
        ERR
    } else if app.busy {
        ACCENT
    } else {
        OK
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ● ", Style::default().fg(color)),
            Span::styled(message, Style::default().fg(color)),
        ]))
        .wrap(Wrap { trim: false }),
        chunks[4],
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Tab/↑↓", Style::default().fg(ACCENT)),
            Span::raw(" 切换  "),
            Span::styled("Enter", Style::default().fg(ACCENT)),
            Span::raw(" 登录  "),
            Span::styled("Ctrl+U", Style::default().fg(ACCENT)),
            Span::raw(" 清空  "),
            Span::styled("Esc", Style::default().fg(ACCENT)),
            Span::raw(" 退出"),
        ]))
        .alignment(Alignment::Left),
        chunks[5],
    );
}

fn field_paragraph(
    label: &str,
    state: &FieldState,
    focused: bool,
    masked: bool,
    placeholder: &str,
) -> Paragraph<'static> {
    let marker = if focused { "▸ " } else { "  " };
    let label_style = if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED)
    };
    let cursor = state.cursor.min(state.char_count());
    let before: String = state.value.chars().take(cursor).collect();
    let after: String = state.value.chars().skip(cursor).collect();
    let before = if masked {
        "•".repeat(before.chars().count())
    } else {
        before
    };
    let after = if masked {
        "•".repeat(after.chars().count())
    } else {
        after
    };
    let cursor_style = Style::default().fg(Color::Black).bg(ACCENT);

    let label_width = UnicodeWidthStr::width(label);
    let label = format!("{label}{}", " ".repeat(12usize.saturating_sub(label_width)));
    let mut spans = vec![
        Span::styled(format!("{marker}{label}"), label_style),
        Span::styled(before, Style::default().fg(Color::White)),
    ];
    if state.value.is_empty() {
        spans.push(Span::styled("▌", cursor_style));
        spans.push(Span::styled(
            format!(" {placeholder}"),
            Style::default().fg(MUTED),
        ));
    } else {
        spans.push(Span::styled("▌", cursor_style));
        spans.push(Span::styled(after, Style::default().fg(Color::White)));
    }
    Paragraph::new(Line::from(spans))
}
