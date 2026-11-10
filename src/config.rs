//! `~/.config/asclean/config.toml`，缺失时全部使用默认值。

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// 配色：ocean | forest | sunset。
    pub theme: String,
    /// 是否启用鼠标（滚轮滚动、点击选择）。
    pub mouse: bool,
    /// 自动重扫间隔（秒），最小 5。
    pub rescan_secs: u64,
    /// 删除前是否弹确认框。
    pub confirm_delete: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "ocean".to_string(),
            mouse: true,
            rescan_secs: 30,
            confirm_delete: true,
        }
    }
}

fn config_dir() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("asclean").join("config.toml")
    }

    /// 改名前的配置路径 `~/.config/sesstui/config.toml`：只读兜底，
    /// 免得老用户的设置静默失效；写入永远走新路径。
    fn legacy_path() -> PathBuf {
        config_dir().join("sesstui").join("config.toml")
    }

    /// 写入默认配置；已存在时只有 `force` 才覆盖。
    pub fn init(force: bool) -> Result<PathBuf> {
        let path = Self::path();
        if path.exists() && !force {
            bail!("配置已存在：{}（加 --force 覆盖）", path.display());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("创建目录 {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(&Self::default()).context("序列化默认配置失败")?;
        fs::write(&path, text).with_context(|| format!("写入 {}", path.display()))?;
        Ok(path)
    }

    pub fn load() -> Result<Self> {
        let mut path = Self::path();
        if !path.exists() {
            let legacy = Self::legacy_path();
            if legacy.exists() {
                path = legacy;
            } else {
                return Ok(Self::default());
            }
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("读取配置 {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("解析配置 {}", path.display()))
    }

    pub fn rescan_interval(&self) -> Duration {
        Duration::from_secs(self.rescan_secs.max(5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let config = Config::default();
        assert!(config.rescan_interval() >= Duration::from_secs(5));
        assert!(config.confirm_delete);
    }

    #[test]
    fn parses_partial_document() {
        let config: Config = toml::from_str("theme = \"forest\"").unwrap();
        assert_eq!(config.theme, "forest");
        assert!(config.mouse);
    }
}
