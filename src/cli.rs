//! CLI 子命令。无子命令时由 `main` 启动 TUI。

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;
use unicode_width::UnicodeWidthStr;

use crate::bili;
use crate::config::Config;
use crate::delete;
use crate::magic;
use crate::model::{self, Client, Session, WorkspaceGroup};
use crate::scan;
use crate::util;

#[derive(Parser)]
#[command(
    name = "asclean",
    version,
    about = "浏览、筛选与删除本机 AI 编码客户端的会话",
    long_about = "扫描 DeepSeek Harness、Codex、Opencode 的会话存储，列出会话并在终端里\
                  多选删除。无子命令时启动 TUI。"
)]
pub struct Cli {
    /// 以 JSON 输出
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// 列出会话
    List {
        /// 只看某个客户端：dsh / codex / opencode
        #[arg(long)]
        client: Option<String>,
        /// 只看某个工作区：标识、路径、标题或路径末段
        #[arg(long)]
        workspace: Option<String>,
    },
    /// 删除会话，参数形如 `dsh:session-<uuid>`、`codex:<uuid>`、`opencode:ses_<id>`
    Delete {
        /// 会话标识；省略 `client:` 前缀时按 ID 全局匹配
        ids: Vec<String>,
        /// 只打印将要删除的内容
        #[arg(long)]
        dry_run: bool,
        /// 只删除某个客户端的会话
        #[arg(long)]
        client: Option<String>,
    },
    /// 列出工作区（含还没有会话的登记项）
    Workspaces {
        /// 只看某个客户端：dsh / codex / opencode
        #[arg(long)]
        client: Option<String>,
    },
    /// 删除整个工作区：清掉它的会话存储与登记，不动项目目录
    DeleteWorkspace {
        /// 工作区标识、完整路径、标题，或路径末段
        specs: Vec<String>,
        /// 只打印将要删除的内容
        #[arg(long)]
        dry_run: bool,
        /// 只匹配某个客户端的工作区
        #[arg(long)]
        client: Option<String>,
    },
    /// 清理残留：会话目录已删，但工作区登记、渲染缓存或
    /// billion-context 记录里还留着的条目
    Gc {
        /// 只统计，不删除
        #[arg(long)]
        dry_run: bool,
    },
    /// 检查扫描到的存储位置、数据库与依赖
    Doctor,
    /// 打印扫描路径
    Paths,
    /// 显示配置路径与当前值
    Config {
        /// 写入默认配置文件
        #[arg(long)]
        init: bool,
        /// 覆盖已有配置文件（配合 --init）
        #[arg(long)]
        force: bool,
    },
}

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        None => unreachable!("无子命令时由 main 启动 TUI"),
        Some(command) => dispatch(command, cli.json),
    }
}

fn dispatch(command: Cmd, json: bool) -> Result<()> {
    match command {
        Cmd::List { client, workspace } => list(client.as_deref(), workspace.as_deref(), json),
        Cmd::Delete {
            ids,
            dry_run,
            client,
        } => delete_cmd(&ids, dry_run, client.as_deref(), json),
        Cmd::Workspaces { client } => workspaces_cmd(client.as_deref(), json),
        Cmd::DeleteWorkspace {
            specs,
            dry_run,
            client,
        } => delete_workspace_cmd(&specs, dry_run, client.as_deref(), json),
        Cmd::Gc { dry_run } => gc_cmd(dry_run, json),
        Cmd::Doctor => doctor(),
        Cmd::Paths => paths(json),
        Cmd::Config { init, force } => config_cmd(init, force, json),
    }
}

fn parse_client(name: &str) -> Result<Client> {
    Client::from_key(name)
        .ok_or_else(|| anyhow::anyhow!("未知客户端：{name}（可用 dsh / codex / opencode）"))
}

fn sessions_of(client: Option<Client>) -> Vec<Session> {
    scan::scan()
        .sessions
        .into_iter()
        .filter(|session| client.is_none_or(|wanted| session.client == wanted))
        .collect()
}

/// 扫描一次并按客户端筛出工作区行。
fn workspace_groups(snapshot: &scan::Snapshot, client: Option<Client>) -> Vec<WorkspaceGroup> {
    let mut groups = model::group_workspaces(&snapshot.sessions, &snapshot.workspaces);
    if let Some(client) = client {
        groups.retain(|group| group.client == client);
    }
    groups
}

fn list(client: Option<&str>, workspace: Option<&str>, json: bool) -> Result<()> {
    let client = client.map(parse_client).transpose()?;
    let snapshot = scan::scan();
    let groups = workspace_groups(&snapshot, client);
    let workspace = workspace
        .map(|spec| resolve_workspace(&groups, spec))
        .transpose()?;
    let sessions: Vec<Session> = snapshot
        .sessions
        .into_iter()
        .filter(|session| client.is_none_or(|wanted| session.client == wanted))
        .filter(|session| {
            workspace
                .as_ref()
                .is_none_or(|group| session.workspace_group_key() == group.key)
        })
        .collect();
    if json {
        let items: Vec<_> = sessions
            .iter()
            .map(|session| {
                json!({
                    "client": session.client.key(),
                    "id": session.id,
                    "title": session.title,
                    "path": session.path.display().to_string(),
                    "cwd": session.cwd,
                    "workspace": session.workspace.as_ref().map(|workspace| json!({
                        "id": workspace.id,
                        "path": workspace.path,
                        "title": workspace.title,
                    })),
                    "bytes": session.bytes,
                    "bili": session.bili.as_ref().map(|meta| json!({
                        "db": meta.db.display().to_string(),
                        "bytes": meta.bytes,
                        "messages": meta.messages,
                        "status": meta.status,
                        "updated": meta.updated.map(util::local_datetime),
                    })),
                    "magic": session.magic.as_ref().map(|meta| json!({
                        "db": meta.db.display().to_string(),
                        "compartments": meta.compartments,
                        "status": meta.status,
                        "updated": meta.updated.map(util::local_datetime),
                    })),
                    "messages": session.messages,
                    "locked": session.locked,
                    "tokens_input": session.tokens.map(|tokens| tokens.0),
                    "tokens_output": session.tokens.map(|tokens| tokens.1),
                    "cost": session.cost,
                    "created": session.created.map(util::local_datetime),
                    "updated": session.updated.map(util::local_datetime),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    if sessions.is_empty() {
        if let Some(group) = &workspace {
            println!("工作区「{}」下没有会话。", group.title());
        } else {
            println!("没有找到会话。用 `asclean doctor` 检查扫描路径。");
        }
        return Ok(());
    }

    let widths = TableWidths::new(&sessions);
    println!("{}", header_row(&widths));
    for session in &sessions {
        println!("{}", session_row(session, &widths));
    }
    let bytes: u64 = sessions.iter().map(Session::total_bytes).sum();
    let bili: u64 = sessions.iter().map(Session::bili_bytes).sum();
    println!(
        "\n共 {} 个会话 · {}\n客户端：DSH {} · Codex {} · Opencode {}",
        sessions.len(),
        util::human_bytes(bytes),
        count_client(&sessions, Client::Dsh),
        count_client(&sessions, Client::Codex),
        count_client(&sessions, Client::Opencode),
    );
    if bili > 0 {
        println!("其中 billion-context 数据 {}", util::human_bytes(bili));
    }
    let magic: u32 = sessions.iter().map(Session::magic_compartments).sum();
    if magic > 0 {
        println!("其中 magic-context 记录 {magic} 个隔间");
    }
    if let Some(group) = &workspace {
        println!(
            "工作区：{} · {}",
            group.title(),
            group.path().unwrap_or("未识别")
        );
    }
    Ok(())
}

fn workspaces_cmd(client: Option<&str>, json: bool) -> Result<()> {
    let client = client.map(parse_client).transpose()?;
    let snapshot = scan::scan();
    let groups = workspace_groups(&snapshot, client);
    if json {
        let items: Vec<_> = groups
            .iter()
            .map(|group| {
                json!({
                    "client": group.client.key(),
                    "id": group.workspace.as_ref().map(|workspace| workspace.id.clone()),
                    "key": group.key,
                    "path": group.path(),
                    "title": group.title(),
                    "sessions": group.count(),
                    "bytes": group.bytes,
                    "messages": group.messages,
                    "locked": group.locked,
                    "updated": group.updated.map(util::local_datetime),
                    "synthetic": group.workspace.as_ref().is_some_and(|workspace| workspace.is_synthetic()),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    if groups.is_empty() {
        println!("没有找到工作区。用 `asclean doctor` 检查扫描路径。");
        return Ok(());
    }

    let widths = WorkspaceWidths::new(&groups);
    println!(
        "{}  {}  {}  {}  {}  路径",
        pad_right("客户端", widths.client),
        pad_right("工作区", widths.title),
        pad_right("会话", widths.sessions),
        pad_right("大小", widths.size),
        pad_right("最近更新", widths.updated),
    );
    for group in &groups {
        println!(
            "{}  {}  {}  {}  {}  {}",
            pad_right(group.client.label(), widths.client),
            pad_right(&util::truncate(group.title(), widths.title), widths.title),
            pad_right(&group.count().to_string(), widths.sessions),
            pad_right(&group.size_label(), widths.size),
            pad_right(&group.time_label(), widths.updated),
            group.path().unwrap_or("未识别"),
        );
    }
    let sessions: usize = groups.iter().map(WorkspaceGroup::count).sum();
    let bytes: u64 = groups.iter().map(|group| group.bytes).sum();
    println!(
        "\n共 {} 个工作区 · {} 个会话 · {}",
        groups.len(),
        sessions,
        util::human_bytes(bytes)
    );
    Ok(())
}

fn count_client(sessions: &[Session], client: Client) -> usize {
    sessions
        .iter()
        .filter(|session| session.client == client)
        .count()
}

struct TableWidths {
    client: usize,
    updated: usize,
    workspace: usize,
    size: usize,
    bili: usize,
    messages: usize,
}

impl TableWidths {
    fn new(sessions: &[Session]) -> Self {
        Self {
            client: sessions
                .iter()
                .map(|session| session.client.label().len())
                .max()
                .unwrap_or(6)
                .max(6),
            updated: 16,
            workspace: sessions
                .iter()
                .map(|session| {
                    session
                        .workspace
                        .as_ref()
                        .map(|workspace| UnicodeWidthStr::width(workspace.title.as_str()))
                        .unwrap_or(4)
                })
                .max()
                .unwrap_or(4)
                .clamp(4, 24),
            size: sessions
                .iter()
                .map(|session| session.size_label().len())
                .max()
                .unwrap_or(6)
                .max(6),
            bili: 7,
            messages: 5,
        }
    }
}

struct WorkspaceWidths {
    client: usize,
    title: usize,
    sessions: usize,
    size: usize,
    updated: usize,
}

impl WorkspaceWidths {
    fn new(groups: &[WorkspaceGroup]) -> Self {
        Self {
            client: groups
                .iter()
                .map(|group| group.client.label().len())
                .max()
                .unwrap_or(6)
                .max(6),
            title: groups
                .iter()
                .map(|group| UnicodeWidthStr::width(group.title()))
                .max()
                .unwrap_or(8)
                .clamp(8, 30),
            sessions: 4,
            size: groups
                .iter()
                .map(|group| group.size_label().len())
                .max()
                .unwrap_or(6)
                .max(6),
            updated: 16,
        }
    }
}

fn pad_right(text: &str, width: usize) -> String {
    let current = UnicodeWidthStr::width(text);
    if current >= width {
        return text.to_string();
    }
    format!("{text}{}", " ".repeat(width - current))
}

fn header_row(widths: &TableWidths) -> String {
    format!(
        "{}  {}  {}  {}  {}  {}  {}  {}",
        pad_right("客户端", widths.client),
        pad_right("更新时间", widths.updated),
        pad_right("工作区", widths.workspace),
        pad_right("大小", widths.size),
        pad_right("附带", widths.bili),
        pad_right("消息", widths.messages),
        pad_right("状态", 6),
        "标题"
    )
}

fn session_row(session: &Session, widths: &TableWidths) -> String {
    let workspace = session
        .workspace
        .as_ref()
        .map(|workspace| util::truncate(&workspace.title, widths.workspace))
        .unwrap_or_else(|| "未识别".to_string());
    let attached = if session.bili.is_some() {
        util::human_bytes(session.bili_bytes())
    } else if session.magic.is_some() {
        format!("{} 间", session.magic_compartments())
    } else {
        "--".to_string()
    };
    format!(
        "{}  {}  {}  {}  {}  {}  {}  {}",
        pad_right(session.client.label(), widths.client),
        pad_right(
            &session
                .updated
                .map(util::local_datetime)
                .unwrap_or_else(|| "--".to_string()),
            widths.updated
        ),
        pad_right(&workspace, widths.workspace),
        pad_right(&session.size_label(), widths.size),
        pad_right(&attached, widths.bili),
        pad_right(&session.messages.to_string(), widths.messages),
        pad_right(
            if session.locked {
                "使用中"
            } else {
                "空闲"
            },
            6
        ),
        session.title
    )
}

/// 支持 `client:id` 前缀，也接受裸 ID。
fn resolve(sessions: &[Session], spec: &str) -> Result<Session> {
    let (client, id) = match spec.split_once(':') {
        Some((prefix, rest)) if Client::from_key(prefix).is_some() => {
            (Client::from_key(prefix), rest)
        }
        _ => (None, spec),
    };
    let matches: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.id == id && client.is_none_or(|wanted| session.client == wanted))
        .collect();
    match matches.len() {
        0 => bail!("没有找到会话：{spec}"),
        1 => Ok(matches[0].clone()),
        _ => bail!(
            "{spec} 匹配到多个会话，请加客户端前缀（{}）",
            matches
                .iter()
                .map(|session| format!("{}:{}", session.client.key(), session.id))
                .collect::<Vec<_>>()
                .join(" / ")
        ),
    }
}

/// 工作区 `<spec>`：标识、完整路径、标题或路径末段，可带 `client:` 前缀。
fn resolve_workspace(groups: &[WorkspaceGroup], spec: &str) -> Result<WorkspaceGroup> {
    let (client, text) = match spec.split_once(':') {
        Some((prefix, rest)) if Client::from_key(prefix).is_some() => {
            (Client::from_key(prefix), rest)
        }
        _ => (None, spec),
    };
    let matches: Vec<&WorkspaceGroup> = groups
        .iter()
        .filter(|group| client.is_none_or(|wanted| group.client == wanted))
        .filter(|group| group.matches_spec(text))
        .collect();
    match matches.len() {
        0 => bail!("没有找到工作区：{spec}（用 `asclean workspaces` 查看可用项）"),
        1 => Ok(matches[0].clone()),
        _ => bail!(
            "{spec} 匹配到多个工作区，请用完整路径或标识（{}）",
            matches
                .iter()
                .map(|group| group.key.clone())
                .collect::<Vec<_>>()
                .join(" / ")
        ),
    }
}

fn delete_workspace_cmd(
    specs: &[String],
    dry_run: bool,
    client: Option<&str>,
    json: bool,
) -> Result<()> {
    let client = client.map(parse_client).transpose()?;
    if specs.is_empty() {
        bail!("请给出至少一个工作区（标识、路径或标题；用 `asclean workspaces` 查看）");
    }
    let snapshot = scan::scan();
    let groups = workspace_groups(&snapshot, client);
    let mut targets: Vec<WorkspaceGroup> = Vec::new();
    let mut seen = BTreeSet::new();
    for spec in specs {
        let group = resolve_workspace(&groups, spec)?;
        if seen.insert(group.key.clone()) {
            targets.push(group);
        }
    }

    let mut deleted = 0usize;
    let mut bytes = 0u64;
    let mut skipped = 0usize;
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut deregistered = 0usize;
    let mut rows = Vec::new();
    for group in &targets {
        let sessions: Vec<Session> = group
            .indices
            .iter()
            .filter_map(|index| snapshot.sessions.get(*index).cloned())
            .collect();
        let report = delete::delete_workspace(&sessions, group.workspace.as_ref(), dry_run);
        deleted += report.deleted;
        bytes += report.bytes;
        skipped += report.skipped;
        errors.extend(report.errors.iter().cloned());
        warnings.extend(report.warnings.iter().cloned());
        if report.deregistered {
            deregistered += 1;
        }
        rows.push((group.clone(), report));
    }

    if json {
        let items: Vec<_> = rows
            .iter()
            .map(|(group, report)| {
                json!({
                    "key": group.key,
                    "title": group.title(),
                    "path": group.path(),
                    "deleted": report.deleted,
                    "bytes": report.bytes,
                    "skipped": report.skipped,
                    "deregistered": report.deregistered,
                    "errors": report.errors,
                    "warnings": report.warnings,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "dry_run": dry_run,
                "deleted": deleted,
                "bytes": bytes,
                "skipped": skipped,
                "deregistered": deregistered,
                "errors": errors,
                "warnings": warnings,
                "workspaces": items,
            }))?
        );
    } else {
        for (group, report) in &rows {
            let status = if dry_run {
                "将删除"
            } else if report.failed() {
                "部分失败"
            } else {
                "已删除"
            };
            println!(
                "{status} · 工作区 {} · {} 个会话 · {}（{}）{}",
                group.client.label(),
                report.deleted,
                util::human_bytes(report.bytes),
                group.title(),
                if report.deregistered {
                    " · 已移除登记"
                } else {
                    ""
                }
            );
        }
        println!(
            "\n{} 个工作区 · {} 个会话 · 释放 {}{}",
            targets.len(),
            deleted,
            util::human_bytes(bytes),
            if dry_run { "（演练）" } else { "" }
        );
        if deregistered > 0 {
            println!("已移除 {deregistered} 个工作区登记；项目目录保持原样。");
        }
        for warning in &warnings {
            println!("提示：{warning}");
        }
        for error in &errors {
            println!("错误：{error}");
        }
    }

    if !errors.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

fn delete_cmd(ids: &[String], dry_run: bool, client: Option<&str>, json: bool) -> Result<()> {
    let client = client.map(parse_client).transpose()?;
    if ids.is_empty() {
        bail!("请给出至少一个会话 ID（或先在 TUI 里用 `S` 复制）");
    }
    let all = sessions_of(client);
    let mut targets = Vec::new();
    for spec in ids {
        targets.push(resolve(&all, spec)?);
    }
    let report = delete::delete_many(&targets, dry_run);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "dry_run": dry_run,
                "deleted": report.deleted,
                "bytes": report.bytes,
                "errors": report.errors,
                "warnings": report.warnings,
            }))?
        );
    } else {
        for session in &targets {
            let status = if dry_run {
                "将删除"
            } else if report.failed.contains(&session.key()) {
                "跳过"
            } else {
                "已删除"
            };
            println!(
                "{status} {} · {}（{}）",
                session.client.label(),
                session.title,
                util::human_bytes(session.total_bytes())
            );
        }
        println!(
            "\n{} 个 · 释放 {}{}",
            report.deleted,
            util::human_bytes(report.bytes),
            if dry_run { "（演练）" } else { "" }
        );
        for warning in &report.warnings {
            println!("提示：{warning}");
        }
        for error in &report.errors {
            println!("错误：{error}");
        }
    }

    if report.failed() {
        std::process::exit(1);
    }
    Ok(())
}

fn gc_cmd(dry_run: bool, json: bool) -> Result<()> {
    let report = crate::gc::run(dry_run);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "dry_run": report.dry_run,
                "registry": report.registry,
                "projcache": report.projcache,
                "bili_records": report.bili_records,
                "bili_bytes": report.bili_bytes,
                "warnings": report.warnings,
            }))?
        );
        return Ok(());
    }
    let verb = if dry_run { "可清理" } else { "已清理" };
    if report.is_empty() {
        println!("没有发现残留记录，{verb} 0 项。");
        return Ok(());
    }
    println!("{verb}：");
    println!("  工作区登记  {} 条", report.registry);
    println!("  渲染缓存    {} 个", report.projcache);
    println!(
        "  billion-context  {} 条（{}）",
        report.bili_records,
        util::human_bytes(report.bili_bytes)
    );
    for warning in &report.warnings {
        println!("  注意：{warning}");
    }
    for sample in &report.samples {
        println!("  例：{}", util::truncate(sample, 40));
    }
    if dry_run {
        println!("\n去掉 --dry-run 即真正清理。");
    }
    Ok(())
}

fn doctor() -> Result<()> {
    let snapshot = scan::scan();
    println!("扫描结果\n");
    if snapshot.roots.is_empty() {
        println!("  没有找到任何根目录。");
    }
    for root in &snapshot.roots {
        println!(
            "  [{}] {}\n        会话 {} 个{}",
            root.client.key(),
            root.path.display(),
            root.sessions,
            root.error
                .as_ref()
                .map(|error| format!("\n        错误：{error}"))
                .unwrap_or_default()
        );
    }
    let groups = workspace_groups(&snapshot, None);
    let bytes: u64 = snapshot.sessions.iter().map(Session::total_bytes).sum();
    let bili: u64 = snapshot.sessions.iter().map(Session::bili_bytes).sum();
    println!(
        "\n合计 {} 个会话 · {} 个工作区 · {}",
        snapshot.sessions.len(),
        groups.len(),
        util::human_bytes(bytes)
    );
    if bili > 0 {
        println!("其中 billion-context 数据 {}", util::human_bytes(bili));
    }
    let magic: u32 = snapshot
        .sessions
        .iter()
        .map(Session::magic_compartments)
        .sum();
    if magic > 0 {
        println!("其中 magic-context 记录 {magic} 个隔间");
    }

    let zstd = which("zstd");
    println!(
        "\n依赖\n  zstd 二进制：{}（仅在内置解码失败时兜底）",
        zstd.map(|path| path.display().to_string())
            .unwrap_or_else(|| "未找到".to_string())
    );
    for root in &snapshot.roots {
        if !root.path.exists() {
            println!("  {} 不存在", root.path.display());
            continue;
        }
        // Opencode 的“根”是数据库文件，写探测落在父目录。
        let target = if root.path.is_dir() {
            root.path.clone()
        } else {
            root.path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default()
        };
        let writable = probe_writable(&target);
        println!(
            "  {} 可写：{}",
            root.path.display(),
            if writable { "是" } else { "否" }
        );
        if root.client == Client::Codex {
            match crate::codex::find_state_db(&root.path) {
                Some(db) => println!("        会话库：{}", db.display()),
                None => println!("        会话库：未找到 state_*.sqlite（只会扫 rollout 文件）"),
            }
        }
    }
    for warning in &snapshot.warnings {
        println!("  警告：{warning}");
    }

    println!("\nbillion-context");
    match &snapshot.bili {
        Some(summary) if summary.known() => {
            for database in &summary.databases {
                println!("  数据库：{}", database.display());
                if let Some(modified) = bili::modified(database) {
                    println!("        最后写入：{}", util::local_datetime(modified));
                }
                let side: Vec<String> = bili::side_files(database)
                    .iter()
                    .filter_map(|path| path.file_name())
                    .map(|name| name.to_string_lossy().to_string())
                    .collect();
                if !side.is_empty() {
                    println!("        伴生文件：{}", side.join(" · "));
                }
            }
            println!(
                "  关联 DSH 会话 {} 个 · 占用 {}",
                summary.sessions(),
                util::human_bytes(summary.bytes)
            );
            if summary.orphans > 0 {
                println!(
                    "  残留 {} 个：bili 库里还有、DSH 侧已找不到的会话记录（经 asclean 删除的会话不会留下这种记录）。",
                    summary.orphans
                );
            }
            for error in &summary.errors {
                println!("  错误：{error}");
            }
        }
        Some(summary) => {
            for error in &summary.errors {
                println!("  错误：{error}");
            }
        }
        None => println!("  未检测到（没有找到 sessions/index.db）"),
    }

    println!("\nmagic-context");
    match &snapshot.magic {
        Some(summary) if summary.known() => {
            for database in &summary.databases {
                println!("  数据库：{}", database.display());
                if let Some(modified) = magic::modified(database) {
                    println!("        最后写入：{}", util::local_datetime(modified));
                }
                let side: Vec<String> = magic::side_files(database)
                    .iter()
                    .filter_map(|path| path.file_name())
                    .map(|name| name.to_string_lossy().to_string())
                    .collect();
                if !side.is_empty() {
                    println!("        伴生文件：{}", side.join(" · "));
                }
            }
            println!(
                "  关联 Opencode 会话 {} 个 · 隔间 {} 个",
                summary.sessions(),
                summary.compartments
            );
            if summary.orphans > 0 {
                println!(
                    "  残留 {} 个：magic-context 里还有、opencode 侧已找不到的会话记录（经 asclean 删除的会话不会留下这种记录）。",
                    summary.orphans
                );
            }
            for error in &summary.errors {
                println!("  错误：{error}");
            }
        }
        Some(summary) => {
            for error in &summary.errors {
                println!("  错误：{error}");
            }
        }
        None => println!("  未检测到（没有找到 cortexkit/magic-context/context.db）"),
    }

    println!("\n配置：{}", Config::path().display());
    Ok(())
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn probe_writable(path: &Path) -> bool {
    let probe = path.join(".asclean-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn paths(json: bool) -> Result<()> {
    let snapshot = scan::scan();
    if json {
        let items: Vec<_> = snapshot
            .roots
            .iter()
            .map(|root| {
                json!({
                    "client": root.client.key(),
                    "root": root.path.display().to_string(),
                    "sessions": root.sessions,
                    "error": root.error,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        for root in &snapshot.roots {
            println!(
                "{:<10} {}  ({} 个)",
                root.client.key(),
                root.path.display(),
                root.sessions
            );
        }
    }
    Ok(())
}

fn config_cmd(init: bool, force: bool, json: bool) -> Result<()> {
    if init {
        let path = Config::init(force)?;
        println!("已写入 {}", path.display());
        return Ok(());
    }
    let path = Config::path();
    let config = Config::load()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&config)?);
        return Ok(());
    }
    println!(
        "配置：{}（{}）",
        path.display(),
        if path.exists() { "存在" } else { "默认值" }
    );
    println!("  theme            = {}", config.theme);
    println!("  mouse            = {}", config.mouse);
    println!("  rescan_secs      = {}", config.rescan_secs);
    println!("  confirm_delete   = {}", config.confirm_delete);
    Ok(())
}
