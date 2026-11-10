//! 交互状态：工作区树、多选、过滤、删除确认与周期性重扫。

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::bili;
use crate::config::Config;
use crate::delete;
use crate::magic;
use crate::model::{self, Client, Session, Workspace, WorkspaceGroup};
use crate::scan::{self, Root};
use crate::theme::Palette;
use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Ok,
    Warn,
    Bad,
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub text: String,
    pub kind: NoticeKind,
}

impl Default for Notice {
    fn default() -> Self {
        Self {
            text: "就绪 · 空格多选 · d 删除".to_string(),
            kind: NoticeKind::Info,
        }
    }
}

/// 列表里的一行：工作区标题行，或它下面缩进的一条会话。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Workspace { group: usize },
    Session { session: usize, group: usize },
}

impl Row {
    pub fn group(self) -> usize {
        match self {
            Row::Workspace { group } | Row::Session { group, .. } => group,
        }
    }
}

/// 删除确认框的目标：若干会话，或一整个工作区。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmTarget {
    Sessions(Vec<usize>),
    Workspace(String),
    /// 清理残留（会话目录已不在，索引里还留着的记录）。
    Cleanup(crate::gc::GcReport),
}

#[derive(Debug, Clone)]
pub struct Confirm {
    pub target: ConfirmTarget,
    pub dry_run: bool,
}

pub struct App {
    pub sessions: Vec<Session>,
    /// 扫描到的工作区登记项，包含还没有会话的。
    pub known_workspaces: Vec<Workspace>,
    /// 按当前客户端与过滤条件聚合出的工作区。
    pub workspaces: Vec<WorkspaceGroup>,
    pub roots: Vec<Root>,
    pub warnings: Vec<String>,
    /// billion-context 的存储概况；本机没装 bili 时为空。
    pub bili: Option<bili::Summary>,
    /// magic-context 的存储概况；本机没装 magic-context 时为空。
    pub magic: Option<magic::Summary>,
    /// 树形列表的可见行：工作区标题行与会话行交错。
    pub rows: Vec<Row>,
    pub selected: usize,
    /// 折叠起来的工作区 key。
    pub collapsed: BTreeSet<String>,
    pub marked: BTreeSet<String>,
    pub filter: String,
    pub filter_editing: bool,
    pub client_filter: Option<Client>,
    pub modal: Option<Confirm>,
    pub show_help: bool,
    pub help_scroll: u16,
    pub detail_scroll: u16,
    pub detail_max: u16,
    pub notice: Notice,
    pub palette: Palette,
    pub config: Config,
    pub dirty: bool,
    pub quitting: bool,
    pub list_area: Rect,
    pub detail_area: Rect,
    pub list_offset: usize,
    last_scan: Instant,
}

impl App {
    pub fn new(config: Config) -> Self {
        Self::from_snapshot(scan::scan(), config)
    }

    pub fn from_snapshot(snapshot: scan::Snapshot, config: Config) -> Self {
        let palette = Palette::from_name(&config.theme);
        let mut app = Self {
            sessions: snapshot.sessions,
            known_workspaces: snapshot.workspaces,
            workspaces: Vec::new(),
            roots: snapshot.roots,
            warnings: snapshot.warnings,
            bili: snapshot.bili,
            magic: snapshot.magic,
            rows: Vec::new(),
            selected: 0,
            collapsed: BTreeSet::new(),
            marked: BTreeSet::new(),
            filter: String::new(),
            filter_editing: false,
            client_filter: None,
            modal: None,
            show_help: false,
            help_scroll: 0,
            detail_scroll: 0,
            detail_max: 0,
            notice: Notice::default(),
            palette,
            config,
            dirty: true,
            quitting: false,
            list_area: Rect::default(),
            detail_area: Rect::default(),
            list_offset: 0,
            last_scan: Instant::now(),
        };
        app.rebuild_rows();
        // 打开时停在第一条会话上，省得先越过工作区标题行。
        app.selected = app.first_session_row().unwrap_or(0);
        let total = app.sessions.len();
        let workspaces = app.workspaces.len();
        app.notice = Notice {
            text: format!("{total} 个会话 · {workspaces} 个工作区 · 空格多选 · d 删除"),
            kind: NoticeKind::Info,
        };
        app
    }

    pub fn reload(&mut self) {
        let previous = self.current_row().and_then(|row| self.row_key(row));
        let snapshot = scan::scan();
        self.sessions = snapshot.sessions;
        self.known_workspaces = snapshot.workspaces;
        self.roots = snapshot.roots;
        self.warnings = snapshot.warnings;
        self.bili = snapshot.bili;
        self.magic = snapshot.magic;
        self.last_scan = Instant::now();
        self.rebuild_rows();
        if let Some(key) = previous
            && let Some(position) = self
                .rows
                .iter()
                .position(|row| self.row_key(*row).as_deref() == Some(key.as_str()))
        {
            self.selected = position;
        }
        self.detail_scroll = 0;
        self.dirty = true;
    }

    /// 重新聚合工作区，再按折叠状态展开成树形行。
    pub fn rebuild_rows(&mut self) {
        let previous = self.current_row().and_then(|row| self.row_key(row));
        let filter = self.filter.trim().to_lowercase();
        let client = self.client_filter;
        let visible: BTreeSet<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| client.is_none_or(|client| session.client == client))
            .filter(|(_, session)| session.matches(&filter))
            .map(|(index, _)| index)
            .collect();

        self.rebuild_workspaces(&visible, &filter);
        self.collapsed
            .retain(|key| self.workspaces.iter().any(|group| &group.key == key));

        let mut rows = Vec::new();
        for (index, group) in self.workspaces.iter().enumerate() {
            rows.push(Row::Workspace { group: index });
            if self.collapsed.contains(&group.key) {
                continue;
            }
            for session in &group.indices {
                if visible.contains(session) {
                    rows.push(Row::Session {
                        session: *session,
                        group: index,
                    });
                }
            }
        }
        self.rows = rows;
        self.selected = previous
            .and_then(|key| {
                self.rows
                    .iter()
                    .position(|row| self.row_key(*row).as_deref() == Some(key.as_str()))
            })
            .unwrap_or_else(|| self.selected.min(self.rows.len().saturating_sub(1)));
        if self.rows.is_empty() {
            self.selected = 0;
        }
        self.dirty = true;
    }

    fn rebuild_workspaces(&mut self, visible: &BTreeSet<usize>, filter: &str) {
        let client = self.client_filter;
        self.workspaces = model::group_workspaces(&self.sessions, &self.known_workspaces);
        self.workspaces.retain(|group| {
            if let Some(client) = client
                && group.client != client
            {
                return false;
            }
            filter.is_empty()
                || group.matches(filter)
                || group.indices.iter().any(|index| visible.contains(index))
        });
    }

    pub fn current_row(&self) -> Option<Row> {
        self.rows.get(self.selected).copied()
    }

    pub fn current(&self) -> Option<&Session> {
        match self.current_row()? {
            Row::Session { session, .. } => self.sessions.get(session),
            Row::Workspace { .. } => None,
        }
    }

    pub fn current_group(&self) -> Option<&WorkspaceGroup> {
        self.workspaces.get(self.current_row()?.group())
    }

    pub fn group_at(&self, group: usize) -> Option<&WorkspaceGroup> {
        self.workspaces.get(group)
    }

    pub fn is_collapsed(&self, group: &WorkspaceGroup) -> bool {
        self.collapsed.contains(&group.key)
    }

    fn row_key(&self, row: Row) -> Option<String> {
        match row {
            Row::Workspace { group } => self
                .workspaces
                .get(group)
                .map(|group| format!("w:{}", group.key)),
            Row::Session { session, .. } => self
                .sessions
                .get(session)
                .map(|session| format!("s:{}", session.key())),
        }
    }

    fn first_session_row(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::Session { .. }))
    }

    pub fn marked_count(&self) -> usize {
        self.marked.len()
    }

    pub fn marked_bytes(&self) -> u64 {
        self.sessions
            .iter()
            .filter(|session| self.marked.contains(&session.key()))
            .map(Session::total_bytes)
            .sum()
    }

    /// 当前过滤条件下的会话（不受折叠影响）。
    fn filtered_sessions(&self) -> impl Iterator<Item = &Session> {
        let filter = self.filter.trim().to_lowercase();
        let client = self.client_filter;
        self.sessions
            .iter()
            .filter(move |session| client.is_none_or(|client| session.client == client))
            .filter(move |session| session.matches(&filter))
    }

    /// 当前过滤条件下的总占用（含 billion-context 数据）。
    pub fn visible_bytes(&self) -> u64 {
        self.filtered_sessions().map(Session::total_bytes).sum()
    }

    pub fn visible_sessions(&self) -> usize {
        self.filtered_sessions().count()
    }

    /// 确认框里要展示的会话。
    pub fn confirm_sessions(&self, target: &ConfirmTarget) -> Vec<&Session> {
        let indices: Vec<usize> = match target {
            ConfirmTarget::Sessions(indices) => indices.clone(),
            ConfirmTarget::Cleanup(_) => Vec::new(),
            ConfirmTarget::Workspace(key) => self
                .workspaces
                .iter()
                .find(|group| &group.key == key)
                .map(|group| group.indices.clone())
                .unwrap_or_default(),
        };
        indices
            .iter()
            .filter_map(|index| self.sessions.get(*index))
            .collect()
    }

    pub fn confirm_workspace(&self, target: &ConfirmTarget) -> Option<&WorkspaceGroup> {
        match target {
            ConfirmTarget::Workspace(key) => self.workspaces.iter().find(|group| &group.key == key),
            ConfirmTarget::Sessions(_) | ConfirmTarget::Cleanup(_) => None,
        }
    }

    /// 删除目标：有勾选就删勾选，否则删当前光标下的会话。
    fn delete_indices(&self) -> Vec<usize> {
        if self.marked.is_empty() {
            return match self.current_row() {
                Some(Row::Session { session, .. }) => vec![session],
                _ => Vec::new(),
            };
        }
        self.rows
            .iter()
            .filter_map(|row| match row {
                Row::Session { session, .. }
                    if self
                        .sessions
                        .get(*session)
                        .is_some_and(|session| self.marked.contains(&session.key())) =>
                {
                    Some(*session)
                }
                _ => None,
            })
            .collect()
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as isize;
        let next = (self.selected as isize + delta).clamp(0, len - 1);
        if next as usize != self.selected {
            self.selected = next as usize;
            self.detail_scroll = 0;
        }
    }

    fn select_group_row(&mut self, group: usize) {
        if let Some(position) = self
            .rows
            .iter()
            .position(|row| matches!(row, Row::Workspace { group: index } if *index == group))
        {
            self.selected = position;
        }
    }

    fn toggle_mark(&mut self) {
        match self.current_row() {
            Some(Row::Session { session, .. }) => {
                let Some(session) = self.sessions.get(session) else {
                    return;
                };
                let key = session.key();
                if !self.marked.remove(&key) {
                    self.marked.insert(key);
                }
                self.dirty = true;
                self.move_by(1);
            }
            Some(Row::Workspace { group }) => {
                let Some(group) = self.workspaces.get(group) else {
                    return;
                };
                let keys: Vec<String> = group
                    .indices
                    .iter()
                    .filter_map(|index| self.sessions.get(*index).map(Session::key))
                    .collect();
                let all_marked =
                    !keys.is_empty() && keys.iter().all(|key| self.marked.contains(key));
                for key in keys {
                    if all_marked {
                        self.marked.remove(&key);
                    } else {
                        self.marked.insert(key);
                    }
                }
                let text = if all_marked {
                    "已取消该工作区的勾选".to_string()
                } else {
                    format!("已勾选该工作区的会话 · 共 {} 个", self.marked.len())
                };
                self.set_notice(NoticeKind::Info, text);
                self.dirty = true;
                self.move_by(1);
            }
            None => {}
        }
    }

    fn mark_all(&mut self) {
        let indices: Vec<usize> = self
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Session { session, .. } => Some(*session),
                Row::Workspace { .. } => None,
            })
            .collect();
        let all_marked = !indices.is_empty()
            && indices
                .iter()
                .all(|index| self.marked.contains(&self.sessions[*index].key()));
        if all_marked {
            self.marked.clear();
            self.set_notice(NoticeKind::Info, "已取消全部勾选");
        } else {
            for index in &indices {
                self.marked.insert(self.sessions[*index].key());
            }
            let count = self.marked.len();
            self.set_notice(NoticeKind::Info, format!("已勾选 {count} 个会话"));
        }
        self.dirty = true;
    }

    /// `X`：清理已经没有会话目录、却还留在登记表 / 缓存 / billion-context
    /// 里的记录。先跑一遍演练，把数字放进确认框。
    fn request_cleanup(&mut self) {
        let preview = crate::gc::run(true);
        if preview.total() == 0 && preview.warnings.is_empty() {
            self.set_notice(NoticeKind::Ok, "没有残留记录，索引是干净的");
            return;
        }
        self.modal = Some(Confirm {
            target: ConfirmTarget::Cleanup(preview),
            dry_run: false,
        });
        self.dirty = true;
    }

    fn apply_cleanup(&mut self, dry_run: bool) {
        let report = crate::gc::run(dry_run);
        if dry_run {
            self.set_notice(
                NoticeKind::Warn,
                format!(
                    "演练：将清理 {} 项，释放 {}",
                    report.total(),
                    util::human_bytes(report.bili_bytes)
                ),
            );
            return;
        }
        self.reload();
        let warning = report.warnings.first().cloned();
        let text = format!(
            "已清理 {} 项（登记 {} · 缓存 {} · bili {} 条 {}）",
            report.total(),
            report.registry,
            report.projcache,
            report.bili_records,
            util::human_bytes(report.bili_bytes)
        );
        match warning {
            Some(warning) => self.set_notice(NoticeKind::Warn, format!("{text} · {warning}")),
            None if report.total() == 0 => {
                self.set_notice(NoticeKind::Info, "没有发现残留记录");
            }
            None => self.set_notice(NoticeKind::Ok, text),
        }
    }

    fn request_delete(&mut self) {
        if self.marked.is_empty() && matches!(self.current_row(), Some(Row::Workspace { .. })) {
            self.request_workspace_delete();
            return;
        }
        let indices = self.delete_indices();
        if indices.is_empty() {
            self.set_notice(NoticeKind::Warn, "没有可删除的会话");
            return;
        }
        if self.config.confirm_delete {
            self.modal = Some(Confirm {
                target: ConfirmTarget::Sessions(indices),
                dry_run: false,
            });
        } else {
            self.apply_delete(indices, false);
        }
        self.dirty = true;
    }

    fn request_workspace_delete(&mut self) {
        let Some(group) = self.current_group().cloned() else {
            self.set_notice(NoticeKind::Warn, "没有选中的工作区");
            return;
        };
        if group.indices.is_empty() && group.workspace.is_none() {
            self.set_notice(NoticeKind::Warn, "这个工作区没有可删除的会话");
            return;
        }
        if self.config.confirm_delete {
            self.modal = Some(Confirm {
                target: ConfirmTarget::Workspace(group.key.clone()),
                dry_run: false,
            });
        } else {
            self.apply_workspace_delete(&group.key, false);
        }
        self.dirty = true;
    }

    fn apply_delete(&mut self, indices: Vec<usize>, dry_run: bool) {
        let sessions: Vec<Session> = indices
            .iter()
            .filter_map(|index| self.sessions.get(*index).cloned())
            .collect();
        let report = delete::delete_many(&sessions, dry_run);
        for session in &sessions {
            if !report.failed.contains(&session.key()) {
                self.marked.remove(&session.key());
            }
        }
        if dry_run {
            self.set_notice(
                NoticeKind::Warn,
                format!(
                    "演练：将删除 {} 个会话，释放 {}",
                    report.deleted,
                    util::human_bytes(report.bytes)
                ),
            );
            return;
        }
        self.reload();
        if report.failed() {
            let first = report.errors.first().cloned().unwrap_or_default();
            self.set_notice(
                NoticeKind::Bad,
                format!(
                    "{} 个已删除，{} 个失败 · {first}",
                    report.deleted,
                    report.errors.len()
                ),
            );
        } else if let Some(warning) = report.warnings.first() {
            self.set_notice(
                NoticeKind::Warn,
                format!(
                    "已删除 {} 个会话，释放 {} · {warning}",
                    report.deleted,
                    util::human_bytes(report.bytes)
                ),
            );
        } else {
            self.set_notice(
                NoticeKind::Ok,
                format!(
                    "已删除 {} 个会话，释放 {}",
                    report.deleted,
                    util::human_bytes(report.bytes)
                ),
            );
        }
        self.detail_scroll = 0;
    }

    fn apply_workspace_delete(&mut self, key: &str, dry_run: bool) {
        let Some(group) = self
            .workspaces
            .iter()
            .find(|group| group.key == key)
            .cloned()
        else {
            self.set_notice(NoticeKind::Warn, "工作区已不存在，请重新扫描");
            return;
        };
        let sessions: Vec<Session> = group
            .indices
            .iter()
            .filter_map(|index| self.sessions.get(*index).cloned())
            .collect();
        let report = delete::delete_workspace(&sessions, group.workspace.as_ref(), dry_run);
        if dry_run {
            self.set_notice(
                NoticeKind::Warn,
                format!(
                    "演练：将删除工作区「{}」的 {} 个会话，释放 {}",
                    group.title(),
                    report.deleted,
                    util::human_bytes(report.bytes)
                ),
            );
            return;
        }
        for session in &sessions {
            self.marked.remove(&session.key());
        }
        self.reload();
        if report.failed() {
            let first = report.errors.first().cloned().unwrap_or_default();
            self.set_notice(
                NoticeKind::Bad,
                format!(
                    "「{}」已删除 {} 个会话，{} 个未处理 · {first}",
                    group.title(),
                    report.deleted,
                    report.skipped
                ),
            );
        } else {
            let mut text = format!(
                "已删除工作区「{}」· {} 个会话 · {}",
                group.title(),
                report.deleted,
                util::human_bytes(report.bytes)
            );
            if report.deregistered {
                text.push_str(" · 已移除登记");
            }
            if let Some(warning) = report.warnings.first() {
                text.push_str(" · ");
                text.push_str(warning);
            }
            let kind = if report.warnings.is_empty() {
                NoticeKind::Ok
            } else {
                NoticeKind::Warn
            };
            self.set_notice(kind, text);
        }
        self.detail_scroll = 0;
    }

    /// 折叠当前工作区；光标在会话上时顺带跳回它的标题行。
    fn collapse_current(&mut self) {
        let Some(row) = self.current_row() else {
            return;
        };
        let group = row.group();
        let Some(key) = self.workspaces.get(group).map(|group| group.key.clone()) else {
            return;
        };
        self.collapsed.insert(key);
        self.rebuild_rows();
        self.select_group_row(group);
        self.detail_scroll = 0;
    }

    /// 展开当前所在的工作区。
    fn expand_current(&mut self) {
        let Some(row) = self.current_row() else {
            return;
        };
        let group = row.group();
        let Some(key) = self.workspaces.get(group).map(|group| group.key.clone()) else {
            return;
        };
        if self.collapsed.remove(&key) {
            self.rebuild_rows();
            self.select_group_row(group);
            self.detail_scroll = 0;
        }
    }

    fn toggle_group(&mut self) {
        let Some(row) = self.current_row() else {
            return;
        };
        let group = row.group();
        let collapsed = self
            .workspaces
            .get(group)
            .is_some_and(|group| self.collapsed.contains(&group.key));
        if collapsed {
            self.expand_current();
        } else {
            self.collapse_current();
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        self.dirty = true;
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.modal.is_some() {
            self.on_modal_key(key);
            return;
        }
        if self.show_help {
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => self.show_help = false,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                KeyCode::PageDown | KeyCode::Char('J') => {
                    self.help_scroll = self.help_scroll.saturating_add(8)
                }
                KeyCode::PageUp | KeyCode::Char('K') => {
                    self.help_scroll = self.help_scroll.saturating_sub(8)
                }
                _ => {}
            }
            return;
        }
        if self.filter_editing {
            self.on_filter_key(key);
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.quitting = true,
            KeyCode::Esc => self.on_escape(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quitting = true
            }
            KeyCode::Tab => self.cycle_client(1),
            KeyCode::BackTab => self.cycle_client(-1),
            KeyCode::Char('/') => {
                self.filter_editing = true;
                self.filter.clear();
                self.rebuild_rows();
            }
            KeyCode::Char('r') => {
                self.set_notice(NoticeKind::Info, "正在重新扫描…");
                self.reload();
                let total = self.sessions.len();
                self.set_notice(NoticeKind::Info, format!("扫描完成 · {total} 个会话"));
            }
            KeyCode::Char('t') => {
                let (palette, name) = self.palette.next();
                self.palette = palette;
                self.set_notice(NoticeKind::Info, format!("主题：{name}"));
            }
            KeyCode::Char('?') => {
                self.show_help = true;
                self.help_scroll = 0;
            }
            KeyCode::Char('J') => self.detail_scroll = self.detail_scroll.saturating_add(1),
            KeyCode::Char('K') => self.detail_scroll = self.detail_scroll.saturating_sub(1),
            KeyCode::Enter => {
                if matches!(self.current_row(), Some(Row::Workspace { .. })) {
                    self.toggle_group();
                }
            }
            KeyCode::Left | KeyCode::Char('h') => self.collapse_current(),
            KeyCode::Right | KeyCode::Char('l') => self.expand_current(),
            _ => self.on_list_key(key),
        }
    }

    fn on_escape(&mut self) {
        if !self.filter.is_empty() {
            self.filter.clear();
            self.rebuild_rows();
            self.set_notice(NoticeKind::Info, "已清除过滤");
        } else if !self.marked.is_empty() {
            self.marked.clear();
            self.dirty = true;
            self.set_notice(NoticeKind::Info, "已取消全部勾选");
        } else {
            self.quitting = true;
        }
    }

    fn on_list_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::Home => {
                self.selected = 0;
                self.detail_scroll = 0;
            }
            KeyCode::End => {
                self.selected = self.rows.len().saturating_sub(1);
                self.detail_scroll = 0;
            }
            KeyCode::Char(' ') => self.toggle_mark(),
            KeyCode::Char('a') => self.mark_all(),
            KeyCode::Char('d') => self.request_delete(),
            KeyCode::Char('X') => self.request_cleanup(),
            _ => {}
        }
    }

    fn on_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.filter_editing = false;
                self.rebuild_rows();
                let count = self.visible_sessions();
                let workspaces = self.workspaces.len();
                self.set_notice(
                    NoticeKind::Info,
                    format!("过滤后 {count} 个会话 · {workspaces} 个工作区"),
                );
            }
            KeyCode::Esc => {
                self.filter_editing = false;
                self.filter.clear();
                self.rebuild_rows();
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.rebuild_rows();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.clear();
                self.rebuild_rows();
            }
            KeyCode::Char(ch) => {
                self.filter.push(ch);
                self.rebuild_rows();
            }
            _ => {}
        }
    }

    fn on_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.clone() else {
            return;
        };
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.modal = None;
                match modal.target {
                    ConfirmTarget::Sessions(indices) => self.apply_delete(indices, modal.dry_run),
                    ConfirmTarget::Workspace(key) => {
                        self.apply_workspace_delete(&key, modal.dry_run)
                    }
                    ConfirmTarget::Cleanup(report) => {
                        let _ = report;
                        self.apply_cleanup(false);
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => {
                self.modal = None;
                self.set_notice(NoticeKind::Info, "已取消删除");
            }
            _ => {}
        }
    }

    fn cycle_client(&mut self, step: isize) {
        let mut order: Vec<Option<Client>> = vec![None];
        order.extend(Client::ALL.iter().copied().map(Some));
        let current = order
            .iter()
            .position(|item| *item == self.client_filter)
            .unwrap_or(0) as isize;
        let len = order.len() as isize;
        let next = ((current + step) % len + len) % len;
        self.client_filter = order[next as usize];
        self.rebuild_rows();
        let label = self
            .client_filter
            .map(|client| client.label())
            .unwrap_or("全部");
        self.set_notice(NoticeKind::Info, format!("视图：{label}"));
        self.detail_scroll = 0;
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        let in_list = |area: Rect| {
            mouse.column >= area.x
                && mouse.column < area.x + area.width
                && mouse.row > area.y
                && mouse.row < area.y + area.height
        };
        match mouse.kind {
            // 滚轮在左边列表上滚动就是移动光标，在右边详情上才是翻详情。
            MouseEventKind::ScrollDown => {
                if in_list(self.list_area) {
                    self.move_by(1);
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
            }
            MouseEventKind::ScrollUp => {
                if in_list(self.list_area) {
                    self.move_by(-1);
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.list_area;
                let inside = mouse.column > area.x
                    && mouse.column < area.x + area.width.saturating_sub(1)
                    && mouse.row > area.y
                    && mouse.row < area.y + area.height.saturating_sub(1);
                if !inside {
                    return;
                }
                let row = (mouse.row - area.y - 1) as usize + self.list_offset;
                if row < self.rows.len() {
                    let same = row == self.selected;
                    self.selected = row;
                    self.detail_scroll = 0;
                    self.dirty = true;
                    // 再点一次已选中的工作区行 = 折叠 / 展开，省得去够 h/l。
                    if same && matches!(self.current_row(), Some(Row::Workspace { .. })) {
                        self.toggle_group();
                    }
                }
            }
            _ => {}
        }
    }

    pub fn tick(&mut self) {
        if self.config.rescan_secs == 0
            || self.modal.is_some()
            || self.show_help
            || self.filter_editing
        {
            return;
        }
        if self.last_scan.elapsed() >= self.config.rescan_interval() {
            self.reload();
        }
    }

    pub fn status_line(&self) -> (String, NoticeKind) {
        (self.notice.text.clone(), self.notice.kind)
    }

    pub fn mouse_enabled(&self) -> bool {
        self.config.mouse
    }

    fn set_notice(&mut self, kind: NoticeKind, text: impl Into<String>) {
        self.notice = Notice {
            text: text.into(),
            kind,
        };
    }
}

/// 启动 TUI 主循环。
pub fn run(config: Config) -> Result<()> {
    let mouse = config.mouse;
    let mut terminal = ratatui::init();
    if mouse {
        let mut stdout = std::io::stdout();
        ratatui::crossterm::execute!(stdout, ratatui::crossterm::event::EnableMouseCapture)?;
    }
    let mut app = App::new(config);

    let result = loop {
        if app.dirty {
            terminal.draw(|frame| crate::ui::draw(frame, &mut app))?;
            app.dirty = false;
        }

        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                event::Event::Key(key) => app.on_key(key),
                event::Event::Mouse(mouse_event) if app.mouse_enabled() => {
                    app.on_mouse(mouse_event)
                }
                event::Event::Resize(_, _) => app.dirty = true,
                _ => {}
            }
        }

        app.tick();
        if app.quitting {
            break Ok(());
        }
    };

    if mouse {
        let mut stdout = std::io::stdout();
        let _ =
            ratatui::crossterm::execute!(stdout, ratatui::crossterm::event::DisableMouseCapture);
    }
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BiliMeta;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn session_index(row: Row) -> usize {
        match row {
            Row::Session { session, .. } => session,
            Row::Workspace { .. } => panic!("工作区行没有会话下标"),
        }
    }

    fn session(client: Client, id: &str, workspace: Option<Workspace>) -> Session {
        Session {
            client,
            id: id.into(),
            title: format!("会话 {id}"),
            preview: String::new(),
            path: PathBuf::from(format!("/tmp/{id}")),
            root: PathBuf::from("/tmp"),
            cwd: None,
            workspace,
            created: None,
            updated: Some(UNIX_EPOCH + Duration::from_secs(60)),
            bytes: 1024,
            messages: 2,
            tokens: None,
            cost: None,
            locked: false,
            bili: None,
            subagent: false,
            parent: None,
            magic: None,
        }
    }

    fn workspace(client: Client, id: &str, path: &str) -> Workspace {
        Workspace {
            client,
            id: id.into(),
            path: path.into(),
            title: path.rsplit('/').next().unwrap_or(path).into(),
            root: PathBuf::from("/tmp"),
        }
    }

    fn app_with(sessions: Vec<Session>, known: Vec<Workspace>) -> App {
        let snapshot = scan::Snapshot {
            sessions,
            workspaces: known,
            roots: Vec::new(),
            warnings: Vec::new(),
            bili: None,
            magic: None,
        };
        App::from_snapshot(snapshot, Config::default())
    }

    fn sample() -> App {
        let project = workspace(Client::Dsh, "ws-1", "/work/project");
        let mut a = session(Client::Dsh, "a", Some(project.clone()));
        a.updated = Some(SystemTime::now());
        let b = session(Client::Dsh, "b", Some(project.clone()));
        let codex = session(Client::Codex, "c", None);
        app_with(vec![a, b, codex], vec![project])
    }

    #[test]
    fn groups_sessions_under_workspace_rows() {
        let app = sample();
        // 1 个工作区标题 + 2 条 DSH 会话 + 1 个未知组标题 + 1 条 Codex 会话。
        assert_eq!(app.rows.len(), 5, "{:?}", app.rows);
        assert!(matches!(app.rows[0], Row::Workspace { .. }));
        assert!(matches!(app.rows[1], Row::Session { .. }));
        assert!(matches!(app.rows[2], Row::Session { .. }));
        assert!(matches!(app.rows[3], Row::Workspace { .. }));
        assert!(matches!(app.rows[4], Row::Session { .. }));
        // 打开时停在第一条会话上。
        assert!(matches!(app.current_row(), Some(Row::Session { .. })));
    }

    #[test]
    fn enter_toggles_only_on_workspace_rows() {
        let mut app = sample();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // 会话行按 Enter 不折叠父工作区。
        app.selected = 1;
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.rows.len(), 5, "{:?}", app.rows);
        // 工作区行按 Enter 折叠，再按一次展开。
        app.selected = 0;
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.rows.len(), 3, "{:?}", app.rows);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.rows.len(), 5, "{:?}", app.rows);
    }

    #[test]
    fn collapsing_hides_children_and_keeps_selection_sane() {
        let mut app = sample();
        let key = app.workspaces[0].key.clone();
        app.collapsed.insert(key);
        app.rebuild_rows();
        assert_eq!(app.rows.len(), 3, "{:?}", app.rows);
        assert!(matches!(app.rows[0], Row::Workspace { .. }));
        assert!(app.selected < app.rows.len());

        // 光标在折叠组的会话上时按左键，跳回标题行。
        app.selected = 0;
        app.collapse_current();
        assert!(matches!(app.current_row(), Some(Row::Workspace { .. })));
        app.expand_current();
        assert_eq!(app.rows.len(), 5, "{:?}", app.rows);
        // 折叠只影响树的行数，头部统计仍按过滤条件算。
        assert_eq!(app.visible_sessions(), 3);
    }

    #[test]
    fn workspace_row_marks_and_unmarks_its_sessions() {
        let mut app = sample();
        app.selected = 0;
        app.toggle_mark();
        assert_eq!(app.marked_count(), 2);
        app.selected = 0;
        app.toggle_mark();
        assert_eq!(app.marked_count(), 0);
    }

    #[test]
    fn delete_targets_marks_before_cursor() {
        let mut app = sample();
        app.selected = 1;
        app.marked.insert(app.sessions[1].key());
        assert_eq!(app.delete_indices(), vec![1]);
        app.marked.clear();
        assert_eq!(app.delete_indices(), vec![session_index(app.rows[1])]);
    }

    #[test]
    fn bili_bytes_are_included_in_totals() {
        let mut app = sample();
        app.sessions[0].bili = Some(BiliMeta {
            db: PathBuf::from("/tmp/index.db"),
            bytes: 2048,
            messages: 7,
            updated: None,
            status: "active".into(),
        });
        app.marked.insert(app.sessions[0].key());
        assert_eq!(app.marked_bytes(), 1024 + 2048);
        let visible = app.visible_bytes();
        assert!(visible >= 1024 + 2048);
    }

    #[test]
    fn filter_collapses_tree_to_matching_sessions() {
        let mut app = sample();
        app.filter = "zzz".into();
        app.rebuild_rows();
        assert_eq!(app.visible_sessions(), 0);
        assert!(app.rows.is_empty(), "{:?}", app.rows);
        app.filter = "会话 b".into();
        app.rebuild_rows();
        assert_eq!(app.visible_sessions(), 1);
        assert_eq!(app.rows.len(), 2, "{:?}", app.rows);
        assert!(matches!(app.rows[0], Row::Workspace { .. }));
        assert!(matches!(app.rows[1], Row::Session { .. }));
    }

    #[test]
    fn escape_clears_filter_then_marks_then_quits() {
        let mut app = sample();
        app.filter = "x".into();
        app.marked.insert(app.sessions[0].key());
        app.on_escape();
        assert!(app.filter.is_empty());
        assert_eq!(app.marked_count(), 1);
        app.on_escape();
        assert_eq!(app.marked_count(), 0);
        assert!(!app.quitting);
        app.on_escape();
        assert!(app.quitting);
    }

    #[test]
    fn moving_clamps_to_visible_rows() {
        let mut app = sample();
        app.move_by(-5);
        assert_eq!(app.selected, 0);
        app.move_by(50);
        assert_eq!(app.selected, app.rows.len() - 1);
    }

    #[test]
    fn empty_snapshot_has_no_rows() {
        let mut app = app_with(Vec::new(), Vec::new());
        assert!(app.rows.is_empty());
        assert!(app.current_row().is_none());
        app.rebuild_rows();
        assert!(app.rows.is_empty());
    }
}
