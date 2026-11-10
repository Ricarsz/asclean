//! 渲染：顶栏、工作区/会话树、详情、状态行、帮助与删除确认。

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Confirm, ConfirmTarget, Row};
use crate::model::{Client, Session, WorkspaceGroup};
use crate::theme::Palette;
use crate::util;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let palette = app.palette;
    let area = frame.area();
    if area.width < 60 || area.height < 16 {
        frame.render_widget(
            Paragraph::new("终端太小（至少 60×16），请放大窗口")
                .style(Style::default().fg(palette.bad)),
            area,
        );
        return;
    }

    let [header, body, status, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(frame, app, header, palette);

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
    draw_tree(frame, app, left, palette);
    draw_details(frame, app, right, palette);
    draw_status(frame, app, status, palette);
    draw_keys(frame, app, keys, palette);

    if let Some(modal) = app.modal.clone() {
        draw_confirm(frame, app, area, &modal, palette);
    } else if app.show_help {
        draw_help(frame, app, area, palette);
    }
}

/// 客户端徽标固定占 5 列，工作区行与会话行的列才对得齐。
fn badge_text(client: Client) -> String {
    format!("{:<4} ", client.badge())
}

fn client_color(client: Client, palette: Palette) -> ratatui::style::Color {
    match client {
        Client::Dsh => palette.accent,
        Client::Codex => palette.warn,
        Client::Opencode => palette.good,
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect, palette: Palette) {
    let [left, right] =
        Layout::horizontal([Constraint::Min(20), Constraint::Length(50)]).areas(area);

    let mut title = vec![
        Span::styled(
            " asclean ",
            Style::default()
                .fg(palette.surface)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" 会话清理", Style::default().fg(palette.text)),
    ];
    if !app.filter.is_empty() {
        title.push(Span::styled(
            format!("  /{}", app.filter),
            Style::default().fg(palette.warn),
        ));
    }
    if let Some(client) = app.client_filter {
        title.push(Span::styled(
            format!("  [{}]", client.badge()),
            Style::default().fg(client_color(client, palette)),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(title)), left);

    let mut parts = vec![
        format!("{} 个工作区", app.workspaces.len()),
        format!("{} 个会话", app.visible_sessions()),
    ];
    if app.marked_count() > 0 {
        parts.push(format!(
            "已选 {} · {}",
            app.marked_count(),
            util::human_bytes(app.marked_bytes())
        ));
    }
    parts.push(util::human_bytes(app.visible_bytes()));
    if let Some(summary) = &app.bili
        && summary.known()
    {
        let mut bili = format!("bili {}", util::human_bytes(summary.bytes));
        if summary.orphans > 0 {
            bili.push_str(&format!(" · 残留 {}", summary.orphans));
        }
        parts.push(bili);
    }
    if let Some(summary) = &app.magic
        && summary.known()
    {
        let mut magic = format!("magic {} 间", summary.compartments);
        if summary.orphans > 0 {
            magic.push_str(&format!(" · 残留 {}", summary.orphans));
        }
        parts.push(magic);
    }
    if !app.warnings.is_empty() {
        parts.push(format!("{} 警告", app.warnings.len()));
    }
    frame.render_widget(
        Paragraph::new(format!("{} ", parts.join(" · ")))
            .alignment(Alignment::Right)
            .style(Style::default().fg(palette.dim)),
        right,
    );
}

fn draw_tree(frame: &mut Frame, app: &mut App, area: Rect, palette: Palette) {
    let title = if app.filter_editing {
        Line::from(vec![
            Span::styled(" 过滤工作区 / 会话 ", Style::default().fg(palette.accent)),
            Span::styled(
                format!("{}▏", app.filter),
                Style::default().fg(palette.warn),
            ),
            Span::styled(" ", Style::default()),
        ])
    } else if !app.filter.is_empty() {
        Line::from(format!(" 工作区 · /{} ", app.filter))
    } else if let Some(client) = app.client_filter {
        Line::from(format!(" 工作区 · {} ", client.label()))
    } else {
        Line::from(" 工作区 / 会话 ")
    };

    let inner_width = area.width.saturating_sub(2) as usize;
    app.list_area = area;

    if app.rows.is_empty() {
        let hint = if app.sessions.is_empty() {
            " 没有发现任何会话；按 r 重新扫描"
        } else {
            " 没有匹配的会话；按 / 调整过滤条件"
        };
        frame.render_widget(
            Paragraph::new(hint)
                .style(Style::default().fg(palette.dim))
                .block(panel(title, palette)),
            area,
        );
        return;
    }

    // 高亮符号占 2 列。
    let width = inner_width.saturating_sub(2);
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| {
            let line = match *row {
                Row::Workspace { group } => app
                    .group_at(group)
                    .map(|group| workspace_line(group, app.is_collapsed(group), width, palette))
                    .unwrap_or_else(|| Line::from("")),
                Row::Session { session, .. } => match app.sessions.get(session) {
                    Some(session) => session_line(
                        session,
                        app.marked.contains(&session.key()),
                        is_last_child(app, *row),
                        width,
                        palette,
                    ),
                    None => Line::from(""),
                },
            };
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(panel(title, palette))
        .highlight_symbol("▌ ")
        .highlight_style(
            Style::default()
                .bg(palette.selected)
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        );

    let mut state = ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(list, area, &mut state);
    app.list_offset = state.offset();
}

/// 这条会话是否是所在工作区的最后一行（决定树枝用 `└─` 还是 `├─`）。
fn is_last_child(app: &App, row: Row) -> bool {
    let Row::Session { group, .. } = row else {
        return false;
    };
    let position = app.rows.iter().position(|item| *item == row);
    let Some(position) = position else {
        return false;
    };
    match app.rows.get(position + 1) {
        Some(Row::Session { group: next, .. }) => *next != group,
        _ => true,
    }
}

/// 工作区行：折叠标记、客户端、标题，右侧是会话数、总大小与最近更新。
fn workspace_line(
    group: &WorkspaceGroup,
    collapsed: bool,
    width: usize,
    palette: Palette,
) -> Line<'static> {
    let marker = if collapsed { "▸ " } else { "▾ " };
    let badge = badge_text(group.client);
    let stats = format!(
        "{} 个 · {} · {}",
        group.count(),
        group.size_label(),
        group.age_label()
    );
    let used = 2 + 5 + UnicodeWidthStr::width(stats.as_str());
    let title = util::truncate(group.title(), width.saturating_sub(used + 1).max(6));
    let title_width = UnicodeWidthStr::width(title.as_str());
    let fill = width.saturating_sub(used + title_width + 1);

    Line::from(vec![
        Span::styled(
            marker.to_string(),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            badge,
            Style::default()
                .fg(client_color(group.client, palette))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            title,
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{}{stats}", " ".repeat(fill)),
            Style::default().fg(palette.dim),
        ),
    ])
}

/// 会话行：缩进的树枝 + 勾选位 + 时间、客户端、标题、大小；有 bili 数据时加标记。
fn session_line(
    session: &Session,
    marked: bool,
    last: bool,
    width: usize,
    palette: Palette,
) -> Line<'static> {
    let branch = if last { "  └─ " } else { "  ├─ " };
    let (mark, mark_color) = if marked {
        ("✓ ", palette.good)
    } else {
        ("· ", palette.faint)
    };
    let time = format!("{}  ", session.time_label());
    let badge = badge_text(session.client);
    let flag = if session.subagent {
        "子 "
    } else if session.bili.is_some() || session.magic.is_some() {
        "• "
    } else {
        "  "
    };
    // 树枝 5 + 勾选 2 + 时间 13 + badge 5 + 附带标记 2，再加大小列 7。
    let used = 5 + 2 + 13 + 5 + 2;
    let title = util::truncate(&session.title, width.saturating_sub(used + 8).max(6));
    let title_width = UnicodeWidthStr::width(title.as_str());
    let size = util::human_bytes(session.total_bytes());
    let size_width = UnicodeWidthStr::width(size.as_str());
    let fill = width.saturating_sub(used + title_width + size_width + 1);

    Line::from(vec![
        Span::styled(branch.to_string(), Style::default().fg(palette.faint)),
        Span::styled(mark.to_string(), Style::default().fg(mark_color)),
        Span::styled(time, Style::default().fg(palette.dim)),
        Span::styled(
            badge,
            Style::default()
                .fg(client_color(session.client, palette))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            flag.to_string(),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(title.clone(), Style::default().fg(palette.text)),
        Span::styled(
            format!("{}{size}", " ".repeat(fill)),
            Style::default().fg(palette.dim),
        ),
    ])
}

fn draw_details(frame: &mut Frame, app: &mut App, area: Rect, palette: Palette) {
    app.detail_area = area;
    let block = panel(" 详情 ", palette);
    let inner = block.inner(area);
    let lines = app.details();
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = paragraph.line_count(inner.width) as u16;
    let max_scroll = total.saturating_sub(inner.height);
    app.detail_max = max_scroll;
    if app.detail_scroll > max_scroll {
        app.detail_scroll = max_scroll;
    }

    frame.render_widget(paragraph.block(block).scroll((app.detail_scroll, 0)), area);

    if max_scroll > 0 && !app.show_help && app.modal.is_none() {
        let mut state = ScrollbarState::new(total as usize)
            .viewport_content_length(inner.height as usize)
            .position(app.detail_scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(palette.accent))
                .track_style(Style::default().fg(palette.faint)),
            area,
            &mut state,
        );
    }
}

impl App {
    pub fn details(&self) -> Vec<Line<'static>> {
        match self.current_row() {
            Some(Row::Session { .. }) => self.session_details(),
            Some(Row::Workspace { .. }) => self.workspace_details(),
            None => vec![Line::from(Span::styled(
                " 没有选中任何行",
                Style::default().fg(self.palette.dim),
            ))],
        }
    }

    fn session_details(&self) -> Vec<Line<'static>> {
        let palette = self.palette;
        let Some(session) = self.current() else {
            return vec![Line::from(Span::styled(
                " 没有选中会话",
                Style::default().fg(palette.dim),
            ))];
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(kv(
            palette,
            "客户端",
            format!("{}（{}）", session.client.label(), session.client.key()),
        ));
        let (workspace_text, workspace_color) = match &session.workspace {
            Some(workspace) => (
                format!("{}（{}）", workspace.title, workspace.path),
                palette.text,
            ),
            None => ("未识别 · 归入未知组".to_string(), palette.warn),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {}  ", pad("工作区", 8)),
                Style::default().fg(palette.dim),
            ),
            Span::styled(workspace_text, Style::default().fg(workspace_color)),
        ]));
        lines.push(kv(palette, "标题", session.title.clone()));
        lines.push(kv(palette, "标识", session.id.clone()));
        if session.subagent {
            let text = match &session.parent {
                Some(parent) => format!("子会话 · 父 {parent}"),
                None => "子会话".to_string(),
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {}  ", pad("来源", 8)),
                    Style::default().fg(palette.dim),
                ),
                Span::styled(text, Style::default().fg(palette.warn)),
            ]));
        }
        if let Some(cwd) = &session.cwd {
            lines.push(kv(palette, "工作目录", cwd.clone()));
        }
        lines.push(kv(palette, "存储位置", session.path.display().to_string()));
        lines.push(kv(palette, "根目录", session.root.display().to_string()));
        lines.push(Line::from(""));
        lines.push(kv(
            palette,
            "创建",
            session
                .created
                .map(util::local_datetime)
                .unwrap_or_else(|| "--".to_string()),
        ));
        lines.push(kv(
            palette,
            "更新",
            format!(
                "{}（{}）",
                session
                    .updated
                    .map(util::local_datetime)
                    .unwrap_or_else(|| "--".to_string()),
                session.age_label()
            ),
        ));
        lines.push(kv(
            palette,
            "大小",
            format!(
                "{} · {} 条消息",
                util::human_bytes(session.bytes),
                session.messages
            ),
        ));

        if let Some(meta) = &session.bili {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  billion-context",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(kv(
                palette,
                "附带数据",
                format!(
                    "{} · {} 条消息 · {}",
                    util::human_bytes(meta.bytes),
                    meta.messages,
                    meta.status
                ),
            ));
            lines.push(kv(
                palette,
                "最后更新",
                meta.updated
                    .map(util::local_datetime)
                    .unwrap_or_else(|| "--".to_string()),
            ));
            lines.push(kv(palette, "数据库", meta.db.display().to_string()));
            lines.push(Line::from(Span::styled(
                "  删除本会话时会一并清理这些数据。",
                Style::default().fg(palette.dim),
            )));
        }

        if let Some(meta) = &session.magic {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  magic-context",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(kv(
                palette,
                "隔间",
                format!("{} 个 · {}", meta.compartments, meta.status),
            ));
            lines.push(kv(
                palette,
                "最后活动",
                meta.updated
                    .map(util::local_datetime)
                    .unwrap_or_else(|| "--".to_string()),
            ));
            lines.push(kv(palette, "数据库", meta.db.display().to_string()));
            lines.push(Line::from(Span::styled(
                "  删除本会话时会一并清理这些记录。",
                Style::default().fg(palette.dim),
            )));
        }

        if let Some((input, output)) = session.tokens {
            lines.push(kv(
                palette,
                "令牌",
                format!(
                    "输入 {} · 输出 {}",
                    util::thousands(input),
                    util::thousands(output)
                ),
            ));
        }
        if let Some(cost) = session.cost {
            lines.push(kv(palette, "费用", format!("${cost:.4}")));
        }

        let (state_text, state_color) = if session.locked {
            ("使用中 · 会话锁被占用，删除会被跳过", palette.warn)
        } else {
            ("空闲", palette.good)
        };
        lines.push(state_line(palette, state_text, state_color));

        if !session.preview.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  首条消息",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            )));
            for text in wrap_plain(&session.preview, 46) {
                lines.push(Line::from(Span::styled(
                    format!("  {text}"),
                    Style::default().fg(palette.dim),
                )));
            }
        }
        lines
    }

    fn workspace_details(&self) -> Vec<Line<'static>> {
        let palette = self.palette;
        let Some(group) = self.current_group() else {
            return vec![Line::from(Span::styled(
                " 没有选中工作区",
                Style::default().fg(palette.dim),
            ))];
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(kv(
            palette,
            "客户端",
            format!("{}（{}）", group.client.label(), group.client.key()),
        ));
        lines.push(kv(palette, "工作区", group.title().to_string()));
        lines.push(kv(palette, "标识", group.key.clone()));
        lines.push(kv(
            palette,
            "路径",
            group.path().unwrap_or("--").to_string(),
        ));
        lines.push(Line::from(""));
        lines.push(kv(palette, "会话", format!("{} 个", group.count())));
        lines.push(kv(palette, "大小", group.size_label()));
        lines.push(kv(palette, "消息", format!("{} 条", group.messages)));
        lines.push(kv(
            palette,
            "更新",
            format!("{}（{}）", group.time_label(), group.age_label()),
        ));
        let bili_bytes: u64 = group
            .indices
            .iter()
            .filter_map(|index| self.sessions.get(*index))
            .map(Session::bili_bytes)
            .sum();
        if bili_bytes > 0 {
            let linked = group
                .indices
                .iter()
                .filter_map(|index| self.sessions.get(*index))
                .filter(|session| session.bili.is_some())
                .count();
            lines.push(kv(
                palette,
                "bili 附带",
                format!("{} · {} 个会话", util::human_bytes(bili_bytes), linked),
            ));
        }
        let magic: Vec<&Session> = group
            .indices
            .iter()
            .filter_map(|index| self.sessions.get(*index))
            .filter(|session| session.magic.is_some())
            .collect();
        if !magic.is_empty() {
            let compartments: u32 = magic
                .iter()
                .map(|session| session.magic_compartments())
                .sum();
            lines.push(kv(
                palette,
                "magic",
                format!("{compartments} 个隔间 · {} 个会话", magic.len()),
            ));
        }
        let (state_text, state_color) = if group.locked > 0 {
            (
                format!("{} 个会话正在使用，删除会被跳过", group.locked),
                palette.warn,
            )
        } else {
            ("空闲".to_string(), palette.good)
        };
        lines.push(state_line(palette, &state_text, state_color));
        if let Some(workspace) = &group.workspace {
            lines.push(Line::from(""));
            lines.push(kv(palette, "存储桶", workspace.root.display().to_string()));
            lines.push(kv(
                palette,
                "登记",
                if workspace.is_synthetic() {
                    "由会话目录推断（未找到客户端登记）".to_string()
                } else {
                    "来自客户端登记".to_string()
                },
            ));
        } else {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  这些会话没有工作区信息，只能逐个删除。",
                Style::default().fg(palette.warn),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Enter / ←/→ 折叠或展开 · d 删除工作区",
            Style::default().fg(palette.accent),
        )));
        lines.push(Line::from(Span::styled(
            "  删除只清理会话存储与登记，不动项目目录。",
            Style::default().fg(palette.dim),
        )));
        lines
    }
}

fn kv(palette: Palette, label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {}  ", pad(label, 8)),
            Style::default().fg(palette.dim),
        ),
        Span::styled(value, Style::default().fg(palette.text)),
    ])
}

fn state_line<'a>(palette: Palette, text: &str, color: ratatui::style::Color) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("  {}  ", pad("状态", 8)),
            Style::default().fg(palette.dim),
        ),
        Span::styled(text.to_string(), Style::default().fg(color)),
    ])
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect, palette: Palette) {
    let (text, kind) = app.status_line();
    let color = match kind {
        crate::app::NoticeKind::Info => palette.dim,
        crate::app::NoticeKind::Ok => palette.good,
        crate::app::NoticeKind::Warn => palette.warn,
        crate::app::NoticeKind::Bad => palette.bad,
    };
    frame.render_widget(
        Paragraph::new(Span::styled(format!(" {text}"), Style::default().fg(color))),
        area,
    );
}

fn draw_keys(frame: &mut Frame, app: &App, area: Rect, palette: Palette) {
    let mut hints: Vec<&str> = vec![
        "↑↓ 选择",
        "Enter 折叠",
        "←→ 展开",
        "空格 勾选",
        "a 全选",
        "d 删除",
        "X 清理残留",
        "Tab 客户端",
        "/ 过滤",
        "r 重扫",
        "t 主题",
        "? 帮助",
        "q 退出",
    ];
    if app.marked_count() > 0 {
        hints.insert(5, "Esc 清除");
    }
    let mut line = String::from(" ");
    for hint in hints {
        if UnicodeWidthStr::width(line.as_str()) + UnicodeWidthStr::width(hint) + 3
            > area.width as usize
        {
            break;
        }
        if line.len() > 1 {
            line.push_str("   ");
        }
        line.push_str(hint);
    }
    frame.render_widget(
        Paragraph::new(line).style(Style::default().fg(palette.faint)),
        area,
    );
}

/// 清理残留的确认框：数字来自 `gc::run(true)` 的演练结果。
fn draw_cleanup_confirm(
    frame: &mut Frame,
    area: Rect,
    report: &crate::gc::GcReport,
    palette: Palette,
) {
    let mut body = vec![
        Line::from(Span::styled(
            "清理已删除会话留下的残留记录：",
            Style::default().fg(palette.text),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("  工作区登记        {} 条", report.registry),
            Style::default().fg(palette.text),
        )),
        Line::from(Span::styled(
            format!("  渲染缓存          {} 个", report.projcache),
            Style::default().fg(palette.text),
        )),
        Line::from(Span::styled(
            format!(
                "  billion-context   {} 条（{}）",
                report.bili_records,
                util::human_bytes(report.bili_bytes)
            ),
            Style::default().fg(palette.text),
        )),
    ];
    for warning in report.warnings.iter().take(3) {
        body.push(Line::from(Span::styled(
            format!("  注意：{warning}"),
            Style::default().fg(palette.warn),
        )));
    }
    body.push(Line::from(""));
    body.push(Line::from(Span::styled(
        "只动索引与记录，不会删除仍在的会话；删除后不可恢复。",
        Style::default().fg(palette.dim),
    )));

    let width = 58u16.min(area.width.saturating_sub(4));
    let height = (body.len() as u16 + 4).min(area.height.saturating_sub(2));
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Line::from(vec![
            Span::styled("▍", Style::default().fg(palette.warn)),
            Span::styled(" 清理残留 ", Style::default().fg(palette.text)),
        ]))
        .border_style(Style::default().fg(palette.warn));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [content, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(palette.text)),
        content,
    );
    frame.render_widget(
        Paragraph::new(" Enter / y 清理    Esc / n 取消").style(Style::default().fg(palette.dim)),
        footer,
    );
}

fn draw_confirm(frame: &mut Frame, app: &App, area: Rect, modal: &Confirm, palette: Palette) {
    if let ConfirmTarget::Cleanup(report) = &modal.target {
        draw_cleanup_confirm(frame, area, report, palette);
        return;
    }
    let sessions: Vec<&Session> = app.confirm_sessions(&modal.target);
    let group = app.confirm_workspace(&modal.target);
    let bytes: u64 = if let Some(group) = group {
        group.bytes
    } else {
        sessions.iter().map(|session| session.total_bytes()).sum()
    };
    let bili_bytes: u64 = sessions.iter().map(|session| session.bili_bytes()).sum();
    let magic_compartments: u32 = sessions
        .iter()
        .map(|session| session.magic_compartments())
        .sum();
    let locked = sessions.iter().filter(|session| session.locked).count();
    let deleting_workspace = group.is_some();

    let mut body = vec![Line::from(Span::styled(
        if let Some(group) = group {
            format!(
                "将删除工作区「{}」的 {} 个会话，释放 {}：",
                util::truncate(group.title(), 20),
                sessions.len(),
                util::human_bytes(bytes)
            )
        } else {
            format!(
                "将永久删除 {} 个会话，释放 {}：",
                sessions.len(),
                util::human_bytes(bytes)
            )
        },
        Style::default().fg(palette.text),
    ))];
    if bili_bytes > 0 {
        body.push(Line::from(Span::styled(
            format!(
                "  其中 billion-context 数据 {}（一并清理）",
                util::human_bytes(bili_bytes)
            ),
            Style::default().fg(palette.warn),
        )));
    }
    if magic_compartments > 0 {
        body.push(Line::from(Span::styled(
            format!("  其中 magic-context 记录 {magic_compartments} 个隔间（一并清理）"),
            Style::default().fg(palette.warn),
        )));
    }
    body.push(Line::from(""));
    for session in sessions.iter().take(8) {
        body.push(Line::from(vec![
            Span::styled(
                format!("  {} ", session.client.badge()),
                Style::default().fg(client_color(session.client, palette)),
            ),
            Span::styled(
                util::truncate(&session.title, 46),
                Style::default().fg(palette.text),
            ),
        ]));
    }
    if sessions.len() > 8 {
        body.push(Line::from(Span::styled(
            format!("  … 还有 {} 个", sessions.len() - 8),
            Style::default().fg(palette.dim),
        )));
    }
    body.push(Line::from(""));
    if locked > 0 {
        body.push(Line::from(Span::styled(
            format!("{locked} 个会话正在使用，会被跳过。"),
            Style::default().fg(palette.warn),
        )));
    }
    if deleting_workspace {
        body.push(Line::from(Span::styled(
            "只清理会话存储与登记，项目目录不受影响；删除后不可恢复。",
            Style::default().fg(palette.dim),
        )));
    } else {
        body.push(Line::from(Span::styled(
            "删除后不可恢复。",
            Style::default().fg(palette.dim),
        )));
    }

    let hint = " Enter / y 确认    Esc / n 取消";
    let width = if deleting_workspace { 62 } else { 56 };
    let width = width.min(area.width.saturating_sub(4) as usize) as u16;
    let height = (body.len() as u16 + 4).min(area.height.saturating_sub(2));
    let popup = centered(area, width, height);

    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Line::from(vec![
            Span::styled("▍", Style::default().fg(palette.bad)),
            Span::styled(
                if deleting_workspace {
                    " 删除工作区 "
                } else {
                    " 删除会话 "
                },
                Style::default().fg(palette.text),
            ),
        ]))
        .border_style(Style::default().fg(palette.bad));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [content, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(palette.text)),
        content,
    );
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(palette.dim)),
        footer,
    );
}

fn draw_help(frame: &mut Frame, app: &mut App, area: Rect, palette: Palette) {
    let mut lines = vec![
        Line::from(Span::styled(
            "按键",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  ↑ ↓ / k j         在树上移动（含工作区标题行）"),
        Line::from("  PgUp / PgDn       翻 10 行"),
        Line::from("  Home / End        跳到首尾"),
        Line::from("  Enter             折叠 / 展开当前工作区"),
        Line::from("  ← / h             折叠（会话行会跳回它的标题行）"),
        Line::from("  → / l             展开当前工作区"),
        Line::from("  空格              会话：勾选并下移；工作区：整组勾选 / 取消"),
        Line::from("  a                 全选可见会话 / 取消全选"),
        Line::from("  d                 删除（有勾选删勾选；工作区行删整个工作区）"),
        Line::from("  X                 清理残留（目录已删，登记/缓存/bili 还留着的记录）"),
        Line::from("  Tab / Shift+Tab   切换客户端视图（全部 → DSH → Codex → Opencode）"),
        Line::from("  /                 按标题、ID、目录、工作区过滤"),
        Line::from("  J / K（详情）     滚动详情（PgUp/PgDn 也行）"),
        Line::from("  鼠标             左栏滚轮移动光标、点击选中；再点工作区行折叠"),
        Line::from("                   右栏滚轮翻详情；点空白处不清勾选"),
        Line::from("  r                 立即重新扫描"),
        Line::from("  t                 切换主题"),
        Line::from("  ?                 显示 / 隐藏本帮助"),
        Line::from("  q / Esc           退出（先清过滤，再清勾选）"),
        Line::from(""),
        Line::from(Span::styled(
            "工作区与附带数据",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "扫描位置",
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    )));
    for root in &app.roots {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", root.client.badge()),
                Style::default().fg(client_color(root.client, palette)),
            ),
            Span::styled(
                root.path.display().to_string(),
                Style::default().fg(palette.dim),
            ),
            Span::styled(
                format!("  {} 个", root.sessions),
                Style::default().fg(palette.faint),
            ),
        ]));
        if let Some(error) = &root.error {
            lines.push(Line::from(Span::styled(
                format!("    {error}"),
                Style::default().fg(palette.bad),
            )));
        }
    }
    for warning in app.warnings.iter().take(4) {
        lines.push(Line::from(Span::styled(
            format!("  ! {}", util::truncate(warning, 70)),
            Style::default().fg(palette.warn),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "工作区：DSH 读 workspace.json 登记并按会话 ID 归属；Codex 按会话 cwd 推断；",
        Style::default().fg(palette.dim),
    )));
    lines.push(Line::from(Span::styled(
        "Opencode 读 project 表。工作区删除只清会话存储与登记，不动项目目录。",
        Style::default().fg(palette.dim),
    )));
    lines.push(Line::from(Span::styled(
        "会话删除语义：DSH 删除会话目录（占用锁时跳过）；Codex 删除 rollout 文件并清理索引；",
        Style::default().fg(palette.dim),
    )));
    lines.push(Line::from(Span::styled(
        "Opencode 在 SQLite 事务中删除会话及其消息。",
        Style::default().fg(palette.dim),
    )));
    lines.push(Line::from(Span::styled(
        "billion-context：DSH 会话行带 • 标记；删除 DSH 会话时按同名 session_id",
        Style::default().fg(palette.dim),
    )));
    lines.push(Line::from(Span::styled(
        "连带清理 ~/.local/share/billion-context/sessions/index.db，并回收无人引用的 content。",
        Style::default().fg(palette.dim),
    )));
    lines.push(Line::from(Span::styled(
        "magic-context：Opencode 会话行带 • 标记；删除时按同名 session_id 清掉",
        Style::default().fg(palette.dim),
    )));
    lines.push(Line::from(Span::styled(
        "~/.local/share/cortexkit/magic-context/context.db 里的全部记录。",
        Style::default().fg(palette.dim),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ? / Esc 关闭",
        Style::default().fg(palette.faint),
    )));

    let body_width = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    let width = (body_width + 4).min(area.width.saturating_sub(4) as usize) as u16;
    let popup = centered(
        area,
        width,
        (lines.len() as u16 + 2).min(area.height.saturating_sub(2)),
    );
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Line::from(vec![
            Span::styled("▍", Style::default().fg(palette.accent)),
            Span::styled(" 帮助 ", Style::default().fg(palette.text)),
        ]))
        .border_style(Style::default().fg(palette.accent));
    let inner = block.inner(popup);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = paragraph.line_count(inner.width) as u16;
    let max_scroll = total.saturating_sub(inner.height);
    if app.help_scroll > max_scroll {
        app.help_scroll = max_scroll;
    }
    frame.render_widget(paragraph.block(block).scroll((app.help_scroll, 0)), popup);
    if max_scroll > 0 {
        let mut state = ScrollbarState::new(total as usize)
            .viewport_content_length(inner.height as usize)
            .position(app.help_scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(palette.accent))
                .track_style(Style::default().fg(palette.faint)),
            popup,
            &mut state,
        );
    }
}

fn panel<'a>(title: impl Into<Line<'a>>, palette: Palette) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(palette.border))
        .title_style(Style::default().fg(palette.dim))
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// 按显示宽度右侧补空格。
fn pad(text: &str, width: usize) -> String {
    let current = UnicodeWidthStr::width(text);
    if current >= width {
        return text.to_string();
    }
    format!("{text}{}", " ".repeat(width - current))
}

/// 简单按宽度折行，供预览文本使用。
fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if current_width + w > width && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += w;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pads_by_display_width() {
        assert_eq!(pad("abc", 5), "abc  ");
        assert_eq!(pad("中文", 6), "中文  ");
        assert_eq!(pad("abcdef", 3), "abcdef");
    }

    #[test]
    fn wraps_on_width() {
        let lines = wrap_plain("abcdefghij", 4);
        assert_eq!(lines, vec!["abcd", "efgh", "ij"]);
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::app::{App, Confirm, ConfirmTarget, NoticeKind};
    use crate::config::Config;
    use crate::model::{BiliMeta, Client, MagicMeta, Session, Workspace};
    use crate::scan::{Root, Snapshot};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn session(client: Client, id: &str, title: &str) -> Session {
        Session {
            client,
            id: id.to_string(),
            title: title.to_string(),
            preview: "想把这些会话清理掉，顺便验证一下界面。".to_string(),
            path: PathBuf::from(format!("/tmp/{id}")),
            root: PathBuf::from("/tmp"),
            cwd: Some("/workspace/projects".to_string()),
            workspace: None,
            created: Some(SystemTime::now() - Duration::from_secs(7200)),
            updated: Some(SystemTime::now() - Duration::from_secs(60)),
            bytes: 1_048_576,
            messages: 42,
            tokens: Some((123_456, 7_890)),
            cost: Some(0.25),
            locked: false,
            bili: None,
            subagent: false,
            parent: None,
            magic: None,
        }
    }

    fn workspace(client: Client, id: &str, path: &str, title: &str) -> Workspace {
        Workspace {
            client,
            id: id.to_string(),
            path: path.to_string(),
            title: title.to_string(),
            root: PathBuf::from("/tmp"),
        }
    }

    fn sample_app() -> App {
        let mut codex = session(
            Client::Codex,
            "codex-1",
            "这是一个很长很长的标题，用来观察列表里的截断行为是否正常",
        );
        codex.locked = true;
        let mut dsh = session(Client::Dsh, "dsh-1", "清理 DSH 会话");
        dsh.workspace = Some(workspace(
            Client::Dsh,
            "ws-1",
            "/workspace/projects",
            "projects",
        ));
        dsh.bili = Some(BiliMeta {
            db: PathBuf::from("/tmp/index.db"),
            bytes: 2_097_152,
            messages: 233,
            updated: Some(SystemTime::now()),
            status: "active".into(),
        });
        let mut oc = session(Client::Opencode, "oc-1", "验证 OpenCode 接入");
        oc.workspace = Some(workspace(
            Client::Opencode,
            "proj-1",
            "/workspace/projects",
            "projects",
        ));
        oc.magic = Some(MagicMeta {
            db: PathBuf::from("/tmp/context.db"),
            compartments: 3,
            updated: Some(SystemTime::now()),
            status: "opencode2".into(),
        });
        let snapshot = Snapshot {
            sessions: vec![dsh, oc, codex],
            workspaces: vec![
                workspace(Client::Dsh, "ws-1", "/workspace/projects", "projects"),
                workspace(Client::Dsh, "ws-empty", "/workspace/empty", "empty"),
            ],
            roots: vec![Root {
                client: Client::Dsh,
                path: PathBuf::from("/var/lib/dsh"),
                sessions: 1,
                error: None,
            }],
            warnings: Vec::new(),
            bili: None,
            magic: None,
        };
        let mut app = App::from_snapshot(snapshot, Config::default());
        app.notice = crate::app::Notice {
            text: "3 个会话 · 就绪".to_string(),
            kind: NoticeKind::Info,
        };
        app
    }

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("draw for {width}x{height}");
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..height {
            let mut x = 0;
            while x < width {
                let symbol = buffer[(x, y)].symbol();
                out.push_str(symbol);
                // 宽字符占两列，跳过它的续列，否则导出文本里会多出空格。
                let advance = symbol
                    .chars()
                    .next()
                    .and_then(|ch| ch.width())
                    .unwrap_or(1)
                    .max(1) as u16;
                x += advance;
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_tree_header_details_and_keys() {
        let mut app = sample_app();
        // 选中带 bili 数据的 DSH 会话，详情里才会出现 billion-context 段。
        app.selected = app
            .rows
            .iter()
            .position(|row| match row {
                Row::Session { session, .. } => app.sessions[*session].bili.is_some(),
                Row::Workspace { .. } => false,
            })
            .expect("DSH 会话行");
        let text = render(&mut app, 120, 34);
        assert!(text.contains("asclean"), "{text}");
        assert!(text.contains("清理 DSH 会话"), "{text}");
        assert!(text.contains("验证 OpenCode 接入"), "{text}");
        assert!(text.contains("projects"), "{text}");
        assert!(text.contains("▾"), "{text}");
        assert!(text.contains("├─") || text.contains("└─"), "{text}");
        assert!(text.contains("存储位置"), "{text}");
        assert!(text.contains("billion-context"), "{text}");
        assert!(text.contains("空格"), "{text}");
    }

    #[test]
    fn renders_at_small_sizes_without_panicking() {
        for (width, height) in [(80, 24), (60, 16), (30, 8), (20, 5)] {
            let mut app = sample_app();
            let _ = render(&mut app, width, height);
        }
    }

    #[test]
    fn renders_magic_context_details_and_confirm_line() {
        let mut app = sample_app();
        app.selected = app
            .rows
            .iter()
            .position(|row| match row {
                Row::Session { session, .. } => app.sessions[*session].magic.is_some(),
                Row::Workspace { .. } => false,
            })
            .expect("Opencode 会话行");
        let text = render(&mut app, 120, 34);
        assert!(text.contains("magic-context"), "{text}");
        assert!(text.contains("3 个 · opencode2"), "{text}");

        app.modal = Some(Confirm {
            target: ConfirmTarget::Sessions(vec![0, 1, 2]),
            dry_run: false,
        });
        let text = render(&mut app, 100, 30);
        assert!(text.contains("其中 magic-context 记录 3 个隔间"), "{text}");
    }

    #[test]
    fn renders_session_confirm_with_bili_line() {
        let mut app = sample_app();
        app.modal = Some(Confirm {
            target: ConfirmTarget::Sessions(vec![0, 1, 2]),
            dry_run: false,
        });
        let text = render(&mut app, 100, 30);
        assert!(text.contains("删除会话"), "{text}");
        assert!(text.contains("将永久删除 3 个会话"), "{text}");
        assert!(text.contains("其中 billion-context 数据"), "{text}");
        assert!(text.contains("Enter / y 确认"), "{text}");
        assert!(text.contains("1 个会话正在使用"), "{text}");
    }

    #[test]
    fn renders_workspace_row_details_and_confirm() {
        let mut app = sample_app();
        // 选中第一个工作区标题行。
        app.selected = 0;
        let text = render(&mut app, 120, 34);
        assert!(text.contains("工作区"), "{text}");
        assert!(text.contains("projects"), "{text}");
        assert!(text.contains("empty"), "{text}");
        assert!(text.contains("折叠或展开"), "{text}");

        app.modal = Some(Confirm {
            target: ConfirmTarget::Workspace("dsh:ws-1".to_string()),
            dry_run: false,
        });
        let text = render(&mut app, 120, 34);
        assert!(text.contains("删除工作区"), "{text}");
        assert!(text.contains("将删除工作区"), "{text}");
        assert!(text.contains("项目目录不受影响"), "{text}");
    }

    #[test]
    fn renders_cleanup_confirm() {
        let mut app = sample_app();
        let mut report = crate::gc::GcReport {
            dry_run: true,
            ..crate::gc::GcReport::default()
        };
        report.registry = 2;
        report.projcache = 3;
        report.bili_records = 1;
        report.bili_bytes = 4096;
        app.modal = Some(Confirm {
            target: ConfirmTarget::Cleanup(report),
            dry_run: false,
        });
        let text = render(&mut app, 120, 34);
        assert!(text.contains("清理残留"), "{text}");
        assert!(text.contains("工作区登记"), "{text}");
        assert!(text.contains("billion-context"), "{text}");
        assert!(text.contains("不会删除仍在的会话"), "{text}");
    }

    #[test]
    fn renders_help_and_filter() {
        let mut app = sample_app();
        app.show_help = true;
        let text = render(&mut app, 120, 60);
        assert!(text.contains("扫描位置"), "{text}");
        assert!(text.contains("按标题、ID、目录、工作区过滤"), "{text}");
        assert!(text.contains("折叠 / 展开当前工作区"), "{text}");
        assert!(text.contains("index.db"), "{text}");

        let mut app = sample_app();
        app.filter = "opencode".to_string();
        app.filter_editing = true;
        let text = render(&mut app, 100, 28);
        assert!(text.contains("/opencode"), "{text}");
    }
}
