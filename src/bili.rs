//! billion-context 的会话存储（`sessions/index.db`）：探测、统计与连带删除。
//!
//! bili 用 `<数据根>/sessions/index.db` 记录它代理过的会话，`session_id`
//! 与 DSH 的会话目录同名，所以能按 id 一一对应。这里只读写这个数据库，
//! 不碰日志、证书与代理进程。
//!
//! 数据根默认是 `~/.local/share/billion-context`，可用 `ASCLEAN_BILI_DIR`
//! 覆盖（多个用 `:` 分隔，指向数据根或 `index.db` 都行）。

use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};

use crate::model::{BiliMeta, Client, Session};
use crate::util;

/// 覆盖 bili 数据根目录。
pub const ENV_DIR: &str = "ASCLEAN_BILI_DIR";

/// 会随会话一起删除的表；bili 没给这些表加外键，只能手工清理。
const OWNED_TABLES: [&str; 10] = [
    "messages",
    "message_media",
    "blocks",
    "refs",
    "dead_refs",
    "ccr_entries",
    "checkpoints",
    "session_ext",
    "conversations",
    "sessions",
];

/// bili 存储的总体情况，供 doctor 与界面提示使用。
#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub databases: Vec<PathBuf>,
    /// 与 DSH 会话对上的记录数。
    pub linked: usize,
    /// 数据库里还在、但 DSH 侧已经没有同名会话的残留记录数。
    pub orphans: usize,
    pub bytes: u64,
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

/// 本机所有可用的 bili 数据库文件（不存在就返回空）。
pub fn databases() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots() {
        let db = if root.extension().is_some_and(|ext| ext == "db") {
            root
        } else {
            root.join("sessions").join("index.db")
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
    vec![base.join("billion-context")]
}

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 把 bili 记录贴到同名 DSH 会话上，并统计残留。
///
/// 找不到任何 bili 数据库时返回 `None`（表示本机没装 bili）。
pub fn attach(sessions: &mut [Session]) -> Option<Summary> {
    let databases = databases();
    if databases.is_empty() {
        return None;
    }
    let mut summary = Summary {
        databases: databases.clone(),
        ..Summary::default()
    };
    let mut meta: Vec<(String, BiliMeta)> = Vec::new();
    for db in &databases {
        match load(db) {
            Ok(rows) => meta.extend(rows),
            Err(error) => summary.errors.push(format!("{}：{error:#}", db.display())),
        }
    }
    let dsh_ids: HashSet<String> = sessions
        .iter()
        .filter(|session| session.client == Client::Dsh)
        .map(|session| session.id.clone())
        .collect();
    let mut linked = HashSet::new();
    for (id, record) in &meta {
        summary.bytes += record.bytes;
        if !dsh_ids.contains(id) {
            summary.orphans += 1;
        }
    }
    for session in sessions.iter_mut() {
        if session.client != Client::Dsh {
            continue;
        }
        if let Some((_, record)) = meta.iter().find(|(id, _)| id == &session.id) {
            session.bili = Some(record.clone());
            linked.insert(session.id.clone());
        }
    }
    summary.linked = linked.len();
    Some(summary)
}

/// bili 数据库里的一条会话记录（轻量视图，用于找残留）。
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    /// 代理实例命名空间，DSH 写的是 `dsh`。
    pub namespace: String,
    pub title: String,
}

/// 读取 bili 数据库里的会话清单。
pub fn entries(db: &Path) -> Result<Vec<Entry>> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("打开 {}", db.display()))?;
    conn.busy_timeout(Duration::from_secs(1))?;
    let mut stmt = conn
        .prepare("SELECT session_id, namespace, COALESCE(title, '') FROM sessions")
        .with_context(|| format!("读取 {}", db.display()))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    Ok(rows
        .into_iter()
        .map(|(id, namespace, title)| Entry {
            id,
            namespace,
            title,
        })
        .collect())
}

/// 某个会话在 bili 里占的字节数（只读，不删）。
pub fn bytes_of(db: &Path, id: &str) -> Result<u64> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("打开 {}", db.display()))?;
    conn.busy_timeout(Duration::from_secs(1))?;
    session_bytes(&conn, id)
}

fn load(db: &Path) -> Result<Vec<(String, BiliMeta)>> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("打开 {}", db.display()))?;
    conn.busy_timeout(Duration::from_secs(1))?;
    let mut stmt = conn
        .prepare("SELECT session_id, status, updated_at FROM sessions")
        .with_context(|| format!("读取 {}", db.display()))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let mut out = Vec::with_capacity(rows.len());
    for (id, status, updated_at) in rows {
        out.push((
            id.clone(),
            BiliMeta {
                db: db.to_path_buf(),
                bytes: session_bytes(&conn, &id).unwrap_or(0),
                messages: message_count(&conn, &id).unwrap_or(0),
                updated: util::system_time_from_millis(updated_at),
                status,
            },
        ));
    }
    Ok(out)
}

fn message_count(conn: &Connection, id: &str) -> Result<u32> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
        [id],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as u32)
}

/// 会话在 bili 存储里占用的去重字节数（内容按 hash 共享，可能重复计入共享块）。
fn session_bytes(conn: &Connection, id: &str) -> Result<u64> {
    const SQL: &str = "
        SELECT COALESCE(SUM(sz), 0) FROM (
            SELECT c.byte_len AS sz FROM messages m
                JOIN content c ON c.hash = m.content_hash
                WHERE m.session_id = ?1
            UNION ALL
            SELECT c.byte_len FROM blocks b
                JOIN content c ON c.hash = b.summary_hash
                WHERE b.session_id = ?1
            UNION ALL
            SELECT c.byte_len FROM blocks b
                JOIN content c ON c.hash = b.one_hash
                WHERE b.session_id = ?1 AND b.one_hash IS NOT NULL
            UNION ALL
            SELECT c.byte_len FROM message_media mm
                JOIN content c ON c.hash = mm.media_hash
                WHERE mm.session_id = ?1
            UNION ALL
            SELECT c.byte_len FROM ccr_entries e
                JOIN content c ON c.hash = e.content_hash
                WHERE e.session_id = ?1
            UNION ALL
            SELECT c.byte_len FROM checkpoints p
                JOIN content c ON c.hash = p.state_hash
                WHERE p.session_id = ?1
        )";
    let bytes: i64 = conn.query_row(SQL, [id], |row| row.get(0))?;
    Ok(bytes.max(0) as u64)
}

/// 这个会话引用到的所有内容 hash（用于删除后回收孤儿内容）。
fn session_hashes(conn: &Connection, id: &str) -> Result<HashSet<String>> {
    const SQL: &str = "
        SELECT content_hash AS h FROM messages WHERE session_id = ?1
        UNION SELECT summary_hash FROM blocks WHERE session_id = ?1
        UNION SELECT one_hash FROM blocks WHERE session_id = ?1 AND one_hash IS NOT NULL
        UNION SELECT media_hash FROM message_media WHERE session_id = ?1
        UNION SELECT content_hash FROM ccr_entries WHERE session_id = ?1
        UNION SELECT state_hash FROM checkpoints WHERE session_id = ?1";
    let mut stmt = conn.prepare(SQL)?;
    let rows = stmt.query_map([id], |row| row.get::<_, String>(0))?;
    let mut out = HashSet::new();
    for row in rows {
        out.insert(row?);
    }
    Ok(out)
}

fn hash_referenced(conn: &Connection, hash: &str) -> bool {
    const SQL: &str = "
        SELECT 1 FROM messages WHERE content_hash = ?1
        UNION ALL SELECT 1 FROM blocks WHERE summary_hash = ?1
        UNION ALL SELECT 1 FROM blocks WHERE one_hash = ?1
        UNION ALL SELECT 1 FROM message_media WHERE media_hash = ?1
        UNION ALL SELECT 1 FROM ccr_entries WHERE content_hash = ?1
        UNION ALL SELECT 1 FROM checkpoints WHERE state_hash = ?1
        LIMIT 1";
    conn.query_row(SQL, [hash], |_| Ok(())).is_ok()
}

/// 在 bili 数据库里删掉一个会话及其独占内容，返回释放的字节数。
///
/// 删除在一个事务里完成；数据库被代理进程占着时会在 3 秒后报 `busy`。
pub fn delete_session(db: &Path, id: &str) -> Result<u64> {
    if db.file_name().and_then(|name| name.to_str()) != Some("index.db") {
        bail!("不是 billion-context 数据库：{}", db.display());
    }
    if !db.is_file() {
        bail!("数据库不存在：{}", db.display());
    }
    let mut conn = Connection::open(db).with_context(|| format!("打开 {}", db.display()))?;
    // 代理进程一直在写这份库，给它一点让路的时间再判失败。
    conn.busy_timeout(Duration::from_secs(5))?;
    let tx = conn.transaction()?;
    let bytes = session_bytes(&tx, id)?;
    let hashes = session_hashes(&tx, id)?;
    let mut removed = 0usize;
    for table in OWNED_TABLES {
        removed += tx
            .execute(&format!("DELETE FROM {table} WHERE session_id = ?1"), [id])
            .with_context(|| format!("清理 {table}"))?;
    }
    if removed == 0 {
        bail!("数据库里没有这个会话，可能已被清理");
    }
    // 内容按 hash 共享：只回收已经没人引用的那部分。
    for hash in hashes {
        if hash_referenced(&tx, &hash) {
            continue;
        }
        tx.execute("DELETE FROM content WHERE hash = ?1", [&hash])?;
    }
    tx.commit()?;
    Ok(bytes)
}

/// 数据库旁边的锁文件与 WAL 文件，供 doctor 提示并发风险。
pub fn side_files(db: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for suffix in ["-wal", "-shm", ".lock"] {
        let path = PathBuf::from(format!("{}{suffix}", db.display()));
        if path.is_file() {
            out.push(path);
        }
    }
    out
}

/// 数据库最后被写入的时间，用来判断 bili 是否还在跑。
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
        let dir = Scratch::new("bili");
        let db = dir.path().join("sessions").join("index.db");
        std::fs::create_dir_all(db.parent().expect("父目录")).expect("建目录");
        let conn = Connection::open(&db).expect("建库");
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
             INSERT INTO sessions VALUES ('shared', 'active', 1700000000000);
             INSERT INTO sessions VALUES ('only-mine', 'active', 1700000000000);
             INSERT INTO messages VALUES ('shared', 'h-shared');
             INSERT INTO messages VALUES ('only-mine', 'h-shared');
             INSERT INTO messages VALUES ('only-mine', 'h-mine');
             INSERT INTO content VALUES ('h-shared', 100);
             INSERT INTO content VALUES ('h-mine', 50);",
        )
        .expect("建表");
        (dir, db)
    }

    #[test]
    fn deletes_session_rows_and_shared_content_survives() {
        let (_dir, db) = fixture();
        let freed = delete_session(&db, "only-mine").expect("删除");
        assert_eq!(freed, 150);
        let conn = Connection::open(&db).expect("打开");
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .expect("计数");
        assert_eq!(sessions, 1);
        let content: i64 = conn
            .query_row("SELECT COUNT(*) FROM content", [], |row| row.get(0))
            .expect("计数");
        // h-mine 没人引用被回收，h-shared 还被 shared 会话引用。
        assert_eq!(content, 1);
    }

    #[test]
    fn rejects_non_index_db_paths() {
        let error = delete_session(Path::new("/tmp/opencode.db"), "x").unwrap_err();
        assert!(error.to_string().contains("不是 billion-context 数据库"));
    }
}
