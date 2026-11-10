//! 扫描本机的会话存储：
//!
//! - DeepSeek Harness：`~/.dsh*/sessions/<slug>/session-<uuid>/session*.jsonl.zstd`
//! - Codex：`~/.codex*/sessions/YYYY/MM/DD/rollout-*.jsonl`，以及新版本
//!   `~/.codex*/state_<n>.sqlite` 里的会话记录（见 [`crate::codex`]）
//! - Opencode：`~/.local/share/opencode/opencode.db`（SQLite）
//!
//! 目录位置可由 `ASCLEAN_DSH_ROOT` / `ASCLEAN_CODEX_HOME` / `ASCLEAN_OPENCODE_DIR`
//! 覆盖，多个路径用 `:` 分隔，便于扫描代理版等独立实例。

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::model::{Client, Session, Workspace};
use crate::util;

#[derive(Debug, Clone)]
pub struct Root {
    pub client: Client,
    pub path: PathBuf,
    pub sessions: usize,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    /// 扫描到的工作区，包含还没有任何会话的登记项。
    pub workspaces: Vec<Workspace>,
    pub roots: Vec<Root>,
    pub warnings: Vec<String>,
    /// billion-context 的存储概况；本机没装 bili 时为空。
    pub bili: Option<crate::bili::Summary>,
    /// magic-context 的存储概况；本机没装 magic-context 时为空。
    pub magic: Option<crate::magic::Summary>,
}

impl Snapshot {}

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn env_roots(name: &str) -> Vec<PathBuf> {
    env::var_os(name)
        .map(|raw| {
            env::split_paths(&raw)
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// `$HOME` 下所有名字形如 `<prefix>*` 且包含 `sessions/` 的目录。
fn sibling_roots(prefix: &str) -> Vec<PathBuf> {
    sibling_roots_where(prefix, |path| path.join("sessions").is_dir())
}

/// 同上，目录是否算一个实例由 `keep` 决定（Codex 新版本可能没有 `sessions/`）。
fn sibling_roots_where(prefix: &str, keep: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(home()) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && keep(path))
        .collect();
    found.sort();
    found
}

/// Codex 实例：有 rollout 目录，或者新版本的会话库。
fn codex_root(path: &Path) -> bool {
    path.join("sessions").is_dir() || crate::codex::find_state_db(path).is_some()
}

pub fn scan() -> Snapshot {
    let mut snapshot = Snapshot::default();
    scan_dsh(&mut snapshot);
    scan_codex(&mut snapshot);
    scan_opencode(&mut snapshot);
    inherit_subagent_workspaces(&mut snapshot.sessions);
    snapshot.sessions.sort_by(|a, b| {
        b.updated_or_epoch()
            .cmp(&a.updated_or_epoch())
            .then_with(|| a.client.cmp(&b.client))
    });
    dedupe_storage(&mut snapshot);
    dedupe_workspaces(&mut snapshot);
    snapshot.bili = crate::bili::attach(&mut snapshot.sessions);
    if let Some(summary) = &snapshot.bili {
        for error in &summary.errors {
            snapshot.warnings.push(format!("billion-context：{error}"));
        }
    }
    snapshot.magic = crate::magic::attach(&mut snapshot.sessions);
    if let Some(summary) = &snapshot.magic {
        for error in &summary.errors {
            snapshot.warnings.push(format!("magic-context：{error}"));
        }
    }
    snapshot
}

/// 同一份存储在 `$HOME` 下可能有多个入口（例如代理实例把 `sessions`
/// 符号链接到主目录）。这里按规范化路径合并，避免同一个会话重复列出。
fn dedupe_storage(snapshot: &mut Snapshot) {
    let mut root_map: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut kept_roots = Vec::with_capacity(snapshot.roots.len());
    let mut aliases = Vec::new();
    for root in std::mem::take(&mut snapshot.roots) {
        let key = storage_key(root.client, &root.path);
        match root_map.get(&key) {
            Some(first) => aliases.push(format!(
                "{} 与 {} 指向同一存储，已合并",
                root.path.display(),
                first.display()
            )),
            None => {
                root_map.insert(key, root.path.clone());
                kept_roots.push(root);
            }
        }
    }
    snapshot.roots = kept_roots;
    snapshot.warnings.extend(aliases);

    // 会话重复：codex 的别名实例会扫到同一批复刻文件；opencode 的 db 是同一个文件，按 ID 区分。
    let mut seen = HashSet::new();
    snapshot.sessions.retain(|session| match session.client {
        Client::Opencode => true,
        _ => seen.insert(canonical(&session.path)),
    });
    for session in &mut snapshot.sessions {
        if let Some(root) = root_map.get(&storage_key(session.client, &session.root)) {
            session.root = root.clone();
            if let Some(workspace) = &mut session.workspace {
                workspace.root = root.clone();
            }
        }
    }
}

/// 别名根会让同一份工作区登记出现两次，按客户端 + 标识 + 规范化根去重。
fn dedupe_workspaces(snapshot: &mut Snapshot) {
    let mut seen = HashSet::new();
    snapshot.workspaces.retain(|workspace| {
        seen.insert((
            workspace.client,
            workspace.id.clone(),
            canonical(&workspace.root),
        ))
    });
}

/// 判断两个根目录是否指向同一份存储：DSH/Codex 看 `sessions/`，Opencode 看数据库文件。
fn storage_key(client: Client, root: &Path) -> PathBuf {
    match client {
        Client::Opencode => canonical(root),
        Client::Dsh | Client::Codex => canonical(&root.join("sessions")),
    }
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

// ---------------------------------------------------------------- DSH

/// DSH 的工作区登记：`<root>/storages/workspace.json` 里的 `tables.workspaces`。
#[derive(Debug, Default)]
struct DshWorkspaces {
    all: Vec<Workspace>,
    /// 会话 id → 工作区 id。
    session_owner: HashMap<String, String>,
}

impl DshWorkspaces {
    fn by_id(&self, id: &str) -> Option<&Workspace> {
        self.all.iter().find(|workspace| workspace.id == id)
    }

    /// 会话归属的工作区；`id` 带不带 `session-` 前缀都能查。
    fn owner_of(&self, id: &str) -> Option<&Workspace> {
        self.session_owner
            .get(util::dsh_core(id))
            .and_then(|owner| self.by_id(owner))
    }

    fn by_path(&self, path: &str) -> Option<&Workspace> {
        self.all
            .iter()
            .find(|workspace| same_path(&workspace.path, path))
    }
}

fn load_dsh_workspaces(root: &Path) -> Result<DshWorkspaces> {
    let path = root.join("storages").join("workspace.json");
    let text = fs::read_to_string(&path).with_context(|| format!("读取 {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("解析 {}", path.display()))?;
    Ok(parse_dsh_workspaces(&value, root))
}

fn parse_dsh_workspaces(value: &Value, root: &Path) -> DshWorkspaces {
    let mut out = DshWorkspaces::default();
    let Some(table) = value
        .pointer("/tables/workspaces")
        .and_then(Value::as_object)
    else {
        return out;
    };
    for (id, record) in table {
        let Some(path) = record
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
        else {
            continue;
        };
        let title = record
            .get("title")
            .and_then(Value::as_str)
            .map(util::one_line)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| util::path_title(path));
        for session_id in record
            .get("sessionIds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            // 老版本登记成裸 UUID，新版本带 `session-` 前缀，统一按裸 id 存。
            out.session_owner
                .insert(util::dsh_core(session_id).to_string(), id.clone());
        }
        out.all.push(Workspace {
            client: Client::Dsh,
            id: id.clone(),
            path: path.to_string(),
            title,
            root: root.to_path_buf(),
        });
    }
    out
}

/// 比较两个目录路径，容忍结尾斜杠与 `//`。
fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// 本机所有 DSH 存储根：默认位置 + 环境变量覆盖的位置。
///
/// 清理残留时必须用它，而不是当前扫描范围 —— 否则 `ASCLEAN_DSH_ROOT`
/// 指到别处时，真实会话会被当成“目录不存在”而误删 bili 记录。
pub fn all_dsh_roots() -> Vec<PathBuf> {
    let mut roots = sibling_roots(".dsh");
    for root in env_roots("ASCLEAN_DSH_ROOT") {
        if !roots.iter().any(|known| known == &root) {
            roots.push(root);
        }
    }
    roots
}

/// 所有 DSH 会话目录的裸 id（只读目录名，不解析内容）。
pub fn dsh_session_dirs() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for root in all_dsh_roots() {
        let Ok(entries) = fs::read_dir(root.join("sessions")) else {
            continue;
        };
        for slug in entries.flatten() {
            if !slug.path().is_dir() {
                continue;
            }
            let Ok(sessions) = fs::read_dir(slug.path()) else {
                continue;
            };
            for session in sessions.flatten() {
                let path = session.path();
                if !path.is_dir() {
                    continue;
                }
                let name = session.file_name().to_string_lossy().to_string();
                if util::looks_like_dsh_id(&name) {
                    out.push((util::dsh_core(&name).to_string(), path));
                }
            }
        }
    }
    out
}

fn scan_dsh(snapshot: &mut Snapshot) {
    let mut roots = env_roots("ASCLEAN_DSH_ROOT");
    if roots.is_empty() {
        roots = sibling_roots(".dsh");
    }
    for root in roots {
        let base = root.join("sessions");
        let mut root_info = Root {
            client: Client::Dsh,
            path: root.clone(),
            sessions: 0,
            error: None,
        };
        let workspaces = match load_dsh_workspaces(&root) {
            Ok(workspaces) => {
                snapshot.workspaces.extend(workspaces.all.iter().cloned());
                workspaces
            }
            Err(error) => {
                // 老版本 DSH 可能还没有工作区登记，这只是少了一列信息。
                if root.join("storages").join("workspace.json").exists() {
                    snapshot
                        .warnings
                        .push(format!("{}：{error:#}", root.display()));
                }
                DshWorkspaces::default()
            }
        };
        let mut entries: Vec<PathBuf> = fs::read_dir(&base)
            .map(|reads| {
                reads
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        for bucket in entries {
            let Ok(sessions) = fs::read_dir(&bucket) else {
                continue;
            };
            for session_dir in sessions.flatten() {
                let dir = session_dir.path();
                if !dir.is_dir() {
                    continue;
                }
                match load_dsh_session(&dir, &root, &workspaces) {
                    Ok(Some(session)) => {
                        root_info.sessions += 1;
                        snapshot.sessions.push(session);
                    }
                    Ok(None) => {}
                    Err(error) => snapshot
                        .warnings
                        .push(format!("{}：{error}", dir.display())),
                }
            }
        }
        snapshot.roots.push(root_info);
    }
}

fn load_dsh_session(
    dir: &Path,
    root: &Path,
    workspaces: &DshWorkspaces,
) -> Result<Option<Session>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("读取 {}", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    let payload = files
        .iter()
        .find(|path| path.to_string_lossy().ends_with(".jsonl.zstd"))
        .or_else(|| {
            files
                .iter()
                .find(|path| path.to_string_lossy().ends_with(".jsonl"))
        });

    let mut id = String::new();
    let mut cwd = None;
    let mut created = None;
    let mut title = String::new();
    let mut preview = String::new();
    let mut messages = 0u32;
    let mut subagent = false;
    let mut parent: Option<String> = None;

    if let Some(payload) = payload {
        let mut reader = open_maybe_zstd(payload)?;
        let mut line = String::new();
        while reader.read_line(&mut line)? > 0 {
            let trimmed = line.trim_end();
            if !trimmed.is_empty()
                && let Ok(value) = serde_json::from_str::<Value>(trimmed)
            {
                match value.get("type").and_then(Value::as_str).unwrap_or("") {
                    "session" => {
                        if id.is_empty() {
                            id = value
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                        }
                        if created.is_none() {
                            created = value
                                .get("createdAt")
                                .and_then(Value::as_i64)
                                .and_then(util::system_time_from_millis);
                        }
                        if cwd.is_none() {
                            cwd = value.get("cwd").and_then(Value::as_str).map(str::to_string);
                        }
                        if value.get("origin").and_then(Value::as_str) == Some("subagent") {
                            subagent = true;
                        }
                        if parent.is_none() {
                            parent = value
                                .get("parentSession")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                        }
                    }
                    "session/title" => {
                        if let Some(text) = value
                            .pointer("/data/title")
                            .and_then(Value::as_str)
                            .filter(|text| !text.trim().is_empty())
                        {
                            title = util::one_line(text);
                        }
                    }
                    "user/message" => {
                        messages += 1;
                        if preview.is_empty()
                            && let Some(text) = dsh_user_text(&value)
                        {
                            preview = text;
                        }
                    }
                    "assistant/message" => messages += 1,
                    _ => {}
                }
            }
            line.clear();
        }
    }

    if id.is_empty() {
        // 目录名就是会话 id：老版本是裸 UUID，新版本带 `session-` 前缀。
        id = dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
    }

    let mut updated = None;
    let mut bytes = 0;
    for file in &files {
        if let Ok(meta) = file.metadata() {
            bytes += meta.len();
            if let Ok(modified) = meta.modified() {
                updated = Some(updated.map_or(modified, |prev: SystemTime| prev.max(modified)));
            }
        }
    }

    let locked = util::lock_held(&dir.join("session.lock"));
    let workspace = workspaces
        .owner_of(&id)
        .or_else(|| {
            // 登记表里没有这个会话时，按 cwd 找同名工作区，最后退回按目录合成。
            cwd.as_deref().and_then(|cwd| workspaces.by_path(cwd))
        })
        .cloned()
        .or_else(|| {
            cwd.as_ref()
                .map(|cwd| Workspace::from_path(Client::Dsh, cwd.clone(), root.to_path_buf()))
        });
    let title = if title.is_empty() {
        if preview.is_empty() {
            "(无标题)".to_string()
        } else {
            util::truncate(&preview, 60)
        }
    } else {
        title
    };

    Ok(Some(Session {
        client: Client::Dsh,
        id,
        title,
        preview,
        path: dir.to_path_buf(),
        root: root.to_path_buf(),
        cwd,
        workspace,
        created,
        updated,
        bytes,
        messages,
        tokens: None,
        cost: None,
        locked,
        subagent,
        parent,
        bili: None,
        magic: None,
    }))
}

/// 子会话跟随父会话的工作区：父会话登记过工作区、自己只能按 cwd 猜时，
/// 用父会话的归属，避免同一个工作区在树里裂成两行。
fn inherit_subagent_workspaces(sessions: &mut [Session]) {
    let mut owners: HashMap<String, Workspace> = HashMap::new();
    for session in sessions.iter() {
        if session.client != Client::Dsh {
            continue;
        }
        if let Some(workspace) = &session.workspace {
            owners
                .entry(util::dsh_core(&session.id).to_string())
                .or_insert_with(|| workspace.clone());
        }
    }
    for session in sessions.iter_mut() {
        if session.client != Client::Dsh || !session.subagent {
            continue;
        }
        if session
            .workspace
            .as_ref()
            .is_some_and(|workspace| !workspace.is_synthetic())
        {
            continue;
        }
        let Some(parent) = session.parent.as_deref() else {
            continue;
        };
        if let Some(workspace) = owners.get(util::dsh_core(parent)) {
            session.workspace = Some(workspace.clone());
        }
    }
}

/// 取一条真实用户消息：跳过 runtime context、system-reminder 之类的系统注入。
fn dsh_user_text(record: &Value) -> Option<String> {
    let source = record
        .pointer("/data/source/kind")
        .and_then(Value::as_str)
        .unwrap_or("");
    if source != "user" {
        return None;
    }
    let parts = record.pointer("/data/content").and_then(Value::as_array)?;
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        let text = util::one_line(text);
        if text.is_empty() || looks_like_injection(&text) {
            continue;
        }
        return Some(util::truncate(&text, 160));
    }
    None
}

fn looks_like_injection(text: &str) -> bool {
    const MARKERS: [&str; 8] = [
        "Current runtime context",
        "<system-reminder>",
        "<environment_context>",
        "<user_instructions>",
        "Time sampled while preparing",
        "Approval prompts are disabled",
        "Current DSH file policy",
        "Time sampled",
    ];
    let head = text.chars().take(80).collect::<String>();
    MARKERS.iter().any(|marker| head.starts_with(marker))
}

fn open_maybe_zstd(path: &Path) -> Result<Box<dyn BufRead>> {
    let file = fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    if path.to_string_lossy().ends_with(".zstd") {
        return Ok(Box::new(BufReader::new(ZstdReader::new(
            path.to_path_buf(),
            BufReader::new(file),
        ))));
    }
    Ok(Box::new(BufReader::new(file)))
}

/// DSH 把 JSONL 分块追加成 zstd 流（一个文件里几十上百帧），
/// 而 `StreamingDecoder` 只解一帧；这里在一帧结束后重建解码器接着解下一帧。
/// 首帧就失败时退回系统 `zstd -dc`。
struct ZstdReader {
    path: PathBuf,
    decoder: Option<
        ruzstd::decoding::StreamingDecoder<BufReader<fs::File>, ruzstd::decoding::FrameDecoder>,
    >,
    child: Option<BufReader<std::process::ChildStdout>>,
    started: bool,
}

impl ZstdReader {
    fn new(path: PathBuf, file: BufReader<fs::File>) -> Self {
        let decoder = ruzstd::decoding::StreamingDecoder::new(file).ok();
        Self {
            path,
            decoder,
            child: None,
            started: false,
        }
    }

    fn fallback(&mut self, buf: &mut [u8], error: std::io::Error) -> std::io::Result<usize> {
        let Ok(mut child) = Command::new("zstd")
            .args(["-dc"])
            .arg(&self.path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return Err(error);
        };
        let Some(stdout) = child.stdout.take() else {
            return Err(error);
        };
        let mut reader = BufReader::new(stdout);
        let read = reader.read(buf)?;
        self.child = Some(reader);
        self.decoder = None;
        self.started = true;
        Ok(read)
    }

    /// 当前帧已解完，换到下一帧；返回 false 表示流结束。
    fn next_frame(&mut self) -> bool {
        let Some(decoder) = self.decoder.take() else {
            return false;
        };
        let (reader, _) = decoder.into_parts();
        match ruzstd::decoding::StreamingDecoder::new(reader) {
            Ok(decoder) => {
                self.decoder = Some(decoder);
                true
            }
            Err(_) => false,
        }
    }
}

impl Read for ZstdReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(child) = self.child.as_mut() {
            return child.read(buf);
        }
        loop {
            let Some(decoder) = self.decoder.as_mut() else {
                return Ok(0);
            };
            match decoder.read(buf) {
                Ok(0) => {
                    if !self.next_frame() {
                        return Ok(0);
                    }
                }
                Ok(read) => {
                    self.started = true;
                    return Ok(read);
                }
                Err(error) if self.started => return Err(error),
                Err(error) => return self.fallback(buf, error),
            }
        }
    }
}

// ---------------------------------------------------------------- Codex

fn scan_codex(snapshot: &mut Snapshot) {
    let mut roots = env_roots("ASCLEAN_CODEX_HOME");
    if roots.is_empty() {
        roots = sibling_roots_where(".codex", codex_root);
    }
    for root in roots {
        let mut root_info = Root {
            client: Client::Codex,
            path: root.clone(),
            sessions: 0,
            error: None,
        };
        let titles = codex_titles(&root.join("session_index.jsonl"));
        let mut files = Vec::new();
        collect_rollouts(&root.join("sessions"), 0, &mut files);
        files.sort();
        let mut seen = HashSet::new();
        for file in files {
            match load_codex_session(&file, &root, &titles) {
                Ok(session) => {
                    seen.insert(session.id.clone());
                    root_info.sessions += 1;
                    snapshot.sessions.push(session);
                }
                Err(error) => snapshot
                    .warnings
                    .push(format!("{}：{error}", file.display())),
            }
        }
        // 新版本把会话搬进了 SQLite；同一会话已有 rollout 文件时以文件为准。
        match crate::codex::load_threads(&root) {
            Ok(threads) => {
                for thread in threads {
                    if !seen.insert(thread.id.clone()) {
                        continue;
                    }
                    root_info.sessions += 1;
                    snapshot
                        .sessions
                        .push(codex_thread_session(thread, &root, &titles));
                }
            }
            Err(error) => snapshot.warnings.push(format!(
                "{}：读取 Codex 会话库失败：{error:#}",
                root.display()
            )),
        }
        snapshot.roots.push(root_info);
    }
}

/// 把 `threads` 表里的一行映射成统一的会话视图。
fn codex_thread_session(
    thread: crate::codex::Thread,
    root: &Path,
    titles: &HashMap<String, String>,
) -> Session {
    let preview = if thread.preview.trim().is_empty() {
        String::new()
    } else {
        let text = util::one_line(&thread.preview);
        if looks_like_injection(&text) {
            String::new()
        } else {
            util::truncate(&text, 160)
        }
    };
    let mut title = {
        let raw = thread.title.trim();
        if !raw.is_empty() {
            util::truncate(&util::one_line(raw), 60)
        } else if let Some(title) = titles.get(&thread.id) {
            title.clone()
        } else if preview.is_empty() {
            "(无标题)".to_string()
        } else {
            util::truncate(&preview, 60)
        }
    };
    if thread.archived {
        title = format!("{title}（已归档）");
    }
    let cwd = if thread.cwd.trim().is_empty() {
        None
    } else {
        Some(thread.cwd.clone())
    };
    let workspace = cwd
        .as_ref()
        .map(|cwd| Workspace::from_path(Client::Codex, cwd.clone(), root.to_path_buf()));
    // rollout 文件可能已经被清理，路径只用来定位与去重，删除时走会话库。
    let path = if thread.rollout.as_os_str().is_empty() {
        root.join("sessions")
            .join(format!("rollout-{}.jsonl", thread.id))
    } else {
        thread.rollout
    };
    Session {
        client: Client::Codex,
        id: thread.id,
        title,
        preview,
        path,
        root: root.to_path_buf(),
        cwd,
        workspace,
        created: thread.created_ms.and_then(util::system_time_from_millis),
        updated: thread.updated_ms.and_then(util::system_time_from_millis),
        bytes: thread.bytes,
        messages: thread.messages,
        tokens: None,
        cost: None,
        locked: false,
        bili: None,
        subagent: false,
        parent: None,
        magic: None,
    }
}

fn collect_rollouts(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollouts(&path, depth + 1, out);
        } else if path
            .file_name()
            .map(|name| {
                let name = name.to_string_lossy();
                name.starts_with("rollout-") && name.ends_with(".jsonl")
            })
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
}

fn codex_titles(index: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(text) = fs::read_to_string(index) else {
        return map;
    };
    for line in text.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            let (Some(id), Some(title)) = (
                value.get("id").and_then(Value::as_str),
                value.get("thread_name").and_then(Value::as_str),
            ) else {
                continue;
            };
            if !title.trim().is_empty() {
                map.insert(id.to_string(), util::one_line(title));
            }
        }
    }
    map
}

fn load_codex_session(
    file: &Path,
    root: &Path,
    titles: &HashMap<String, String>,
) -> Result<Session> {
    let mut id = String::new();
    let mut cwd = None;
    let mut created = None;
    let mut preview = String::new();
    let mut messages = 0u32;

    let handle = fs::File::open(file).with_context(|| format!("打开 {}", file.display()))?;
    for line in BufReader::new(handle).lines() {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = value.get("payload").cloned().unwrap_or(Value::Null);
        match kind {
            "session_meta" => {
                if id.is_empty() {
                    id = payload
                        .get("session_id")
                        .or_else(|| payload.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                }
                if cwd.is_none() {
                    cwd = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                if created.is_none() {
                    created = payload
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(parse_rfc3339);
                }
            }
            "response_item" if payload.get("type").and_then(Value::as_str) == Some("message") => {
                let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                if role == "user" || role == "assistant" {
                    messages += 1;
                }
                if role == "user"
                    && preview.is_empty()
                    && let Some(text) = codex_message_text(&payload)
                    && !looks_like_injection(&text)
                {
                    preview = util::truncate(&text, 160);
                }
            }
            _ => {}
        }
    }

    if id.is_empty() {
        id = codex_id_from_name(file);
    }

    let meta = fs::metadata(file)?;
    let updated = meta.modified().ok();
    // Codex 没有工作区登记表，用会话记录的 cwd 作为工作区。
    let workspace = cwd
        .as_ref()
        .map(|cwd| Workspace::from_path(Client::Codex, cwd.clone(), root.to_path_buf()));
    let title = titles
        .get(&id)
        .cloned()
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| {
            if preview.is_empty() {
                "(无标题)".to_string()
            } else {
                util::truncate(&preview, 60)
            }
        });

    Ok(Session {
        client: Client::Codex,
        id,
        title,
        preview,
        path: file.to_path_buf(),
        root: root.to_path_buf(),
        cwd,
        workspace,
        created,
        updated,
        bytes: meta.len(),
        messages,
        tokens: None,
        cost: None,
        locked: false,
        bili: None,
        subagent: false,
        parent: None,
        magic: None,
    })
}

fn codex_message_text(payload: &Value) -> Option<String> {
    let parts = payload.get("content")?.as_array()?;
    for part in parts {
        let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(kind, "input_text" | "text" | "output_text") {
            continue;
        }
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        let text = util::one_line(text);
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

fn codex_id_from_name(file: &Path) -> String {
    let name = file.file_stem().unwrap_or_default().to_string_lossy();
    // rollout-2026-01-01T00-00-00-<uuid>
    if name.len() >= 36 {
        let tail = &name[name.len() - 36..];
        if tail.chars().filter(|c| *c == '-').count() == 4 {
            return tail.to_string();
        }
    }
    name.to_string()
}

/// 极简 RFC3339 解析：`2026-10-29T10:31:12.701Z` / `+08:00`。
fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    use time_like::days_from_civil;
    let text = text.trim();
    let bytes = text.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let hour: i64 = text.get(11..13)?.parse().ok()?;
    let minute: i64 = text.get(14..16)?.parse().ok()?;
    let second: i64 = text.get(17..19)?.parse().ok()?;
    let mut epoch = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    // 时区偏移
    let rest = &text[19..];
    if let Some(offset) = rest.strip_suffix('Z').map(|_| 0) {
        epoch -= offset;
    } else if rest.len() >= 6 {
        let sign = rest.as_bytes()[0];
        if sign == b'+' || sign == b'-' {
            let hours: i64 = rest.get(1..3)?.parse().ok()?;
            let minutes: i64 = rest.get(4..6)?.parse().ok()?;
            let offset = hours * 3600 + minutes * 60;
            epoch -= if sign == b'+' { offset } else { -offset };
        }
    }
    if epoch < 0 {
        return None;
    }
    SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(epoch as u64))
}

mod time_like {
    /// Howard Hinnant 的 civil_from_days 逆运算。
    pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
        let year = if month <= 2 { year - 1 } else { year };
        let era = if year >= 0 { year } else { year - 399 } / 400;
        let yoe = year - era * 400;
        let mp = (month + 9) % 12;
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }
}

// ---------------------------------------------------------------- Opencode

fn scan_opencode(snapshot: &mut Snapshot) {
    let mut roots = env_roots("ASCLEAN_OPENCODE_DIR");
    if roots.is_empty() {
        let data = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .or_else(|| Some(home().join(".local").join("share")))
            .unwrap_or_else(|| PathBuf::from(".local/share"));
        roots = vec![data.join("opencode")];
    }
    for root in roots {
        let db = root.join("opencode.db");
        let mut root_info = Root {
            client: Client::Opencode,
            path: db.clone(),
            sessions: 0,
            error: None,
        };
        if !db.is_file() {
            root_info.error = Some("未找到 opencode.db".to_string());
            snapshot.roots.push(root_info);
            continue;
        }
        match load_opencode_sessions(&db, &root) {
            Ok((sessions, workspaces)) => {
                root_info.sessions = sessions.len();
                snapshot.sessions.extend(sessions);
                snapshot.workspaces.extend(workspaces);
            }
            Err(error) => root_info.error = Some(format!("{error:#}")),
        }
        snapshot.roots.push(root_info);
    }
}

/// Opencode 把工作区叫 `project`，单独一张表；即使还没有会话也会登记。
fn load_opencode_projects(conn: &rusqlite::Connection, root: &Path) -> Vec<Workspace> {
    let Ok(mut stmt) =
        conn.prepare("SELECT id, COALESCE(worktree, ''), COALESCE(name, '') FROM project")
    else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    }) else {
        return Vec::new();
    };
    rows.flatten()
        .filter_map(|(id, worktree, name)| {
            let worktree = worktree.trim();
            if worktree.is_empty() {
                return None;
            }
            let title = if name.trim().is_empty() {
                util::path_title(worktree)
            } else {
                util::one_line(&name)
            };
            Some(Workspace {
                client: Client::Opencode,
                id,
                path: worktree.to_string(),
                title,
                root: root.to_path_buf(),
            })
        })
        .collect()
}

fn load_opencode_sessions(db: &Path, root: &Path) -> Result<(Vec<Session>, Vec<Workspace>)> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("打开 {}", db.display()))?;
    let projects = load_opencode_projects(&conn, root);
    let mut stmt = conn.prepare(
        "SELECT s.id, COALESCE(s.title, ''), s.directory, s.time_created, s.time_updated,
                s.tokens_input, s.tokens_output, s.cost,
                (SELECT COUNT(*) FROM session_message m WHERE m.session_id = s.id),
                (SELECT COALESCE(SUM(LENGTH(m.data)), 0) FROM session_message m WHERE m.session_id = s.id),
                COALESCE(s.project_id, '')
         FROM session_v2 s
         ORDER BY s.time_updated DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, f64>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, i64>(9)?,
            row.get::<_, String>(10)?,
        ))
    })?;

    let mut sessions = Vec::new();
    for row in rows {
        let (
            id,
            title,
            directory,
            created,
            updated,
            tokens_in,
            tokens_out,
            cost,
            messages,
            bytes,
            project_id,
        ) = row?;
        let title = if title.trim().is_empty() {
            "(无标题)".to_string()
        } else {
            util::one_line(&title)
        };
        let workspace = projects
            .iter()
            .find(|workspace| workspace.id == project_id)
            .cloned()
            .or_else(|| {
                Some(Workspace::from_path(
                    Client::Opencode,
                    directory.clone(),
                    root.to_path_buf(),
                ))
            });
        sessions.push(Session {
            client: Client::Opencode,
            id,
            title,
            preview: String::new(),
            path: db.to_path_buf(),
            root: root.to_path_buf(),
            cwd: Some(directory),
            workspace,
            created: util::system_time_from_millis(created),
            updated: util::system_time_from_millis(updated),
            bytes: bytes.max(0) as u64,
            messages: messages.max(0) as u32,
            tokens: Some((tokens_in.max(0) as u64, tokens_out.max(0) as u64)),
            cost: Some(cost),
            locked: false,
            bili: None,
            subagent: false,
            parent: None,
            magic: None,
        });
    }
    Ok((sessions, projects))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339_with_zulu() {
        let time = parse_rfc3339("2026-10-29T10:31:12.701Z").unwrap();
        let secs = time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(secs, 1_793_269_872);
    }

    #[test]
    fn codex_id_falls_back_to_stem() {
        let path = Path::new(
            "/tmp/rollout-2026-10-29T18-30-58-01a18736-81c4-7400-837e-fa5b3fe08842.jsonl",
        );
        assert_eq!(
            codex_id_from_name(path),
            "01a18736-81c4-7400-837e-fa5b3fe08842"
        );
    }

    #[test]
    fn skips_injected_user_messages() {
        let record = serde_json::json!({
            "type": "user/message",
            "data": {
                "source": {"kind": "user"},
                "content": [{"type": "text", "text": "<system-reminder>hi</system-reminder>"}]
            }
        });
        assert!(dsh_user_text(&record).is_none());
    }

    #[test]
    fn parses_dsh_workspace_registry() {
        let value = serde_json::json!({
            "tables": {
                "workspaces": {
                    "ws-1": {
                        "path": "/work/projects/",
                        "title": "Projects",
                        "sessionIds": ["session-a", "session-b"]
                    },
                    "ws-2": {
                        "path": "/work/other",
                        "sessionIds": []
                    }
                }
            }
        });
        let workspaces = parse_dsh_workspaces(&value, Path::new("/root"));
        assert_eq!(workspaces.all.len(), 2);
        // 登记表里的 id 带 `session-` 前缀，键统一存裸 id。
        assert_eq!(
            workspaces.session_owner.get("a").map(String::as_str),
            Some("ws-1")
        );
        assert_eq!(
            workspaces.owner_of("session-a").map(|ws| ws.id.as_str()),
            Some("ws-1")
        );
        let by_path = workspaces.by_path("/work/projects").expect("路径匹配");
        assert_eq!(by_path.id, "ws-1");
        assert_eq!(by_path.title, "Projects");
        let other = workspaces.by_id("ws-2").expect("标识匹配");
        assert_eq!(other.title, "other", "没有标题时回退到目录名");
    }
}
