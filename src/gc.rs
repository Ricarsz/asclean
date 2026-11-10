//! 清理残留：会话目录已经被删掉，但工作区登记表、渲染缓存和
//! billion-context 数据库里还留着记录的情况。
//!
//! 判据只有一条 —— 「这个 id 在本机已经找不到会话目录」。为了避免误删，
//! 只处理 DSH 形状的 id（`<uuid>` / `session-<uuid>`），并且一次扫不到
//! 任何 DSH 会话时直接放弃，宁可少清也不清错。

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::bili;
use crate::delete;
use crate::model::Client;
use crate::scan;
use crate::util;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    pub dry_run: bool,
    /// 从 `workspace.json` 摘掉的会话 id 数。
    pub registry: usize,
    /// 从 `session_projcache` 删掉的缓存文件数。
    pub projcache: usize,
    /// 从 billion-context 删掉的会话记录数。
    pub bili_records: usize,
    /// 从 billion-context 释放的字节数。
    pub bili_bytes: u64,
    /// 残留记录的标题样本，CLI 里列出来方便核对。
    pub samples: Vec<String>,
    pub warnings: Vec<String>,
}

impl GcReport {
    pub fn total(&self) -> usize {
        self.registry + self.projcache + self.bili_records
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0 && self.warnings.is_empty()
    }
}

/// 扫描一遍，把没有会话目录的 DSH 记录从各处索引里摘掉。
/// `dry_run` 为真时只统计，不落盘。
pub fn run(dry_run: bool) -> GcReport {
    let mut report = GcReport {
        dry_run,
        ..GcReport::default()
    };
    let snapshot = scan::scan();
    // 判定“目录还在不在”只看磁盘：用全机所有 DSH 根，而不是本次扫描范围。
    let present: HashSet<String> = scan::dsh_session_dirs()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    if present.is_empty() {
        report
            .warnings
            .push("没找到任何 DSH 会话目录，跳过清理以免误删".to_string());
        return report;
    }

    for root in snapshot
        .roots
        .iter()
        .filter(|root| root.client == Client::Dsh)
    {
        if let Some(error) = &root.error {
            report
                .warnings
                .push(format!("{}：{error}", root.path.display()));
            continue;
        }
        match registry_step(&root.path, &present, dry_run) {
            Ok(count) => report.registry += count,
            Err(error) => report
                .warnings
                .push(format!("{}：{error:#}", root.path.display())),
        }
        match projcache_step(&root.path, &present, dry_run) {
            Ok(count) => report.projcache += count,
            Err(error) => report
                .warnings
                .push(format!("{}：{error:#}", root.path.display())),
        }
    }

    for db in bili::databases() {
        match bili_step(&db, &present, dry_run, &mut report.samples) {
            Ok((records, bytes)) => {
                report.bili_records += records;
                report.bili_bytes += bytes;
            }
            Err(error) => report.warnings.push(format!("{}：{error:#}", db.display())),
        }
    }
    report
}

fn registry_step(
    root: &Path,
    present: &HashSet<String>,
    dry_run: bool,
) -> Result<usize, anyhow::Error> {
    if dry_run {
        return Ok(delete::dangling_registry(root, present)?.len());
    }
    delete::prune_registry(root, present)
}

fn projcache_step(
    root: &Path,
    present: &HashSet<String>,
    dry_run: bool,
) -> Result<usize, anyhow::Error> {
    let dir = root
        .join("storages")
        .join("session_projcache")
        .join("sessions");
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut stale: Vec<std::path::PathBuf> = Vec::new();
    for entry in fs::read_dir(&dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if util::looks_like_dsh_id(id) && !present.contains(util::dsh_core(id)) {
            stale.push(entry.path());
        }
    }
    if dry_run {
        return Ok(stale.len());
    }
    let mut removed = 0;
    for path in stale {
        if fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

fn bili_step(
    db: &Path,
    present: &HashSet<String>,
    dry_run: bool,
    samples: &mut Vec<String>,
) -> Result<(usize, u64), anyhow::Error> {
    let mut records = 0;
    let mut bytes = 0;
    for entry in bili::entries(db)? {
        // 只动 DSH 自己记的会话，别的客户端（如 dsh-pet）一律不碰。
        if entry.namespace != "dsh" {
            continue;
        }
        if !util::looks_like_dsh_id(&entry.id) || present.contains(util::dsh_core(&entry.id)) {
            continue;
        }
        if dry_run {
            bytes += bili::bytes_of(db, &entry.id).unwrap_or(0);
            if samples.len() < 5 {
                samples.push(entry.title.clone());
            }
        } else {
            bytes += bili::delete_session(db, &entry.id)?;
        }
        records += 1;
    }
    Ok((records, bytes))
}
