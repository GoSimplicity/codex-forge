use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::harness::types::{ApprovalRecord, SubagentRecord, TaskNodeRecord, ToolCallRecord};

pub(super) fn append_jsonl<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    ensure_parent(path)?;
    let payload = serde_json::to_string(value).context("序列化 JSONL 记录失败")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("打开 JSONL 文件失败：{}", path.display()))?;
    writeln!(file, "{payload}").with_context(|| format!("写入 JSONL 文件失败：{}", path.display()))
}

pub(super) fn overwrite_jsonl<T: serde::Serialize>(path: &Path, values: &[T]) -> Result<()> {
    ensure_parent(path)?;
    let payload = if values.is_empty() {
        String::new()
    } else {
        let mut lines = values
            .iter()
            .map(|item| serde_json::to_string(item).context("序列化 JSONL 记录失败"))
            .collect::<Result<Vec<_>>>()?
            .join("\n");
        lines.push('\n');
        lines
    };
    fs::write(path, payload).with_context(|| format!("覆盖 JSONL 文件失败：{}", path.display()))
}

pub(super) fn rewrite_jsonl<T, F>(path: &Path, rewrite: F) -> Result<()>
where
    T: serde::Serialize + for<'de> serde::Deserialize<'de>,
    F: FnOnce(&mut Vec<T>),
{
    let mut items: Vec<T> = read_jsonl(path)?;
    rewrite(&mut items);
    overwrite_jsonl(path, &items)
}

pub(super) fn read_jsonl<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("读取 JSONL 文件失败：{}", path.display()))?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<T>(line).context("解析 JSONL 记录失败"))
        .collect()
}

pub(super) fn replace_by_id<T>(items: &mut [T], id: &str, updated: T)
where
    T: RecordId,
{
    if let Some(index) = items.iter().position(|item| item.record_id() == id) {
        items[index] = updated;
    }
}

pub(super) trait RecordId {
    fn record_id(&self) -> &str;
}

impl RecordId for ToolCallRecord {
    fn record_id(&self) -> &str {
        &self.id
    }
}

impl RecordId for ApprovalRecord {
    fn record_id(&self) -> &str {
        &self.id
    }
}

impl RecordId for SubagentRecord {
    fn record_id(&self) -> &str {
        &self.id
    }
}

impl RecordId for TaskNodeRecord {
    fn record_id(&self) -> &str {
        &self.id
    }
}

fn ensure_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        bail!("路径缺少父目录：{}", path.display());
    };
    fs::create_dir_all(parent).with_context(|| format!("创建父目录失败：{}", parent.display()))
}
