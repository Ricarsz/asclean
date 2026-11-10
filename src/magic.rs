//! magic-context 的会话存储（`context.db`）：探测、统计与连带删除。
//!
//! magic-context 是 opencode 的上下文/记忆插件，把每个 opencode 会话的
//! 索引、隔间、标签与记忆状态存进自己的 SQLite 库，`session_id` 就是
//! opencode 的会话 id（`ses_...`），所以能按 id 一一对应。
//!
//! 数据根默认是 `~/.local/share/cortexkit/magic-context`，可用
//! `ASCLEAN_MAGIC_DIR` 覆盖（多个用 `:` 分隔，指向数据根或 `context.db`）。
//!
//! 插件自己在 opencode 里收到 `session.deleted` 事件时会清理这些行；
//! asclean 直接删 opencode.db 的记录不产生事件，所以要在这里补一刀。

use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};

use crate::model::{Client, MagicMeta, Session};
use crate::util;

/// 覆盖 magic-context 数据根目录。
pub const ENV_DIR: &str = "ASCLEAN_MAGIC_DIR";

/// magic-context 存储的总体情况，供 doctor 与界面提示使用。
#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub databases: Vec<PathBuf>,
    /// 与 opencode 会话对上的记录数。
    pub linked: usize,
    /// 数据库里还在、但 opencode 侧已经没有同名会话的残留记录数。
    pub orphans: usize,
    /// 所有记录里的隔间总数。
    pub compartments: u32,
    pub errors: Vec<String>,
}

impl Summary {
    pub fn known(&self) -> bool {
        !self.databases.is_empty()
    }

    pub fn sessions(&self) -> usize {
        self.linked + self.orphans
    }
}

/// 本机所有可用的 magic-context 数据库文件（不存在就返回空）。
pub fn databases() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots() {
        let db = if root.extension().is_some_and(|ext| ext == "db") {
            root
        } else {
            root.join("context.db")
        };
        if db.is_file() && !found.contains(&db) {
            found.push(db);
        }
    }
    found
}

fn roots() -> Vec<PathBuf> {
    if let Some(raw) = env::var_os(ENV_DIR) {
        return env::split_paths(&raw)
            .filter(|path| !path.as_os_str().is_empty())
            .collect();
    }
    let base = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home().join(".local").join("share"));
    vec![base.join("cortexkit").join("magic-context")]
}

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 把 magic-context 记录贴到同名 opencode 会话上，并统计残留。
///
/// 找不到任何 magic-context 数据库时返回 `None`（表示本机没装这个插件）。
pub fn attach(sessions: &mut [Session]) -> Option<Summary> {
    let databases = databases();
    if databases.is_empty() {
        return None;
    }
    let mut summary = Summary {
        databases: databases.clone(),
        ..Summary::default()
    };
    let mut meta: Vec<(String, MagicMeta)> = Vec::new();
    for db in &databases {
        match load(db) {
            Ok(rows) => meta.extend(rows),
            Err(error) => summary.errors.push(format!("{}：{error:#}", db.display())),
        }
    }
    let opencode_ids: HashSet<String> = sessions
        .iter()
        .filter(|session| session.client == Client::Opencode)
        .map(|session| session.id.clone())
        .collect();
    let mut linked = HashSet::new();
    for (id, record) in &meta {
        summary.compartments += record.compartments;
        if !opencode_ids.contains(id) {
            summary.orphans += 1;
        }
    }
    for session in sessions.iter_mut() {
        if session.client != Client::Opencode {
            continue;
        }
        if let Some((_, record)) = meta.iter().find(|(id, _)| id == &session.id) {
            session.magic = Some(record.clone());
            linked.insert(session.id.clone());
        }
    }
    summary.linked = linked.len();
    Some(summary)
}

fn load(db: &Path) -> Result<Vec<(String, MagicMeta)>> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("打开 {}", db.display()))?;
    conn.busy_timeout(Duration::from_secs(1))?;
    const SQL: &str = "
        SELECT s.session_id,
               COALESCE(s.harness, ''),
               COALESCE(s.last_response_time, 0),
               (SELECT COUNT(*) FROM compartments c WHERE c.session_id = s.session_id)
        FROM session_meta s";
    let mut stmt = conn
        .prepare(SQL)
        .with_context(|| format!("读取 {}", db.display()))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(id, status, updated_at, compartments)| {
            (
                id,
                MagicMeta {
                    db: db.to_path_buf(),
                    compartments: compartments.max(0) as u32,
                    updated: util::system_time_from_millis(updated_at),
                    status,
                },
            )
        })
        .collect())
}

/// 在 magic-context 数据库里删掉一个会话的所有行。
///
/// 表结构随插件版本变化（当前 100 多张表），所以不写死表名：先扫
/// `sqlite_master`，凡是有 `session_id` 列的表都按 id 删一遍。整件事在
/// 一个事务里完成，数据库被 opencode 占着时最多等 3 秒。
pub fn delete_session(db: &Path, id: &str) -> Result<()> {
    if db.file_name().and_then(|name| name.to_str()) != Some("context.db") {
        bail!("不是 magic-context 数据库：{}", db.display());
    }
    if !db.is_file() {
        bail!("数据库不存在：{}", db.display());
    }
    let mut conn = Connection::open(db).with_context(|| format!("打开 {}", db.display()))?;
    conn.busy_timeout(Duration::from_secs(3))?;
    let tx = conn.transaction()?;
    let mut rows = 0usize;
    for table in session_tables(&tx)? {
        rows += tx
            .execute(
                &format!("DELETE FROM \"{table}\" WHERE session_id = ?1"),
                [id],
            )
            .with_context(|| format!("清理 {table}"))?;
    }
    if rows == 0 {
        bail!("数据库里没有这个会话，可能已被清理");
    }
    tx.commit()?;
    Ok(())
}

/// 当前库里所有带 `session_id` 列的用户表。
fn session_tables(tx: &rusqlite::Transaction<'_>) -> Result<Vec<String>> {
    let mut stmt = tx.prepare(
        "SELECT name FROM sqlite_master
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for name in names {
        let mut info = tx.prepare(&format!("PRAGMA table_info(\"{}\")", quote(&name)))?;
        let columns = info
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if columns.iter().any(|column| column == "session_id") {
            out.push(name);
        }
    }
    Ok(out)
}

fn quote(name: &str) -> String {
    name.replace('"', "\"\"")
}

/// 数据库旁边的 WAL 文件，供 doctor 提示并发风险。
pub fn side_files(db: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for suffix in ["-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", db.display()));
        if path.is_file() {
            out.push(path);
        }
    }
    out
}

/// 数据库最后被写入的时间。
pub fn modified(db: &Path) -> Option<SystemTime> {
    std::fs::metadata(db).and_then(|meta| meta.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default();
            let path = env::temp_dir().join(format!("asclean-{name}-{stamp}"));
            std::fs::create_dir_all(&path).expect("建目录");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (Scratch, PathBuf) {
        let dir = Scratch::new("magic");
        let db = dir.path().join("context.db");
        let conn = Connection::open(&db).expect("建库");
        conn.execute_batch(
            "CREATE TABLE session_meta (session_id TEXT PRIMARY KEY, harness TEXT,
                last_response_time INTEGER NOT NULL);
             CREATE TABLE compartments (session_id TEXT NOT NULL, seq INTEGER NOT NULL);
             CREATE TABLE tags (session_id TEXT NOT NULL, tag TEXT NOT NULL);
             CREATE TABLE schema_migrations_meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO session_meta VALUES ('ses_mine', 'opencode2', 1700000000000);
             INSERT INTO session_meta VALUES ('ses_keep', 'opencode2', 1700000000000);
             INSERT INTO compartments VALUES ('ses_mine', 0);
             INSERT INTO compartments VALUES ('ses_mine', 1);
             INSERT INTO compartments VALUES ('ses_keep', 0);
             INSERT INTO tags VALUES ('ses_mine', 'a');
             INSERT INTO schema_migrations_meta VALUES ('schema', 'v95');",
        )
        .expect("建表");
        (dir, db)
    }

    #[test]
    fn deletes_only_the_named_session() {
        let (_dir, db) = fixture();
        delete_session(&db, "ses_mine").expect("删除");
        let conn = Connection::open(&db).expect("打开");
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_meta", [], |row| row.get(0))
            .expect("计数");
        assert_eq!(sessions, 1);
        let compartments: i64 = conn
            .query_row("SELECT COUNT(*) FROM compartments", [], |row| row.get(0))
            .expect("计数");
        assert_eq!(compartments, 1);
        let tags: i64 = conn
            .query_row("SELECT COUNT(*) FROM tags", [], |row| row.get(0))
            .expect("计数");
        assert_eq!(tags, 0);
        let migrations: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations_meta", [], |row| {
                row.get(0)
            })
            .expect("计数");
        assert_eq!(migrations, 1, "不认识的表不该被碰");
    }

    #[test]
    fn unknown_session_is_an_error() {
        let (_dir, db) = fixture();
        let error = delete_session(&db, "ses_missing").unwrap_err();
        assert!(error.to_string().contains("没有这个会话"));
    }

    #[test]
    fn rejects_non_context_db_paths() {
        let error = delete_session(Path::new("/tmp/index.db"), "x").unwrap_err();
        assert!(error.to_string().contains("不是 magic-context 数据库"));
    }
}
