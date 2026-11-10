//! 会话模型：把三个 AI 编码客户端的会话整理成同一份视图。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::util;

/// 会话来源客户端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Client {
    Dsh,
    Codex,
    Opencode,
}

impl Client {
    pub const ALL: [Client; 3] = [Client::Dsh, Client::Codex, Client::Opencode];

    /// 稳定标识，用于命令行参数与会话 key。
    pub fn key(self) -> &'static str {
        match self {
            Client::Dsh => "dsh",
            Client::Codex => "codex",
            Client::Opencode => "opencode",
        }
    }

    /// 界面上展示的全称。
    pub fn label(self) -> &'static str {
        match self {
            Client::Dsh => "DeepSeek Harness",
            Client::Codex => "Codex",
            Client::Opencode => "Opencode",
        }
    }

    /// 列表里的短徽标，统一三列宽。
    pub fn badge(self) -> &'static str {
        match self {
            Client::Dsh => "DSH",
            Client::Codex => "CDX",
            Client::Opencode => "OC",
        }
    }

    /// 解析命令行或过滤参数里的客户端名。
    pub fn from_key(value: &str) -> Option<Client> {
        match value.trim().to_lowercase().as_str() {
            "dsh" | "deepseek" | "harness" => Some(Client::Dsh),
            "codex" | "cdx" => Some(Client::Codex),
            "opencode" | "oc" => Some(Client::Opencode),
            _ => None,
        }
    }
}

/// 会话所属的工作区。
///
/// 三个客户端的来源不同：
/// - DSH：`~/.dsh*/storages/workspace.json` 里登记的工作区；
/// - Opencode：数据库 `project` 表里的项目；
/// - Codex：没有登记表，按会话记录的 `cwd` 合成。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Workspace {
    pub client: Client,
    /// 客户端内部标识：DSH 是工作区 UUID，Opencode 是项目 ID，Codex 是 `path:<目录>`。
    pub id: String,
    /// 工作区目录（绝对路径）。
    pub path: String,
    pub title: String,
    /// 工作区所在的扫描根，注销登记项时用来定位存储。
    pub root: PathBuf,
}

impl Workspace {
    /// 全局唯一 key，用作视图的状态标识。
    pub fn key(&self) -> String {
        format!("{}:{}", self.client.key(), self.id)
    }

    /// 没有登记表时按目录合成一个工作区。
    pub fn from_path(client: Client, path: impl Into<String>, root: PathBuf) -> Self {
        let path = path.into();
        let title = util::path_title(&path);
        Self {
            client,
            id: format!("path:{path}"),
            path,
            title,
            root,
        }
    }

    /// 是否由目录路径合成，而不是客户端的登记项。
    pub fn is_synthetic(&self) -> bool {
        self.id.starts_with("path:")
    }
}

/// billion-context 为同一个 DSH 会话留下的存储记录。
///
/// bili 的 `session_id` 与 DSH 会话目录同名，所以扫描时能直接按 id 对上。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BiliMeta {
    /// 记录所在的 `index.db`。
    pub db: PathBuf,
    /// 内容字节数（按内容 hash 去重后累加）。
    pub bytes: u64,
    pub messages: u32,
    pub updated: Option<SystemTime>,
    pub status: String,
}

/// magic-context 为同一个 opencode 会话留下的存储记录。
///
/// magic-context 的 `session_id` 与 opencode 的会话 id 同名，所以扫描时
/// 能直接按 id 对上。它没有 bili 那样的内容表，用隔间数衡量体量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagicMeta {
    /// 记录所在的 `context.db`。
    pub db: PathBuf,
    /// 已经切出来的隔间数。
    pub compartments: u32,
    pub updated: Option<SystemTime>,
    /// 记录该会话的 harness，通常是 `opencode2`。
    pub status: String,
}

/// 一条会话记录。
#[derive(Debug, Clone)]
pub struct Session {
    pub client: Client,
    pub id: String,
    pub title: String,
    /// 首条用户消息，作为详情里的摘要。
    pub preview: String,
    /// 会话自身的存储位置：DSH 是会话目录，Codex 是 rollout 文件，Opencode 是数据库文件。
    pub path: PathBuf,
    /// 会话所在的扫描根。
    pub root: PathBuf,
    pub cwd: Option<String>,
    /// 会话所属工作区；识别不到时为空。
    pub workspace: Option<Workspace>,
    pub created: Option<SystemTime>,
    pub updated: Option<SystemTime>,
    pub bytes: u64,
    pub messages: u32,
    pub tokens: Option<(u64, u64)>,
    pub cost: Option<f64>,
    pub locked: bool,
    /// DSH 的子会话（由某个父会话派生，`origin: subagent`）。
    pub subagent: bool,
    /// 子会话的父会话标识。
    pub parent: Option<String>,
    /// billion-context 侧的同一会话；没接入或没数据时为空。
    pub bili: Option<BiliMeta>,
    /// magic-context 侧的同一会话；没接入或没数据时为空。
    pub magic: Option<MagicMeta>,
}

impl Session {
    /// 全局唯一 key，形如 `dsh:session-<uuid>`。
    pub fn key(&self) -> String {
        format!("{}:{}", self.client.key(), self.id)
    }

    /// 排序与比较用的时间，缺失时视为最旧。
    pub fn updated_or_epoch(&self) -> SystemTime {
        self.updated.unwrap_or(SystemTime::UNIX_EPOCH)
    }

    pub fn time_label(&self) -> String {
        self.updated
            .map(util::local_short)
            .unwrap_or_else(|| "--".into())
    }

    pub fn age_label(&self) -> String {
        self.updated
            .map(util::human_age)
            .unwrap_or_else(|| "未知".into())
    }

    pub fn size_label(&self) -> String {
        util::human_bytes(self.bytes)
    }

    /// billion-context 侧的占用；没接入时是 0。
    pub fn bili_bytes(&self) -> u64 {
        self.bili.as_ref().map_or(0, |meta| meta.bytes)
    }

    /// magic-context 侧的隔间数；没接入时是 0。
    pub fn magic_compartments(&self) -> u32 {
        self.magic.as_ref().map_or(0, |meta| meta.compartments)
    }

    /// 删除时总共会释放的字节数（会话自身 + bili 附带的数据）。
    pub fn total_bytes(&self) -> u64 {
        self.bytes + self.bili_bytes()
    }

    /// 关键字过滤：标题、标识、目录、工作区名与客户端名。
    pub fn matches(&self, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        let haystack = format!(
            "{} {} {} {} {}",
            self.title,
            self.id,
            self.cwd.as_deref().unwrap_or(""),
            self.workspace
                .as_ref()
                .map(|workspace| workspace.title.as_str())
                .unwrap_or(""),
            self.client.label()
        )
        .to_lowercase();
        haystack.contains(filter)
    }

    pub fn workspace_key(&self) -> Option<String> {
        self.workspace.as_ref().map(Workspace::key)
    }

    /// 工作区视图里的分组键：识别不到工作区的会话按客户端归到一组。
    pub fn workspace_group_key(&self) -> String {
        self.workspace_key()
            .unwrap_or_else(|| format!("{}:?", self.client.key()))
    }
}

/// 工作区视图里的一行：一个工作区及其名下的会话。
#[derive(Debug, Clone)]
pub struct WorkspaceGroup {
    pub key: String,
    pub client: Client,
    /// 登记项；按目录合成的会话也会有值，只有“未识别”组为空。
    pub workspace: Option<Workspace>,
    /// `sessions` 里的下标，按会话顺序排列。
    pub indices: Vec<usize>,
    pub bytes: u64,
    pub messages: u32,
    pub updated: Option<SystemTime>,
    pub locked: usize,
}

impl WorkspaceGroup {
    pub fn title(&self) -> &str {
        self.workspace
            .as_ref()
            .map(|workspace| workspace.title.as_str())
            .unwrap_or("未识别工作区")
    }

    pub fn path(&self) -> Option<&str> {
        self.workspace
            .as_ref()
            .map(|workspace| workspace.path.as_str())
    }

    pub fn count(&self) -> usize {
        self.indices.len()
    }

    pub fn size_label(&self) -> String {
        util::human_bytes(self.bytes)
    }

    pub fn time_label(&self) -> String {
        self.updated
            .map(util::local_short)
            .unwrap_or_else(|| "--".into())
    }

    pub fn age_label(&self) -> String {
        self.updated
            .map(util::human_age)
            .unwrap_or_else(|| "未知".into())
    }

    /// 过滤串是否命中工作区（标题、路径、标识）。
    pub fn matches(&self, filter: &str) -> bool {
        let filter = filter.to_lowercase();
        if filter.is_empty() {
            return true;
        }
        let haystack = format!(
            "{} {} {} {}",
            self.title(),
            self.path().unwrap_or(""),
            self.workspace.as_ref().map(|w| w.id.as_str()).unwrap_or(""),
            self.client.label()
        )
        .to_lowercase();
        haystack.contains(&filter)
    }

    /// 命令行的 `<spec>`：标识、完整路径、标题，或路径的末段。
    pub fn matches_spec(&self, spec: &str) -> bool {
        let spec = spec.trim_end_matches('/');
        let Some(workspace) = &self.workspace else {
            return spec == "?" || spec.eq_ignore_ascii_case("unknown");
        };
        let path = workspace.path.trim_end_matches('/');
        workspace.id == spec
            || path == spec
            || workspace.title.eq_ignore_ascii_case(spec)
            || path.ends_with(&format!("/{spec}"))
    }
}

/// 把会话按工作区分组。
///
/// `known` 是扫描时发现的登记项（可能还没有会话），先放进结果；再按会话补上
/// 只在会话里出现的工作区（例如 Codex 的 `cwd`）。
pub fn group_workspaces(sessions: &[Session], known: &[Workspace]) -> Vec<WorkspaceGroup> {
    let mut map: BTreeMap<String, WorkspaceGroup> = BTreeMap::new();

    for workspace in known {
        let key = workspace.key();
        map.entry(key.clone()).or_insert_with(|| WorkspaceGroup {
            key,
            client: workspace.client,
            workspace: Some(workspace.clone()),
            indices: Vec::new(),
            bytes: 0,
            messages: 0,
            updated: None,
            locked: 0,
        });
    }

    for (index, session) in sessions.iter().enumerate() {
        let key = session.workspace_group_key();
        let group = map.entry(key.clone()).or_insert_with(|| WorkspaceGroup {
            key,
            client: session.client,
            workspace: session.workspace.clone(),
            indices: Vec::new(),
            bytes: 0,
            messages: 0,
            updated: None,
            locked: 0,
        });
        group.indices.push(index);
        group.bytes += session.total_bytes();
        group.messages += session.messages;
        group.locked += usize::from(session.locked);
        let updated = session.updated;
        group.updated = match (group.updated, updated) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }

    let mut groups: Vec<WorkspaceGroup> = map.into_values().collect();
    groups.sort_by(|a, b| {
        a.workspace
            .is_none()
            .cmp(&b.workspace.is_none())
            .then_with(|| {
                b.updated
                    .unwrap_or(SystemTime::UNIX_EPOCH)
                    .cmp(&a.updated.unwrap_or(SystemTime::UNIX_EPOCH))
            })
            .then_with(|| a.title().cmp(b.title()))
            .then_with(|| a.key.cmp(&b.key))
    });
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn session(client: Client, id: &str, workspace: Option<Workspace>, bytes: u64) -> Session {
        Session {
            client,
            id: id.into(),
            title: id.into(),
            preview: String::new(),
            path: PathBuf::from("/tmp/x"),
            root: PathBuf::from("/tmp"),
            cwd: None,
            workspace,
            created: None,
            updated: Some(UNIX_EPOCH + Duration::from_secs(bytes)),
            bytes,
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
    fn groups_known_workspaces_even_without_sessions() {
        let known = vec![
            Workspace {
                client: Client::Dsh,
                id: "ws-1".into(),
                path: "/work/a".into(),
                title: "a".into(),
                root: PathBuf::from("/tmp"),
            },
            Workspace {
                client: Client::Opencode,
                id: "pr-1".into(),
                path: "/work/b".into(),
                title: "b".into(),
                root: PathBuf::from("/tmp"),
            },
        ];
        let sessions = vec![session(Client::Dsh, "s1", Some(known[0].clone()), 100)];

        let groups = group_workspaces(&sessions, &known);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].key, "dsh:ws-1");
        assert_eq!(groups[0].count(), 1);
        assert_eq!(groups[0].bytes, 100);
        assert_eq!(
            groups[0].updated,
            Some(UNIX_EPOCH + Duration::from_secs(100))
        );
        assert_eq!(groups[1].count(), 0);
    }

    #[test]
    fn unknown_sessions_group_per_client_and_sort_last() {
        let known = vec![Workspace {
            client: Client::Dsh,
            id: "ws-1".into(),
            path: "/work/a".into(),
            title: "a".into(),
            root: PathBuf::from("/tmp"),
        }];
        let sessions = vec![
            session(Client::Codex, "c1", None, 1),
            session(Client::Dsh, "s1", None, 2),
        ];
        let groups = group_workspaces(&sessions, &known);
        let keys: Vec<_> = groups.iter().map(|group| group.key.as_str()).collect();
        assert_eq!(keys, ["dsh:ws-1", "dsh:?", "codex:?"]);
        assert_eq!(groups[1].title(), "未识别工作区");
        assert_eq!(groups[1].count(), 1);
    }

    #[test]
    fn derives_group_key_from_cwd_when_missing() {
        let session = session(Client::Codex, "c1", None, 1);
        assert_eq!(session.workspace_group_key(), "codex:?");
    }

    #[test]
    fn matches_spec_by_path_id_title_and_suffix() {
        let workspace = Workspace {
            client: Client::Opencode,
            id: "pr-1".into(),
            path: "/work/projects/demo".into(),
            title: "Demo".into(),
            root: PathBuf::from("/tmp"),
        };
        let group = WorkspaceGroup {
            key: workspace.key(),
            client: Client::Opencode,
            workspace: Some(workspace),
            indices: vec![0],
            bytes: 0,
            messages: 0,
            updated: None,
            locked: 0,
        };
        assert!(group.matches_spec("pr-1"));
        assert!(group.matches_spec("/work/projects/demo"));
        assert!(group.matches_spec("/work/projects/demo/"));
        assert!(group.matches_spec("demo"));
        assert!(group.matches_spec("demo"));
        assert!(group.matches("demo"));
        assert!(!group.matches_spec("other"));
    }
}
