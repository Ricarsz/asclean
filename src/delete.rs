//! 删除：按客户端各自的存储方式清理，并做路径与锁的安全检查。

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::model::{Client, Session, Workspace};
use crate::util;

pub struct Report {
    pub deleted: usize,
    pub bytes: u64,
    /// 失败会话的 `client:id`，供界面精确清理勾选状态。
    pub failed: Vec<String>,
    pub errors: Vec<String>,
    /// 会话已删除、只是附带清理没做完的情况。
    pub warnings: Vec<String>,
}

impl Report {
    pub fn failed(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// 删除若干个已选会话；`dry_run` 只统计不落盘。
pub fn delete_many(sessions: &[Session], dry_run: bool) -> Report {
    let mut report = Report {
        deleted: 0,
        bytes: 0,
        failed: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    for session in sessions {
        let outcome = if dry_run {
            Ok(Outcome {
                bytes: session.total_bytes(),
                warnings: Vec::new(),
            })
        } else {
            delete(session)
        };
        match outcome {
            Ok(outcome) => {
                report.deleted += 1;
                report.bytes += outcome.bytes;
                report.warnings.extend(outcome.warnings);
            }
            Err(error) => {
                report.failed.push(session.key());
                report.errors.push(format!(
                    "{} · {}：{error}",
                    session.client.label(),
                    util::truncate(&session.title, 40)
                ));
            }
        }
    }
    report
}

/// 一次删除的结果：释放的字节数，以及不影响会话删除完成度的附带问题。
pub struct Outcome {
    pub bytes: u64,
    pub warnings: Vec<String>,
}

pub fn delete(session: &Session) -> Result<Outcome> {
    match session.client {
        Client::Dsh => delete_dsh(session),
        Client::Codex => delete_codex(session),
        Client::Opencode => delete_opencode(session),
    }
}

fn delete_dsh(session: &Session) -> Result<Outcome> {
    let dir = &session.path;
    let allowed = session.root.join("sessions");
    ensure_inside(dir, &allowed)?;
    if !dsh_session_dir(dir) {
        bail!("不是会话目录：{}", dir.display());
    }
    if util::lock_held(&dir.join("session.lock")) {
        bail!("会话正在使用中（session.lock 被持有），已跳过");
    }
    let bytes = util::dir_size(dir);
    fs::remove_dir_all(dir).with_context(|| format!("删除 {}", dir.display()))?;
    drop_dsh_projcache(&session.root, &session.id);
    // 工作区登记表里还挂着这个会话 id，留着会让 TUI / doctor 一直看到“登记了但目录没了”。
    let deregistered = deregister_dsh_session(&session.root, &session.id).is_ok();
    // 工作区目录空了就一并收掉，避免残留空壳。
    if let Some(parent) = dir.parent()
        && let Ok(mut entries) = fs::read_dir(parent)
        && entries.next().is_none()
    {
        let _ = fs::remove_dir(parent);
    }
    // billion-context 用同一个会话 id 记账；DSH 删掉后一起清，避免留下孤儿。
    let mut warnings = Vec::new();
    let mut bytes = bytes;
    if !deregistered {
        warnings.push("工作区登记表没更新，可能被其他进程占用".to_string());
    }
    if let Some(meta) = &session.bili {
        match crate::bili::delete_session(&meta.db, &session.id) {
            Ok(extra) => bytes += extra,
            Err(error) => warnings.push(format!(
                "DSH 会话已删除，但 billion-context 数据没清理干净：{error:#}"
            )),
        }
    }
    Ok(Outcome { bytes, warnings })
}

fn delete_codex(session: &Session) -> Result<Outcome> {
    let mut bytes = 0;
    let mut removed = false;
    // 老版本只有 rollout 文件；新版本的会话正文在 SQLite 里，文件可能不存在。
    let file = &session.path;
    if let Ok(meta) = fs::metadata(file) {
        ensure_inside(file, &session.root.join("sessions"))?;
        if !file
            .file_name()
            .map(|name| {
                let name = name.to_string_lossy();
                name.starts_with("rollout-") && name.ends_with(".jsonl")
            })
            .unwrap_or(false)
        {
            bail!("不是 rollout 文件：{}", file.display());
        }
        bytes += meta.len();
        fs::remove_file(file).with_context(|| format!("删除 {}", file.display()))?;
        removed = true;
    }
    let threads = crate::codex::delete_thread(&session.root, &session.id)?;
    bytes += threads.bytes;
    removed |= threads.rows > 0;
    if !removed {
        bail!("没有找到该会话的存储（rollout 文件与 Codex 会话库都没有）");
    }
    drop_codex_index(&session.root.join("session_index.jsonl"), &session.id)?;
    Ok(Outcome {
        bytes,
        warnings: Vec::new(),
    })
}

/// 从 `session_index.jsonl` 里摘掉对应条目，写回时用临时文件替换。
fn drop_codex_index(index: &Path, id: &str) -> Result<()> {
    let Ok(text) = fs::read_to_string(index) else {
        return Ok(());
    };
    let mut kept = String::with_capacity(text.len());
    for line in text.lines() {
        let matches = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|value| {
                value
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(|value| value == id)
            })
            .unwrap_or(false);
        if !matches {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    if kept.len() == text.len() {
        return Ok(());
    }
    let tmp = index.with_extension("jsonl.tmp");
    {
        let mut file = fs::File::create(&tmp).with_context(|| format!("写入 {}", tmp.display()))?;
        file.write_all(kept.as_bytes())?;
        file.sync_all().ok();
    }
    fs::rename(&tmp, index).with_context(|| format!("替换 {}", index.display()))?;
    Ok(())
}

fn delete_opencode(session: &Session) -> Result<Outcome> {
    let db = &session.path;
    if db
        .file_name()
        .map(|name| name != "opencode.db")
        .unwrap_or(true)
    {
        bail!("不是 opencode 数据库：{}", db.display());
    }
    let conn = rusqlite::Connection::open(db).with_context(|| format!("打开 {}", db.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = conn.unchecked_transaction()?;
    let removed = tx.execute("DELETE FROM session_v2 WHERE id = ?1", [&session.id])?;
    tx.commit()?;
    if removed == 0 {
        bail!("数据库里没有该会话，可能已被删除");
    }
    // magic-context 用同一个会话 id 记账；它在 opencode 里收到删除事件才
    // 会自己清，asclean 直接改库不产生事件，所以这里补一刀。
    let mut warnings = Vec::new();
    if let Some(meta) = &session.magic
        && let Err(error) = crate::magic::delete_session(&meta.db, &session.id)
    {
        warnings.push(format!(
            "Opencode 会话已删除，但 magic-context 数据没清理干净：{error:#}"
        ));
    }
    Ok(Outcome {
        bytes: session.bytes,
        warnings,
    })
}

/// 删除整个工作区：先逐个删会话，全部成功后再注销工作区登记。
///
/// 只清理会话存储与登记项，**不会**碰工作区目录本身里的项目文件。
pub struct WorkspaceReport {
    pub deleted: usize,
    pub bytes: u64,
    /// 因为锁或路径校验被跳过的会话数。
    pub skipped: usize,
    pub errors: Vec<String>,
    /// 会话删掉了、附带清理没做完的提示。
    pub warnings: Vec<String>,
    /// 是否把登记项（DSH 的 workspace.json、Opencode 的 project 行）也移除了。
    pub deregistered: bool,
}

impl WorkspaceReport {
    pub fn failed(&self) -> bool {
        !self.errors.is_empty()
    }
}

pub fn delete_workspace(
    sessions: &[Session],
    workspace: Option<&Workspace>,
    dry_run: bool,
) -> WorkspaceReport {
    let mut report = WorkspaceReport {
        deleted: 0,
        bytes: 0,
        skipped: 0,
        errors: Vec::new(),
        warnings: Vec::new(),
        deregistered: false,
    };
    for session in sessions {
        let outcome = if dry_run {
            Ok(Outcome {
                bytes: session.total_bytes(),
                warnings: Vec::new(),
            })
        } else {
            delete(session)
        };
        match outcome {
            Ok(outcome) => {
                report.deleted += 1;
                report.bytes += outcome.bytes;
                report.warnings.extend(outcome.warnings);
            }
            Err(error) => {
                report.skipped += 1;
                report.errors.push(format!(
                    "{} · {}：{error}",
                    session.client.label(),
                    util::truncate(&session.title, 40)
                ));
            }
        }
    }
    if dry_run || report.failed() {
        return report;
    }
    let Some(workspace) = workspace else {
        return report;
    };
    let outcome = match workspace.client {
        Client::Dsh => deregister_dsh_workspace(&workspace.root, &workspace.id),
        Client::Opencode => deregister_opencode_project(&workspace.root, &workspace.id),
        // Codex 没有工作区登记表，删掉 rollout 文件就等于工作区里没有会话了。
        Client::Codex => Ok(false),
    };
    match outcome {
        Ok(removed) => report.deregistered = removed,
        Err(error) => report.errors.push(format!("注销工作区失败：{error:#}")),
    }
    report
}

/// 会话目录的判据：目录名是 `session-<uuid>` 或裸 `<uuid>`（老版本 DSH），
/// 或者目录里躺着会话日志 / 锁文件。只看名字会把老会话全都拒掉。
fn dsh_session_dir(dir: &Path) -> bool {
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    if util::looks_like_dsh_id(&name) {
        return true;
    }
    fs::read_dir(dir)
        .map(|entries| {
            entries.flatten().any(|entry| {
                let file = entry.file_name().to_string_lossy().to_string();
                file == "session.lock" || (file.starts_with("session") && file.contains(".jsonl"))
            })
        })
        .unwrap_or(false)
}

/// DSH 为每个会话缓存一份渲染状态，删会话后一并清掉，避免留下孤儿文件。
fn drop_dsh_projcache(root: &Path, id: &str) {
    let sessions = root
        .join("storages")
        .join("session_projcache")
        .join("sessions");
    for candidate in [id.to_string(), format!("session-{}", util::dsh_core(id))] {
        let _ = fs::remove_file(sessions.join(format!("{candidate}.json")));
    }
}

/// 从 `storages/workspace.json` 的所有 `sessionIds` 与全局收藏/归档列表里
/// 摘掉一个会话；`session-` 前缀的写法差异一并抹平。
pub fn deregister_dsh_session(root: &Path, id: &str) -> Result<bool> {
    let (removed, _) = prune_registry_with(root, &|candidate| !util::dsh_same_id(candidate, id))?;
    Ok(removed > 0)
}

/// 登记表里已经没有对应目录的会话 id（不落盘，供清理预览用）。
pub fn dangling_registry(root: &Path, present: &HashSet<String>) -> Result<Vec<String>> {
    let path = root.join("storages").join("workspace.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("读取 {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("解析 {}", path.display()))?;
    let mut out = Vec::new();
    collect_registry_ids(&value, &mut out);
    out.retain(|id| is_dangling(id, present));
    Ok(out)
}

/// 把登记表里没有对应目录的会话 id 全部摘掉，返回摘掉的条数。
pub fn prune_registry(root: &Path, present: &HashSet<String>) -> Result<usize> {
    let (removed, _) = prune_registry_with(root, &|id| !is_dangling(id, present))?;
    Ok(removed)
}

/// 只认 DSH 形状的 id：`dsh-pet` 之类别的客户端记录一律不动。
fn is_dangling(id: &str, present: &HashSet<String>) -> bool {
    util::looks_like_dsh_id(id) && !present.contains(util::dsh_core(id))
}

fn collect_registry_ids(value: &serde_json::Value, out: &mut Vec<String>) {
    if let Some(table) = value
        .pointer("/tables/workspaces")
        .and_then(serde_json::Value::as_object)
    {
        for record in table.values() {
            if let Some(ids) = record
                .get("sessionIds")
                .and_then(serde_json::Value::as_array)
            {
                out.extend(
                    ids.iter()
                        .filter_map(|item| item.as_str())
                        .map(str::to_string),
                );
            }
        }
    }
    if let Some(global) = value
        .pointer("/global")
        .and_then(serde_json::Value::as_object)
    {
        for key in ["archivedSessionIds", "pinnedSessionIds"] {
            if let Some(ids) = global.get(key).and_then(serde_json::Value::as_array) {
                out.extend(
                    ids.iter()
                        .filter_map(|item| item.as_str())
                        .map(str::to_string),
                );
            }
        }
    }
}

/// 按 `keep` 逐条过滤登记表里的会话 id，返回（摘掉的条数, 摘掉的 id）。
fn prune_registry_with(root: &Path, keep: &dyn Fn(&str) -> bool) -> Result<(usize, Vec<String>)> {
    let path = root.join("storages").join("workspace.json");
    if !path.is_file() {
        return Ok((0, Vec::new()));
    }
    let text = fs::read_to_string(&path).with_context(|| format!("读取 {}", path.display()))?;
    let mut value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("解析 {}", path.display()))?;
    let mut removed: Vec<String> = Vec::new();
    if let Some(table) = value
        .pointer_mut("/tables/workspaces")
        .and_then(serde_json::Value::as_object_mut)
    {
        for record in table.values_mut() {
            if let Some(ids) = record
                .get_mut("sessionIds")
                .and_then(serde_json::Value::as_array_mut)
            {
                ids.retain(|item| match item.as_str() {
                    Some(id) if !keep(id) => {
                        removed.push(id.to_string());
                        false
                    }
                    _ => true,
                });
            }
        }
    }
    if let Some(global) = value
        .pointer_mut("/global")
        .and_then(serde_json::Value::as_object_mut)
    {
        for key in ["archivedSessionIds", "pinnedSessionIds"] {
            if let Some(ids) = global
                .get_mut(key)
                .and_then(serde_json::Value::as_array_mut)
            {
                ids.retain(|item| match item.as_str() {
                    Some(id) if !keep(id) => {
                        removed.push(id.to_string());
                        false
                    }
                    _ => true,
                });
            }
        }
    }
    if removed.is_empty() {
        return Ok((0, removed));
    }
    write_json_atomic(&path, &value)?;
    Ok((removed.len(), removed))
}

/// 从 `storages/workspace.json` 里摘掉一个工作区，并清理指向它的全局列表。
fn deregister_dsh_workspace(root: &Path, id: &str) -> Result<bool> {
    let path = root.join("storages").join("workspace.json");
    if !path.is_file() {
        return Ok(false);
    }
    let text = fs::read_to_string(&path).with_context(|| format!("读取 {}", path.display()))?;
    let mut value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("解析 {}", path.display()))?;

    let session_ids = {
        let Some(table) = value
            .pointer_mut("/tables/workspaces")
            .and_then(serde_json::Value::as_object_mut)
        else {
            return Ok(false);
        };
        let Some(record) = table.remove(id) else {
            return Ok(false);
        };
        let session_ids: Vec<String> = record
            .get("sessionIds")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect();
        // 会话还没删干净就保留登记，避免把仍在使用中的会话变成孤儿。
        if let Some(name) = dsh_present_session(root, &session_ids) {
            bail!("工作区里还有会话 {name}，已保留登记");
        }
        session_ids
    };

    if let Some(global) = value
        .pointer_mut("/global")
        .and_then(serde_json::Value::as_object_mut)
    {
        if let Some(ids) = global
            .get_mut("workspaceIds")
            .and_then(serde_json::Value::as_array_mut)
        {
            ids.retain(|item| item.as_str() != Some(id));
        }
        for key in ["archivedSessionIds", "pinnedSessionIds"] {
            if let Some(ids) = global
                .get_mut(key)
                .and_then(serde_json::Value::as_array_mut)
            {
                ids.retain(|item| {
                    item.as_str()
                        .is_none_or(|item| !session_ids.iter().any(|id| id == item))
                });
            }
        }
        let default = global
            .get("defaultWorkspaceId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        if default.as_deref() == Some(id) {
            let next = global
                .get("workspaceIds")
                .and_then(serde_json::Value::as_array)
                .and_then(|ids| ids.first())
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            global.insert("defaultWorkspaceId".into(), next);
        }
    }

    write_json_atomic(&path, &value)?;
    Ok(true)
}

/// 这些会话 id 里是否还有目录留在 `sessions/<bucket>/` 下。
fn dsh_present_session(root: &Path, session_ids: &[String]) -> Option<String> {
    if session_ids.is_empty() {
        return None;
    }
    let entries = fs::read_dir(root.join("sessions")).ok()?;
    for bucket in entries.flatten() {
        let Ok(sessions) = fs::read_dir(bucket.path()) else {
            continue;
        };
        for entry in sessions.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if session_ids
                .iter()
                .any(|candidate| util::dsh_same_id(candidate, &name))
            {
                return Some(name);
            }
        }
    }
    None
}

fn deregister_opencode_project(root: &Path, project_id: &str) -> Result<bool> {
    let db = root.join("opencode.db");
    if !db.is_file() {
        return Ok(false);
    }
    let conn = rusqlite::Connection::open(&db).with_context(|| format!("打开 {}", db.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    let remaining: i64 = conn.query_row(
        "SELECT COUNT(*) FROM session_v2 WHERE project_id = ?1",
        [project_id],
        |row| row.get(0),
    )?;
    if remaining > 0 {
        bail!("项目里还有 {remaining} 个会话，已保留登记");
    }
    let tx = conn.unchecked_transaction()?;
    // 外键级联会带走该项目名下的会话与消息。
    let removed = tx.execute("DELETE FROM project WHERE id = ?1", [project_id])?;
    tx.commit()?;
    Ok(removed > 0)
}

/// 原子写 JSON：先写临时文件再 rename，避免客户端读到半截文件。
fn write_json_atomic(path: &Path, value: &serde_json::Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    let mut tmp = path.to_path_buf();
    tmp.set_extension("json.tmp");
    {
        let mut file = fs::File::create(&tmp).with_context(|| format!("写入 {}", tmp.display()))?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all().ok();
    }
    fs::rename(&tmp, path).with_context(|| format!("替换 {}", path.display()))?;
    Ok(())
}

/// 规范化后确认 `child` 位于 `parent` 内，防止脏数据导致误删。
fn ensure_inside(child: &Path, parent: &Path) -> Result<()> {
    let child = child
        .canonicalize()
        .with_context(|| format!("解析 {}", child.display()))?;
    let parent = parent
        .canonicalize()
        .with_context(|| format!("解析 {}", parent.display()))?;
    if !child.starts_with(&parent) {
        bail!(
            "拒绝删除 {}：不在 {} 之内",
            child.display(),
            parent.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BiliMeta, MagicMeta};
    use rusqlite::Connection;
    use std::path::{Path, PathBuf};
    use std::time::UNIX_EPOCH;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default();
            let path = std::env::temp_dir().join(format!("asclean-{name}-{stamp}"));
            fs::create_dir_all(&path).expect("建目录");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn session_dir(root: &Path, id: &str) -> PathBuf {
        let dir = root.join("sessions").join("--tmp--").join(id);
        fs::create_dir_all(&dir).expect("建会话目录");
        fs::write(dir.join("session.jsonl.zstd"), vec![7u8; 2048]).expect("写会话文件");
        dir
    }

    /// 一份贴近真实结构的 bili 库：`target` 与 `keep` 两条会话，共享一条 content。
    fn bili_db(path: &Path) {
        fs::create_dir_all(path.parent().expect("父目录")).expect("建目录");
        let conn = Connection::open(path).expect("建库");
        conn.execute_batch(
            "CREATE TABLE sessions (session_id TEXT PRIMARY KEY, status TEXT NOT NULL,
                updated_at INTEGER NOT NULL);
             CREATE TABLE messages (session_id TEXT NOT NULL, content_hash TEXT NOT NULL);
             CREATE TABLE blocks (session_id TEXT NOT NULL, summary_hash TEXT NOT NULL,
                one_hash TEXT);
             CREATE TABLE message_media (session_id TEXT NOT NULL, media_hash TEXT NOT NULL);
             CREATE TABLE refs (session_id TEXT NOT NULL);
             CREATE TABLE dead_refs (session_id TEXT NOT NULL);
             CREATE TABLE ccr_entries (session_id TEXT NOT NULL, content_hash TEXT NOT NULL);
             CREATE TABLE checkpoints (session_id TEXT NOT NULL, state_hash TEXT NOT NULL);
             CREATE TABLE session_ext (session_id TEXT NOT NULL);
             CREATE TABLE conversations (session_id TEXT NOT NULL);
             CREATE TABLE content (hash TEXT PRIMARY KEY, byte_len INTEGER NOT NULL);
             INSERT INTO sessions VALUES ('session-target', 'active', 1700000000000);
             INSERT INTO sessions VALUES ('session-keep', 'active', 1700000000000);
             INSERT INTO messages VALUES ('session-target', 'h-shared');
             INSERT INTO messages VALUES ('session-target', 'h-target');
             INSERT INTO messages VALUES ('session-keep', 'h-shared');
             INSERT INTO content VALUES ('h-shared', 100);
             INSERT INTO content VALUES ('h-target', 50);",
        )
        .expect("建表");
    }

    fn dsh_session(root: &Path, dir: PathBuf, db: PathBuf) -> Session {
        Session {
            client: Client::Dsh,
            id: "session-target".to_string(),
            title: "测试会话".to_string(),
            preview: String::new(),
            path: dir,
            root: root.to_path_buf(),
            cwd: None,
            workspace: None,
            created: None,
            updated: None,
            bytes: 0,
            messages: 0,
            tokens: None,
            cost: None,
            locked: false,
            bili: Some(BiliMeta {
                db,
                bytes: 150,
                messages: 2,
                updated: None,
                status: "active".to_string(),
            }),
            subagent: false,
            parent: None,
            magic: None,
        }
    }

    /// Codex 新版本：会话在 `state_5.sqlite` / `thread_history_1.sqlite` 里。
    fn codex_db_fixture(scratch: &Scratch) -> PathBuf {
        let root = scratch.path().join("codex");
        fs::create_dir_all(root.join("sessions")).expect("建目录");
        let state = Connection::open(root.join("state_5.sqlite")).expect("建 state 库");
        state
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL,
                    cwd TEXT NOT NULL, title TEXT NOT NULL, created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL);
                 INSERT INTO threads VALUES ('target-id', '/tmp/gone.jsonl', '/work',
                    'target', 1700000000, 1700000100);
                 INSERT INTO threads VALUES ('keep-id', '/tmp/keep.jsonl', '/work',
                    'keep', 1700000000, 1700000100);",
            )
            .expect("建表");
        let history = Connection::open(root.join("thread_history_1.sqlite")).expect("建历史库");
        history
            .execute_batch(
                "CREATE TABLE thread_items (thread_id TEXT NOT NULL, turn_id TEXT NOT NULL,
                    item_id TEXT NOT NULL, item_json TEXT NOT NULL, item_type TEXT NOT NULL,
                    PRIMARY KEY (thread_id, turn_id, item_id));
                 INSERT INTO thread_items VALUES ('target-id', 't', 'i', '{\"x\":1}', 'userMessage');
                 INSERT INTO thread_items VALUES ('keep-id', 't', 'i', '{\"x\":2}', 'userMessage');",
            )
            .expect("建表");
        root
    }

    fn codex_session(root: &Path, id: &str, path: PathBuf) -> Session {
        Session {
            client: Client::Codex,
            id: id.to_string(),
            title: "测试会话".to_string(),
            preview: String::new(),
            path,
            root: root.to_path_buf(),
            cwd: Some("/work".to_string()),
            workspace: None,
            created: None,
            updated: None,
            bytes: 9,
            messages: 1,
            tokens: None,
            cost: None,
            locked: false,
            bili: None,
            subagent: false,
            parent: None,
            magic: None,
        }
    }

    #[test]
    fn deletes_codex_session_from_the_database() {
        let scratch = Scratch::new("delete-codex-db");
        let root = codex_db_fixture(&scratch);
        // 新版本里 rollout 文件已经不在了，只剩库里的记录。
        let session = codex_session(&root, "target-id", root.join("sessions").join("gone.jsonl"));

        let outcome = delete(&session).expect("删除");
        assert_eq!(outcome.bytes, 7, "释放量按 thread_items 的正文大小算");

        let state = Connection::open(root.join("state_5.sqlite")).expect("打开 state 库");
        let left: i64 = state
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = 'target-id'",
                [],
                |row| row.get(0),
            )
            .expect("计数");
        assert_eq!(left, 0);
        let keep: i64 = state
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = 'keep-id'",
                [],
                |row| row.get(0),
            )
            .expect("计数");
        assert_eq!(keep, 1, "别的会话不受影响");
        let history = Connection::open(root.join("thread_history_1.sqlite")).expect("打开历史库");
        let items: i64 = history
            .query_row(
                "SELECT COUNT(*) FROM thread_items WHERE thread_id = 'target-id'",
                [],
                |row| row.get(0),
            )
            .expect("计数");
        assert_eq!(items, 0);
    }

    #[test]
    fn deletes_codex_rollout_file_and_database_rows_together() {
        let scratch = Scratch::new("delete-codex-file");
        let root = codex_db_fixture(&scratch);
        let file = root
            .join("sessions")
            .join("rollout-2026-01-01T00-00-00-target-id.jsonl");
        fs::write(&file, vec![1u8; 64]).expect("写 rollout");
        let session = codex_session(&root, "target-id", file.clone());

        let outcome = delete(&session).expect("删除");
        assert!(!file.exists());
        assert_eq!(outcome.bytes, 64 + 7);
        let state = Connection::open(root.join("state_5.sqlite")).expect("打开");
        let left: i64 = state
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = 'target-id'",
                [],
                |row| row.get(0),
            )
            .expect("计数");
        assert_eq!(left, 0);
    }

    #[test]
    fn rejects_missing_codex_session() {
        let scratch = Scratch::new("delete-codex-missing");
        let root = codex_db_fixture(&scratch);
        let session = codex_session(
            &root,
            "unknown-id",
            root.join("sessions").join("gone.jsonl"),
        );
        let error = match delete(&session) {
            Ok(_) => panic!("应该拒绝删除"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("没有找到"), "{error}");
    }

    #[test]
    fn rejects_paths_outside_root() {
        let error = ensure_inside(Path::new("/etc"), Path::new("/tmp")).unwrap_err();
        assert!(error.to_string().contains("拒绝删除"));
    }

    #[test]
    fn deletes_dsh_session_and_its_bili_rows() {
        let scratch = Scratch::new("delete-bili");
        let root = scratch.path().join("dsh");
        let dir = session_dir(&root, "session-target");
        let db = scratch
            .path()
            .join("bili")
            .join("sessions")
            .join("index.db");
        bili_db(&db);
        let session = dsh_session(&root, dir.clone(), db.clone());

        let outcome = delete(&session).expect("删除");

        assert!(!dir.exists(), "会话目录应该被删除");
        assert_eq!(outcome.bytes, 2048 + 150, "释放量应含 bili 部分");
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        let conn = Connection::open(&db).expect("打开");
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .expect("计数");
        assert_eq!(sessions, 1, "只应删掉目标会话的记录");
        let content: i64 = conn
            .query_row("SELECT COUNT(*) FROM content", [], |row| row.get(0))
            .expect("计数");
        assert_eq!(content, 1, "共享 content 应保留，目标独有的应回收");
    }

    #[test]
    fn dsh_delete_survives_a_broken_bili_database() {
        let scratch = Scratch::new("delete-bili-broken");
        let root = scratch.path().join("dsh");
        let dir = session_dir(&root, "session-target");
        // 文件名不是 index.db，bili 删除会拒绝；DSH 会话仍应删掉并给出提示。
        let db = scratch
            .path()
            .join("bili")
            .join("sessions")
            .join("other.db");
        bili_db(&db);
        let session = dsh_session(&root, dir.clone(), db);

        let outcome = delete(&session).expect("删除");

        assert!(!dir.exists());
        assert_eq!(outcome.bytes, 2048);
        assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
        assert!(outcome.warnings[0].contains("billion-context"));
    }

    /// opencode 库与 magic-context 库各有一条 `target` 与 `keep` 会话。
    fn opencode_fixture(scratch: &Scratch) -> (PathBuf, PathBuf) {
        let db = scratch.path().join("opencode").join("opencode.db");
        fs::create_dir_all(db.parent().expect("父目录")).expect("建目录");
        let conn = Connection::open(&db).expect("建 opencode 库");
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, title TEXT);
             INSERT INTO session_v2 VALUES ('ses_target', 'target');
             INSERT INTO session_v2 VALUES ('ses_keep', 'keep');",
        )
        .expect("建表");
        let magic = scratch
            .path()
            .join("magic")
            .join("cortexkit")
            .join("magic-context")
            .join("context.db");
        fs::create_dir_all(magic.parent().expect("父目录")).expect("建目录");
        let conn = Connection::open(&magic).expect("建 magic 库");
        conn.execute_batch(
            "CREATE TABLE session_meta (session_id TEXT PRIMARY KEY, harness TEXT,
                last_response_time INTEGER NOT NULL);
             CREATE TABLE compartments (session_id TEXT NOT NULL, seq INTEGER NOT NULL);
             CREATE TABLE tags (session_id TEXT NOT NULL, tag TEXT NOT NULL);
             CREATE TABLE schema_migrations_meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO session_meta VALUES ('ses_target', 'opencode2', 1700000000000);
             INSERT INTO session_meta VALUES ('ses_keep', 'opencode2', 1700000000000);
             INSERT INTO compartments VALUES ('ses_target', 0);
             INSERT INTO compartments VALUES ('ses_keep', 0);
             INSERT INTO tags VALUES ('ses_target', 'a');
             INSERT INTO schema_migrations_meta VALUES ('schema', 'v95');",
        )
        .expect("建表");
        (db, magic)
    }

    fn opencode_session(db: PathBuf, magic: PathBuf) -> Session {
        Session {
            client: Client::Opencode,
            id: "ses_target".to_string(),
            title: "opencode 会话".to_string(),
            preview: String::new(),
            path: db,
            root: PathBuf::from("/tmp"),
            cwd: None,
            workspace: None,
            created: None,
            updated: None,
            bytes: 0,
            messages: 0,
            tokens: None,
            cost: None,
            locked: false,
            subagent: false,
            parent: None,
            bili: None,
            magic: Some(MagicMeta {
                db: magic,
                compartments: 1,
                updated: None,
                status: "opencode2".to_string(),
            }),
        }
    }

    #[test]
    fn deletes_opencode_session_and_its_magic_rows() {
        let scratch = Scratch::new("delete-magic");
        let (db, magic_db) = opencode_fixture(&scratch);
        let session = opencode_session(db.clone(), magic_db.clone());

        let outcome = delete(&session).expect("删除");

        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        let conn = Connection::open(&db).expect("打开 opencode");
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_v2", [], |row| row.get(0))
            .expect("计数");
        assert_eq!(left, 1, "只该删掉 target");
        let conn = Connection::open(&magic_db).expect("打开 magic");
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_meta", [], |row| row.get(0))
            .expect("计数");
        assert_eq!(sessions, 1);
        let tags: i64 = conn
            .query_row("SELECT COUNT(*) FROM tags", [], |row| row.get(0))
            .expect("计数");
        assert_eq!(tags, 0);
    }

    #[test]
    fn opencode_delete_survives_a_broken_magic_database() {
        let scratch = Scratch::new("delete-magic-broken");
        let (db, magic_db) = opencode_fixture(&scratch);
        // 文件名不是 context.db，magic 删除会拒绝；opencode 会话仍应删掉并给出提示。
        let broken = magic_db.with_file_name("other.db");
        fs::rename(&magic_db, &broken).expect("改名");
        let session = opencode_session(db.clone(), broken);

        let outcome = delete(&session).expect("删除");

        assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);
        assert!(outcome.warnings[0].contains("magic-context"));
        let conn = Connection::open(&db).expect("打开 opencode");
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_v2", [], |row| row.get(0))
            .expect("计数");
        assert_eq!(left, 1);
    }
}
