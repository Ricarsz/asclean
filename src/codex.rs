//! Codex 的会话存储。
//!
//! 老版本把每次会话写成 `sessions/YYYY/MM/DD/rollout-*.jsonl`；新版本
//! （`history_mode = paginated`）把正文搬进 SQLite：`state_<n>.sqlite` 的
//! `threads` 表登记会话，`thread_history_<n>.sqlite` 的 `thread_items` 表
//! 存消息。这里读这两份库，并在删除时清掉对应记录；只有 rollout 文件的
//! 老会话仍由扫描器按文件处理。
//!
//! 库文件名带版本号（`state_5`、`state_6`…），取编号最大的一份。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

/// `threads` 表里的一条会话，尚未映射成 [`Session`](crate::model::Session)。
#[derive(Debug, Clone)]
pub struct Thread {
    pub id: String,
    pub title: String,
    pub preview: String,
    pub cwd: String,
    /// 会话对应的 rollout 文件；新版本里文件可能已经不存在。
    pub rollout: PathBuf,
    pub created_ms: Option<i64>,
    pub updated_ms: Option<i64>,
    pub archived: bool,
    /// `thread_items` 里该会话的正文大小。
    pub bytes: u64,
    /// 用户消息与助手消息的条数。
    pub messages: u32,
}

/// 删除一条会话后清掉的行数与会话正文大小。
#[derive(Debug, Default, Clone, Copy)]
pub struct Removed {
    pub bytes: u64,
    pub rows: usize,
}

/// `state_<n>.sqlite`：登记会话的库。
pub fn find_state_db(root: &Path) -> Option<PathBuf> {
    newest(root, "state")
}

/// `thread_history_<n>.sqlite`：存消息正文的库。
pub fn find_history_db(root: &Path) -> Option<PathBuf> {
    newest(root, "thread_history")
}

/// 取编号最大的一份 `<stem>[_<n>].sqlite`。
fn newest(root: &Path, stem: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(version) = version_of(&name, stem) else {
            continue;
        };
        if best
            .as_ref()
            .map(|(seen, _)| version > *seen)
            .unwrap_or(true)
        {
            best = Some((version, path));
        }
    }
    best.map(|(_, path)| path)
}

/// `state_5.sqlite` → 5，`state.sqlite` → 0，其它返回 `None`。
fn version_of(name: &str, stem: &str) -> Option<u32> {
    let rest = name.strip_prefix(stem)?.strip_suffix(".sqlite")?;
    if rest.is_empty() {
        return Some(0);
    }
    rest.strip_prefix('_')?.parse().ok()
}

fn open_ro(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("打开 {}", path.display()))?;
    conn.busy_timeout(Duration::from_millis(800))?;
    Ok(conn)
}

fn open_rw(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("打开 {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(3))?;
    Ok(conn)
}

/// 读取一个 Codex 实例里的全部会话；没有库时返回空表。
pub fn load_threads(root: &Path) -> Result<Vec<Thread>> {
    let Some(db) = find_state_db(root) else {
        return Ok(Vec::new());
    };
    let conn = open_ro(&db)?;
    let stats = find_history_db(root)
        .and_then(|path| thread_stats(&path).ok())
        .unwrap_or_default();

    let mut stmt = conn
        .prepare("SELECT * FROM threads")
        .with_context(|| format!("读取 {}", db.display()))?;
    let columns: HashMap<String, usize> = stmt
        .column_names()
        .enumerate()
        .map(|(index, name)| (name.to_string(), index))
        .collect();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let Some(id) = text(row, &columns, "id").filter(|id| !id.is_empty()) else {
            continue;
        };
        let rollout = text(row, &columns, "rollout_path")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_default();
        let stat = stats.get(&id).copied().unwrap_or_default();
        out.push(Thread {
            title: text(row, &columns, "title").unwrap_or_default(),
            preview: text(row, &columns, "first_user_message")
                .filter(|text| !text.trim().is_empty())
                .or_else(|| text(row, &columns, "preview"))
                .unwrap_or_default(),
            cwd: text(row, &columns, "cwd").unwrap_or_default(),
            rollout,
            created_ms: int(row, &columns, "created_at_ms")
                .or_else(|| int(row, &columns, "created_at").map(|secs| secs * 1000)),
            updated_ms: int(row, &columns, "updated_at_ms")
                .or_else(|| int(row, &columns, "updated_at").map(|secs| secs * 1000)),
            archived: int(row, &columns, "archived").unwrap_or(0) != 0,
            bytes: stat.0,
            messages: stat.1,
            id,
        });
    }
    Ok(out)
}

fn text(row: &rusqlite::Row<'_>, columns: &HashMap<String, usize>, name: &str) -> Option<String> {
    let index = *columns.get(name)?;
    row.get::<_, Option<String>>(index).ok().flatten()
}

fn int(row: &rusqlite::Row<'_>, columns: &HashMap<String, usize>, name: &str) -> Option<i64> {
    let index = *columns.get(name)?;
    row.get::<_, Option<i64>>(index).ok().flatten()
}

/// 每条会话的（正文字节数，消息条数）；没有 `thread_items` 表时为空。
fn thread_stats(path: &Path) -> Result<HashMap<String, (u64, u32)>> {
    let conn = open_ro(path)?;
    if !has_table(&conn, "thread_items")? {
        return Ok(HashMap::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT thread_id, \
                    COALESCE(SUM(LENGTH(CAST(item_json AS BLOB))), 0), \
                    COALESCE(SUM(item_type IN ('userMessage', 'agentMessage')), 0) \
             FROM thread_items GROUP BY thread_id",
        )
        .with_context(|| format!("读取 {}", path.display()))?;
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let bytes: i64 = row.get(1)?;
        let messages: i64 = row.get(2)?;
        Ok((id, (bytes.max(0) as u64, messages.max(0) as u32)))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, stat) = row?;
        out.insert(id, stat);
    }
    Ok(out)
}

/// 删掉一条会话在 Codex 库里的全部记录。
///
/// 只按 `thread_id` 找表，不写死表名，Codex 增加附属表时也能清干净。
pub fn delete_thread(root: &Path, id: &str) -> Result<Removed> {
    let mut removed = Removed::default();
    if let Some(db) = find_state_db(root) {
        let conn = open_rw(&db)?;
        let tx = conn.unchecked_transaction()?;
        for table in thread_tables(&tx)? {
            tx.execute(
                &format!("DELETE FROM {} WHERE thread_id = ?1", quote(&table)),
                [id],
            )?;
        }
        removed.rows += tx.execute("DELETE FROM threads WHERE id = ?1", [id])?;
        tx.commit()
            .with_context(|| format!("写回 {}", db.display()))?;
    }
    if let Some(db) = find_history_db(root) {
        let conn = open_rw(&db)?;
        let tx = conn.unchecked_transaction()?;
        if has_table(&tx, "thread_items")? {
            removed.bytes += freed_bytes(&tx, id)?;
        }
        for table in thread_tables(&tx)? {
            tx.execute(
                &format!("DELETE FROM {} WHERE thread_id = ?1", quote(&table)),
                [id],
            )?;
        }
        tx.commit()
            .with_context(|| format!("写回 {}", db.display()))?;
    }
    Ok(removed)
}

fn freed_bytes(conn: &Connection, id: &str) -> Result<u64> {
    let bytes: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(LENGTH(CAST(item_json AS BLOB))), 0) \
             FROM thread_items WHERE thread_id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    Ok(bytes.max(0) as u64)
}

/// 库里所有带 `thread_id` 列的表。
fn thread_tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for name in names {
        let mut info = conn.prepare(&format!("PRAGMA table_info({})", quote(&name)))?;
        let mut has = false;
        for column in info.query_map([], |row| row.get::<_, String>(1))? {
            if column? == "thread_id" {
                has = true;
                break;
            }
        }
        if has {
            out.push(name);
        }
    }
    Ok(out)
}

fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default();
            let path = std::env::temp_dir().join(format!("asclean-codex-{name}-{stamp}"));
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

    fn state_db(root: &Path, name: &str) {
        let conn = Connection::open(root.join(name)).expect("建库");
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                cwd TEXT NOT NULL,
                title TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0,
                first_user_message TEXT NOT NULL DEFAULT '',
                preview TEXT NOT NULL DEFAULT ''
             );
             INSERT INTO threads VALUES
                ('thread-a', '/root/sessions/rollout-a.jsonl', '/work/a', '第一个会话',
                 1700000000, 1700000100, 0, '第一条用户消息', ''),
                ('thread-b', '', '/work/b', '第二个会话',
                 1700000200, 1700000300, 1, '', '备选预览');",
        )
        .expect("建表");
    }

    fn history_db(root: &Path) {
        let conn = Connection::open(root.join("thread_history_1.sqlite")).expect("建库");
        conn.execute_batch(
            "CREATE TABLE thread_items (
                thread_id TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                item_id TEXT NOT NULL,
                item_json TEXT NOT NULL,
                item_type TEXT NOT NULL,
                PRIMARY KEY (thread_id, turn_id, item_id)
             );
             CREATE TABLE thread_turns (thread_id TEXT NOT NULL, turn_id TEXT NOT NULL);
             INSERT INTO thread_items VALUES
                ('thread-a', 't1', 'i1', '{\"x\":1}', 'userMessage'),
                ('thread-a', 't1', 'i2', '{\"x\":2}', 'agentMessage'),
                ('thread-a', 't1', 'i3', '{\"x\":3}', 'reasoning'),
                ('thread-b', 't1', 'i1', '{\"x\":4}', 'userMessage');
             INSERT INTO thread_turns VALUES ('thread-a', 't1'), ('thread-b', 't1');",
        )
        .expect("建表");
    }

    #[test]
    fn picks_the_newest_database() {
        let scratch = Scratch::new("newest");
        fs::write(scratch.path().join("state_4.sqlite"), "").unwrap();
        fs::write(scratch.path().join("state_12.sqlite"), "").unwrap();
        fs::write(scratch.path().join("state.sqlite"), "").unwrap();
        fs::write(scratch.path().join("state_12.sqlite-wal"), "").unwrap();
        assert_eq!(
            find_state_db(scratch.path()).and_then(|path| path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())),
            Some("state_12.sqlite".to_string())
        );
        assert_eq!(version_of("state_5.sqlite", "state"), Some(5));
        assert_eq!(version_of("state.sqlite", "state"), Some(0));
        assert_eq!(version_of("other.sqlite", "state"), None);
    }

    #[test]
    fn loads_threads_with_history_stats() {
        let scratch = Scratch::new("load");
        state_db(scratch.path(), "state_5.sqlite");
        history_db(scratch.path());

        let threads = load_threads(scratch.path()).expect("读库");
        assert_eq!(threads.len(), 2);
        let first = &threads[0];
        assert_eq!(first.id, "thread-a");
        assert_eq!(first.title, "第一个会话");
        assert_eq!(first.preview, "第一条用户消息");
        assert_eq!(first.cwd, "/work/a");
        assert_eq!(first.created_ms, Some(1_700_000_000_000));
        assert!(!first.archived);
        assert_eq!(first.messages, 2, "只算用户与助手消息");
        assert_eq!(first.bytes, 21, "三条 item_json 的字节数");
        assert_eq!(
            first.rollout,
            PathBuf::from("/root/sessions/rollout-a.jsonl")
        );

        let second = &threads[1];
        assert_eq!(second.preview, "备选预览", "没有首条用户消息时用 preview");
        assert_eq!(second.created_ms, Some(1_700_000_200_000));
        assert!(second.archived);
        assert_eq!(second.messages, 1);
    }

    #[test]
    fn deletes_thread_rows_from_both_databases() {
        let scratch = Scratch::new("delete");
        state_db(scratch.path(), "state_5.sqlite");
        history_db(scratch.path());

        let removed = delete_thread(scratch.path(), "thread-a").expect("删除");
        assert_eq!(removed.rows, 1);
        assert_eq!(removed.bytes, 21);

        let state = Connection::open(scratch.path().join("state_5.sqlite")).unwrap();
        let left: i64 = state
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = 'thread-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
        let history = Connection::open(scratch.path().join("thread_history_1.sqlite")).unwrap();
        let items: i64 = history
            .query_row(
                "SELECT COUNT(*) FROM thread_items WHERE thread_id = 'thread-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(items, 0);
        let turns: i64 = history
            .query_row(
                "SELECT COUNT(*) FROM thread_turns WHERE thread_id = 'thread-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(turns, 0, "附属表按 thread_id 一并清理");
        let kept: i64 = history
            .query_row(
                "SELECT COUNT(*) FROM thread_items WHERE thread_id = 'thread-b'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1, "别的会话不受影响");
    }

    #[test]
    fn missing_database_is_not_an_error() {
        let scratch = Scratch::new("empty");
        assert!(load_threads(scratch.path()).expect("没有库").is_empty());
        let removed = delete_thread(scratch.path(), "thread-a").expect("删除");
        assert_eq!(removed.rows, 0);
        assert_eq!(removed.bytes, 0);
    }
}
