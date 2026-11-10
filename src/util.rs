//! 小工具：字节/时间格式化、显示宽度截断、目录大小与会话锁检测。

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// 相对时间：刚刚 / N 分钟前 / N 小时前 / N 天前 / 具体日期。
/// 千分位数字，用于令牌数等不宜按字节表示的量。
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub fn human_age(time: SystemTime) -> String {
    let Ok(age) = SystemTime::now().duration_since(time) else {
        return "刚刚".to_string();
    };
    let secs = age.as_secs();
    if secs < 60 {
        "刚刚".to_string()
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86_400 {
        format!("{} 小时前", secs / 3600)
    } else if secs < 30 * 86_400 {
        format!("{} 天前", secs / 86_400)
    } else {
        local_date(time)
    }
}

fn local_tm(time: SystemTime) -> Option<libc::tm> {
    let secs = time.duration_since(UNIX_EPOCH).ok()?.as_secs() as libc::time_t;
    let raw = secs;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&raw, &mut tm).is_null() };
    ok.then_some(tm)
}

pub fn local_datetime(time: SystemTime) -> String {
    match local_tm(time) {
        Some(tm) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        ),
        None => "--".to_string(),
    }
}

pub fn local_date(time: SystemTime) -> String {
    match local_tm(time) {
        Some(tm) => format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        ),
        None => "--".to_string(),
    }
}

/// 列表用的短时间：`MM-DD HH:MM`。
pub fn local_short(time: SystemTime) -> String {
    match local_tm(time) {
        Some(tm) => format!(
            "{:02}-{:02} {:02}:{:02}",
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        ),
        None => "--".to_string(),
    }
}

/// 按显示宽度截断，超出时补 `…`。
pub fn truncate(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    let budget = max_width.saturating_sub(1);
    let mut width = 0;
    let mut out = String::new();
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if width + w > budget {
            break;
        }
        width += w;
        out.push(ch);
    }
    out.push('…');
    out
}

/// 把多行文本压成一行，方便列表与状态栏显示。
pub fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = false;
    for ch in text.chars() {
        let ch = if ch.is_whitespace() { ' ' } else { ch };
        if ch == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

/// 目录路径的显示名：取最后一段；根目录保持 `/`。
pub fn path_title(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "(未知路径)".into();
    }
    let trimmed = trimmed.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// 目录占用（只统计一层文件，会话目录不会嵌套很深）。
pub fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        match entry.metadata() {
            Ok(meta) if meta.is_file() => total += meta.len(),
            Ok(meta) if meta.is_dir() => total += dir_size(&entry.path()),
            _ => {}
        }
    }
    total
}

/// 会话锁是否被别的进程持有（flock 非阻塞探测）。
pub fn lock_held(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0;
    // file 析构时自动释放临时加的锁。
    held
}

pub fn system_time_from_millis(millis: i64) -> Option<SystemTime> {
    if millis <= 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_millis(millis as u64))
}

/// DSH 会话标识的裸形式：老版本目录名与记录里的 `id` 都是裸 UUID，
/// 新版本带 `session-` 前缀，工作区登记表里两种写法混着出现。
pub fn dsh_core(id: &str) -> &str {
    id.strip_prefix("session-").unwrap_or(id)
}

/// 两个 DSH 标识是否指同一个会话（忽略 `session-` 前缀差异）。
pub fn dsh_same_id(a: &str, b: &str) -> bool {
    dsh_core(a) == dsh_core(b)
}

/// 是否像 DSH 会话标识：`<uuid>` 或 `session-<uuid>`。
///
/// 用来把「DSH 会话」与 billion-context 里别的客户端记的杂项
/// （例如 `dsh-pet`）区分开，清理残留时只认这种形状。
pub fn looks_like_dsh_id(id: &str) -> bool {
    let core = dsh_core(id);
    let bytes = core.as_bytes();
    if core.len() != 36 {
        return false;
    }
    let dashes_ok = [8usize, 13, 18, 23]
        .iter()
        .all(|index| bytes.get(*index) == Some(&b'-'));
    let charset_ok = core.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-');
    dashes_ok && charset_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(50 * 1024 * 1024), "50 MB");
    }

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate("abcdef", 6), "abcdef");
        assert_eq!(truncate("abcdef", 4), "abc…");
        // 每个汉字占两列。
        assert_eq!(truncate("中文标题", 5), "中文…");
        assert_eq!(truncate("中文标题", 0), "");
    }

    #[test]
    fn collapses_whitespace() {
        assert_eq!(one_line("a\n\n  b\t c "), "a b c");
    }

    #[test]
    fn derives_path_title() {
        assert_eq!(path_title("/home/user/projects"), "projects");
        assert_eq!(path_title("/home/user/projects/"), "projects");
        assert_eq!(path_title("/"), "/");
        assert_eq!(path_title(""), "(未知路径)");
    }

    #[test]
    fn recognises_dsh_session_ids() {
        assert!(looks_like_dsh_id("d5f700e1-2818-403a-bca9-6fbe99806eaf"));
        assert!(looks_like_dsh_id(
            "session-d5f700e1-2818-403a-bca9-6fbe99806eaf"
        ));
        assert!(!looks_like_dsh_id("dsh-pet"));
        assert!(!looks_like_dsh_id("session-not-a-uuid"));
        assert!(!looks_like_dsh_id("--home-ricarse--"));
        assert_eq!(dsh_core("session-abc"), "abc");
        assert_eq!(dsh_core("abc"), "abc");
        assert!(dsh_same_id("abc", "session-abc"));
        assert!(!dsh_same_id("abc", "abd"));
    }
}
