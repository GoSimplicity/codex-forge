use std::fs;

use anyhow::{Context, Result};
use chrono::Utc;

use crate::harness::types::{MemoryEntry, MemoryLayer, ThreadMemory};

use super::HarnessStore;
use super::ids::make_id;
use super::jsonl::write_atomic;

impl HarnessStore {
    pub fn load_memory(&self, thread_id: &str, layer: MemoryLayer) -> Result<ThreadMemory> {
        let thread = self.load_thread(thread_id)?;
        let path = memory_path(&thread.memory_dir, layer);
        if !path.exists() {
            return Ok(ThreadMemory {
                layer,
                updated_at: Utc::now(),
                entries: Vec::new(),
            });
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("读取 memory 文件失败：{}", path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("解析 memory 文件失败：{}", path.display()))
    }

    pub fn append_memory_entry(
        &self,
        thread_id: &str,
        layer: MemoryLayer,
        content: String,
        source: String,
        run_id: Option<String>,
        task_node_id: Option<String>,
    ) -> Result<MemoryEntry> {
        let mut memory = self.load_memory(thread_id, layer)?;
        let entry = MemoryEntry {
            id: make_id("memory"),
            content: content.trim().to_string(),
            source,
            created_at: Utc::now(),
            run_id,
            task_node_id,
        };
        if entry.content.is_empty() {
            return Ok(entry);
        }
        if layer == MemoryLayer::Project
            && memory
                .entries
                .iter()
                .any(|item| item.content.trim() == entry.content.trim())
        {
            return Ok(entry);
        }
        memory.entries.push(entry.clone());
        if layer == MemoryLayer::Working && memory.entries.len() > 32 {
            let drain = memory.entries.len().saturating_sub(32);
            memory.entries.drain(0..drain);
        }
        memory.updated_at = Utc::now();
        self.persist_memory(thread_id, &memory)?;
        Ok(entry)
    }

    pub fn persist_memory(&self, thread_id: &str, memory: &ThreadMemory) -> Result<()> {
        let thread = self.load_thread(thread_id)?;
        let path = memory_path(&thread.memory_dir, memory.layer);
        write_atomic(
            &path,
            &serde_json::to_vec_pretty(memory).context("序列化 memory 失败")?,
        )
        .with_context(|| format!("写入 memory 文件失败：{}", path.display()))
    }
}

fn memory_path(memory_dir: &std::path::Path, layer: MemoryLayer) -> std::path::PathBuf {
    match layer {
        MemoryLayer::Working => memory_dir.join("working-memory.json"),
        MemoryLayer::Project => memory_dir.join("project-memory.json"),
    }
}
