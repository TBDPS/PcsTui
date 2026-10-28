use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};

use crate::api::models::{format_bytes, format_speed, format_timestamp};
use crate::tui::app::{App, Focus, InputKind, RemoteMode, local_time, transfer_summary};

const ACCENT: Color = Color::Rgb(90, 200, 250);
const ACCENT_DIM: Color = Color::Rgb(45, 110, 140);
const OK: Color = Color::Rgb(110, 220, 150);
const WARN: Color = Color::Rgb(250, 200, 90);
const ERR: Color = Color::Rgb(250, 110, 110);
const MUTED: Color = Color::Rgb(130, 140, 150);
const BG_SELECTED: Color = Color::Rgb(38, 70, 88);

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let transfer_height = if app.show_transfers { 10 } else { 0 };
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(8),
        Constraint::Length(transfer_height),
        Constraint::Length(4),
    ])
    .split(area);

    draw_header(frame, chunks[0], app);
    draw_panes(frame, chunks[1], app);
    if app.show_transfers {
        draw_transfers(frame, chunks[2], app);
    }
    draw_status(frame, chunks[3], app);

    if app.help {
        draw_help(frame, area);
    }
    if let Some(input) = &app.input {
        draw_input(frame, area, input);
    }
    if let Some(confirm) = &app.confirm {
        draw_confirm(frame, area, &confirm.title, &confirm.message);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let account = app.account_name();
    let quota = app.quota_line();
    let transfers = transfer_summary(&app.transfers);
    let line1 = Line::from(vec![
        Span::styled(
            format!(" pcstui {} ", crate::VERSION),
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("账号 ", Style::default().fg(MUTED)),
        Span::styled(account, Style::default().fg(Color::White)),
        Span::raw("   "),
        Span::styled("容量 ", Style::default().fg(MUTED)),
        Span::styled(quota, Style::default().fg(OK)),
        Span::raw("   "),
        Span::styled("传输 ", Style::default().fg(MUTED)),
        Span::styled(transfers, Style::default().fg(WARN)),
    ]);
    let remote = app.remote.title_path();
    let mode = match app.focus {
        Focus::Local => "本地",
        Focus::Remote => "远程",
        Focus::Transfers => "传输",
    };
    let line2 = Line::from(vec![
        Span::styled(format!(" 焦点:{mode}  "), Style::default().fg(ACCENT_DIM)),
        Span::styled("远程 ", Style::default().fg(MUTED)),
        Span::styled(remote, Style::default().fg(Color::White)),
        Span::raw("   "),
        Span::styled("本地 ", Style::default().fg(MUTED)),
        Span::styled(
            app.local.cwd.display().to_string(),
            Style::default().fg(Color::White),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(vec![line1, line2]).style(Style::default().bg(Color::Rgb(18, 22, 26))),
        area,
    );
}

fn draw_panes(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.width >= 100 {
        let chunks = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        draw_local(frame, chunks[0], app);
        draw_remote(frame, chunks[1], app);
    } else {
        let chunks =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
        draw_local(frame, chunks[0], app);
        draw_remote(frame, chunks[1], app);
    }
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if focused {
            Style::default().fg(ACCENT)
        } else {
            Style::default().fg(Color::Rgb(70, 78, 86))
        })
        .title(Span::styled(
            title,
            if focused {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED)
            },
        ))
}

fn draw_local(frame: &mut Frame, area: Rect, app: &mut App) {
    let mut title = format!(" 本地  {} ", app.local.cwd.display());
    if !app.local.filter.is_empty() {
        title.push_str(&format!("[filter: {}] ", app.local.filter));
    }
    if app.local.show_hidden {
        title.push_str("[隐藏文件] ");
    }
    let block = pane_block(title, app.focus == Focus::Local);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if let Some(error) = &app.local.error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .style(Style::default().fg(ERR))
                .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    let indices = app.local.visible_indices();
    let rows = indices
        .iter()
        .enumerate()
        .filter_map(|(position, index)| {
            app.local.entries.get(*index).map(|entry| (position, entry))
        })
        .map(|(position, entry)| {
            let marked = app.marked_local.contains(&entry.path);
            let name = format!(
                "{}{}",
                if marked { "● " } else { "  " },
                clean_name(&entry.name)
            );
            let kind = if entry.is_dir {
                "DIR"
            } else if entry.is_symlink {
                "LNK"
            } else {
                "FILE"
            };
            let size = if entry.is_dir {
                "-".to_string()
            } else {
                format_bytes(entry.size)
            };
            Row::new(vec![
                Cell::from(quick_index(position)).style(Style::default().fg(ACCENT_DIM)),
                Cell::from(name),
                Cell::from(kind).style(Style::default().fg(if entry.is_dir {
                    ACCENT
                } else {
                    MUTED
                })),
                Cell::from(size).style(Style::default().fg(MUTED)),
                Cell::from(local_time(entry.modified)).style(Style::default().fg(MUTED)),
            ])
        })
        .collect::<Vec<_>>();

    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("（空目录）")
                .style(Style::default().fg(MUTED))
                .alignment(Alignment::Center),
            inner,
        );
        return;
    }
    app.local
        .table_state
        .select(Some(app.local.selected.min(rows.len() - 1)));
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Min(16),
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(16),
        ],
    )
    .header(
        Row::new(["#", "名称", "类型", "大小", "修改时间"])
            .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(
        Style::default()
            .bg(BG_SELECTED)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ");
    frame.render_stateful_widget(table, inner, &mut app.local.table_state);
}

fn draw_remote(frame: &mut Frame, area: Rect, app: &mut App) {
    let loading = if app.remote.loading {
        format!("{} 加载中… ", spinner(app.tick))
    } else {
        String::new()
    };
    let mut title = format!(" 远程  {}{} ", loading, app.remote.title_path());
    if !app.remote.filter.is_empty() {
        title.push_str(&format!("[filter: {}] ", app.remote.filter));
    }
    let block = pane_block(title, app.focus == Focus::Remote);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if let Some(error) = &app.remote.error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .style(Style::default().fg(ERR))
                .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    if app.remote.entries.is_empty() && app.remote.loading {
        frame.render_widget(
            Paragraph::new("正在读取远程目录…")
                .style(Style::default().fg(MUTED))
                .alignment(Alignment::Center),
            inner,
        );
        return;
    }
    let search_mode = matches!(app.remote.mode, RemoteMode::Search { .. });
    let indices = app.remote.visible_indices();
    let rows = indices
        .iter()
        .enumerate()
        .filter_map(|(position, index)| app.remote.entries.get(*index).map(|item| (position, item)))
        .map(|(position, item)| {
            let absolute = item.absolute_path(&app.remote.cwd);
            let marked = app.marked_remote.contains(&absolute);
            let name = if search_mode {
                absolute.clone()
            } else {
                item.name.clone()
            };
            let name = format!("{}{}", if marked { "● " } else { "  " }, clean_name(&name));
            let kind = if item.is_dir {
                "DIR".to_string()
            } else {
                item.extension().to_ascii_uppercase()
            };
            Row::new(vec![
                Cell::from(quick_index(position)).style(Style::default().fg(ACCENT_DIM)),
                Cell::from(name),
                Cell::from(kind).style(Style::default().fg(if item.is_dir {
                    ACCENT
                } else {
                    MUTED
                })),
                Cell::from(if item.is_dir {
                    "-".to_string()
                } else {
                    format_bytes(item.size)
                })
                .style(Style::default().fg(MUTED)),
                Cell::from(format_timestamp(item.mtime)).style(Style::default().fg(MUTED)),
            ])
        })
        .collect::<Vec<_>>();

    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("（空目录）")
                .style(Style::default().fg(MUTED))
                .alignment(Alignment::Center),
            inner,
        );
        return;
    }
    app.remote
        .table_state
        .select(Some(app.remote.selected.min(rows.len() - 1)));
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Min(18),
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(16),
        ],
    )
    .header(
        Row::new(["#", "名称", "类型", "大小", "修改时间"])
            .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(
        Style::default()
            .bg(BG_SELECTED)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ");
    frame.render_stateful_widget(table, inner, &mut app.remote.table_state);
}

fn draw_transfers(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = format!(
        " 传输队列  {}  ·  K 取消  ·  p 隐藏 ",
        transfer_summary(&app.transfers)
    );
    let block = pane_block(title, app.focus == Focus::Transfers);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.transfers.is_empty() {
        frame.render_widget(
            Paragraph::new("暂无传输任务。在本地按 u / 在远程按 d 开始传输。")
                .style(Style::default().fg(MUTED))
                .alignment(Alignment::Center),
            inner,
        );
        return;
    }
    let rows = app
        .transfers
        .iter()
        .enumerate()
        .map(|(position, transfer)| {
            let bar = progress_bar(transfer.ratio(), 13);
            let color = match transfer.status {
                crate::transfer::types::TransferStatus::Completed => OK,
                crate::transfer::types::TransferStatus::Failed => ERR,
                crate::transfer::types::TransferStatus::Cancelled => WARN,
                crate::transfer::types::TransferStatus::Hashing => WARN,
                _ => ACCENT,
            };
            Row::new(vec![
                Cell::from(quick_index(position)).style(Style::default().fg(ACCENT_DIM)),
                Cell::from(transfer.direction.arrow()).style(Style::default().fg(color)),
                Cell::from(truncate(&transfer.name, 32)),
                Cell::from(bar).style(Style::default().fg(color)),
                Cell::from(format!(
                    "{}/{}",
                    format_bytes(transfer.done),
                    format_bytes(transfer.total)
                ))
                .style(Style::default().fg(MUTED)),
                Cell::from(format_speed(transfer.speed_bps)).style(Style::default().fg(MUTED)),
                Cell::from(transfer.status.label()).style(Style::default().fg(color)),
            ])
        })
        .collect::<Vec<_>>();
    app.transfer_state
        .select(Some(app.transfer_selected.min(rows.len() - 1)));
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(18),
            Constraint::Length(15),
            Constraint::Length(18),
            Constraint::Length(10),
            Constraint::Length(8),
        ],
    )
    .header(
        Row::new(["#", "", "名称", "进度", "已传/总量", "速度", "状态"])
            .style(Style::default().fg(MUTED)),
    )
    .row_highlight_style(Style::default().bg(BG_SELECTED).fg(Color::White));
    frame.render_stateful_widget(table, inner, &mut app.transfer_state);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(area);
    let bg = Style::default().bg(Color::Rgb(18, 22, 26));
    let color = if app.status.error { ERR } else { OK };
    let line1 = Line::from(vec![
        Span::styled(" ● ", Style::default().fg(color)),
        Span::styled(app.status.text.clone(), Style::default().fg(color)),
    ]);
    frame.render_widget(Paragraph::new(line1).style(bg), chunks[0]);

    let metadata = Line::from(vec![
        Span::styled(" ◆ ", Style::default().fg(ACCENT_DIM)),
        Span::styled(app.selected_metadata(), Style::default().fg(MUTED)),
    ]);
    frame.render_widget(
        Paragraph::new(metadata).style(bg).wrap(Wrap { trim: true }),
        chunks[1],
    );

    let line2 = Line::from(vec![
        Span::styled(" Tab ", Style::default().fg(Color::Black).bg(ACCENT_DIM)),
        Span::raw("切换  "),
        Span::styled("1-99", Style::default().fg(ACCENT)),
        Span::raw(" 跳转  "),
        Span::styled("Enter", Style::default().fg(ACCENT)),
        Span::raw(" 进入/传输  "),
        Span::styled("d", Style::default().fg(ACCENT)),
        Span::raw(" 下载  "),
        Span::styled("u", Style::default().fg(ACCENT)),
        Span::raw(" 上传  "),
        Span::styled("x", Style::default().fg(ACCENT)),
        Span::raw(" 删除  "),
        Span::styled("m/c", Style::default().fg(ACCENT)),
        Span::raw(" 移动/复制  "),
        Span::styled("R", Style::default().fg(ACCENT)),
        Span::raw(" 重命名  "),
        Span::styled("/", Style::default().fg(ACCENT)),
        Span::raw(" 过滤  "),
        Span::styled("f", Style::default().fg(ACCENT)),
        Span::raw(" 搜索  "),
        Span::styled("p", Style::default().fg(ACCENT)),
        Span::raw(" 传输  "),
        Span::styled("?", Style::default().fg(ACCENT)),
        Span::raw(" 帮助  "),
        Span::styled("q", Style::default().fg(ACCENT)),
        Span::raw(" 退出"),
    ]);
    frame.render_widget(Paragraph::new(line2).style(bg), chunks[2]);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(78, 82, area);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            " pcstui 快捷键 ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![key("Tab"), text("  切换本地 / 远程 / 传输面板")]),
        Line::from(vec![key("↑ ↓ PgUp PgDn"), text("  移动选择")]),
        Line::from(vec![
            key("1-99"),
            text("  快速跳转；单位数稍候生效，两位数立即跳转"),
        ]),
        Line::from(vec![
            key("Enter / →"),
            text("  进入目录；本地文件上传；远程文件下载"),
        ]),
        Line::from(vec![key("Backspace / ←"), text("  返回上级目录")]),
        Line::from(vec![key("Space"), text("  标记 / 取消标记（批量操作）")]),
        Line::from(vec![key("d"), text("  下载远程选中项到本地当前目录")]),
        Line::from(vec![key("u"), text("  上传本地选中项到远程当前目录")]),
        Line::from(vec![key("x"), text("  删除选中项（需要确认）")]),
        Line::from(vec![key("m / c"), text("  移动 / 复制到指定远程目录")]),
        Line::from(vec![key("R"), text("  重命名远程选中项")]),
        Line::from(vec![key("n"), text("  新建远程目录")]),
        Line::from(vec![key("g"), text("  跳转到远程绝对路径")]),
        Line::from(vec![
            key("Tab"),
            text("  在跳转/移动/复制/新建/重命名输入框中补全远程路径"),
        ]),
        Line::from(vec![key("/"), text("  过滤当前面板")]),
        Line::from(vec![key("f"), text("  搜索网盘（当前目录递归）")]),
        Line::from(vec![key("s"), text("  远程排序；本地切换隐藏文件")]),
        Line::from(vec![key("h"), text("  本地显示 / 隐藏点文件")]),
        Line::from(vec![key("r"), text("  刷新当前面板")]),
        Line::from(vec![key("p"), text("  显示 / 隐藏传输队列")]),
        Line::from(vec![key("K"), text("  取消传输面板中选中的任务")]),
        Line::from(vec![key("y"), text("  在本地当前目录启动 Yazi")]),
        Line::from(vec![key("o"), text("  用 xdg-open 打开本地文件")]),
        Line::from(vec![key("?"), text("  关闭帮助")]),
        Line::from(vec![key("q"), text("  退出（有活动传输时确认）")]),
        Line::from(""),
        Line::from(Span::styled(
            "提示：多连接下载断点保存在 .<文件名>.pcstui.part 与 .state.json；上传断点保存在 config/upload-state/。",
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled(
            "待办队列保存在 config/queue.json，重启后会自动恢复未完成的传输任务。",
            Style::default().fg(MUTED),
        )),
    ];
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(ACCENT))
                .title(" 帮助 "),
        )
        .wrap(Wrap { trim: false })
        .style(Style::default().bg(Color::Rgb(20, 24, 28)));
    frame.render_widget(paragraph, popup);
}

fn draw_input(frame: &mut Frame, area: Rect, input: &crate::tui::app::InputState) {
    let popup = centered_rect_height(70, 8, area);
    frame.render_widget(Clear, popup);
    let kind_hint = match input.kind {
        InputKind::FilterLocal | InputKind::FilterRemote => "Enter 应用 · Esc 取消",
        InputKind::Search => "Enter 搜索 · Esc 取消",
        InputKind::Mkdir => "Tab 补全 · Enter 创建 · Esc 取消",
        InputKind::Goto => "Tab 补全 · Enter 跳转 · Esc 取消",
        InputKind::Rename { .. } => "Tab 补全 · Enter 确认重命名 · Esc 取消",
        InputKind::MoveDest { .. } => "Tab 补全 · Enter 移动 · Esc 取消",
        InputKind::CopyDest { .. } => "Tab 补全 · Enter 复制 · Esc 取消",
    };
    let cursor = input.cursor.min(input.value.chars().count());
    let before = input.value.chars().take(cursor).collect::<String>();
    let after = input.value.chars().skip(cursor).collect::<String>();
    let lines = vec![
        Line::from(Span::styled(
            input.prompt.clone(),
            Style::default().fg(MUTED),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("❯ ", Style::default().fg(ACCENT)),
            Span::styled(before, Style::default().fg(Color::White)),
            Span::styled("▌", Style::default().fg(ACCENT)),
            Span::styled(after, Style::default().fg(Color::White)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "←/→ 移动光标 · Home/End 行首尾 · Ctrl+W 删除词 · Delete 删除",
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled(kind_hint, Style::default().fg(MUTED))),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(ACCENT))
                    .title(format!(" {} ", input.title)),
            )
            .style(Style::default().bg(Color::Rgb(20, 24, 28))),
        popup,
    );
}

fn draw_confirm(frame: &mut Frame, area: Rect, title: &str, message: &str) {
    let popup = centered_rect_height(58, 8, area);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" y / Enter ", Style::default().fg(Color::Black).bg(ERR)),
            Span::raw(" 确认    "),
            Span::styled(
                " n / Esc ",
                Style::default().fg(Color::Black).bg(ACCENT_DIM),
            ),
            Span::raw(" 取消"),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(ERR))
                    .title(format!(" {title} ")),
            )
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(Color::Rgb(24, 20, 22))),
        popup,
    );
}

fn key(text: &str) -> Span<'static> {
    Span::styled(
        format!(" {text} "),
        Style::default().fg(Color::Black).bg(ACCENT_DIM),
    )
}

fn text(value: &str) -> Span<'static> {
    Span::raw(value.to_string())
}

fn quick_index(position: usize) -> String {
    if position < 99 {
        format!("{:>2}", position + 1)
    } else {
        "  ".to_string()
    }
}

fn progress_bar(ratio: f64, width: usize) -> String {
    let ratio = ratio.clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let mut bar = String::with_capacity(width + 2);
    bar.push('[');
    for index in 0..width {
        if index < filled {
            bar.push('█');
        } else {
            bar.push('░');
        }
    }
    bar.push(']');
    bar
}

fn spinner(tick: u64) -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    FRAMES[(tick as usize) % FRAMES.len()]
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

fn centered_rect_height(percent_x: u16, height: u16, area: Rect) -> Rect {
    let height = height.min(area.height);
    let top = area.height.saturating_sub(height) / 2;
    let vertical = Layout::vertical([
        Constraint::Length(top),
        Constraint::Length(height),
        Constraint::Min(0),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut result: String = text.chars().take(max.saturating_sub(1)).collect();
    result.push('…');
    result
}

/// File names come from the remote server and from the local filesystem.
/// Control characters would otherwise reach the terminal and could corrupt the
/// layout or inject escape sequences.
fn clean_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { '�' } else { ch })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_control_characters_from_names() {
        assert_eq!(clean_name("normal.pdf"), "normal.pdf");
        assert_eq!(clean_name("a\u{1b}[31mred"), "a�[31mred");
        assert_eq!(clean_name("line\nbreak"), "line�break");
    }
}
