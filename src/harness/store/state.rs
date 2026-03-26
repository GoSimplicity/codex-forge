use std::fs;

use anyhow::{Context, Result};

use crate::harness::types::{
    EvaluationDecision, ExecutionContract, HarnessRunManifest, ProgressLedger,
};

use super::HarnessStore;
use super::jsonl::{append_jsonl, read_jsonl, write_atomic};

impl HarnessStore {
    pub fn save_execution_contract(
        &self,
        thread_id: &str,
        contract: &ExecutionContract,
    ) -> Result<()> {
        let thread = self.load_thread(thread_id)?;
        write_atomic(
            &thread.contract_path,
            &serde_json::to_vec_pretty(contract).context("序列化 execution contract 失败")?,
        )
        .with_context(|| {
            format!(
                "写入 execution contract 失败：{}",
                thread.contract_path.display()
            )
        })
    }

    pub fn load_execution_contract(&self, thread_id: &str) -> Result<ExecutionContract> {
        let thread = self.load_thread(thread_id)?;
        let raw = fs::read_to_string(&thread.contract_path).with_context(|| {
            format!(
                "读取 execution contract 失败：{}",
                thread.contract_path.display()
            )
        })?;
        serde_json::from_str(&raw).with_context(|| {
            format!(
                "解析 execution contract 失败：{}",
                thread.contract_path.display()
            )
        })
    }

    pub fn save_progress_ledger(&self, thread_id: &str, progress: &ProgressLedger) -> Result<()> {
        let thread = self.load_thread(thread_id)?;
        write_atomic(
            &thread.progress_path,
            &serde_json::to_vec_pretty(progress).context("序列化 progress ledger 失败")?,
        )
        .with_context(|| {
            format!(
                "写入 progress ledger 失败：{}",
                thread.progress_path.display()
            )
        })
    }

    pub fn load_progress_ledger(&self, thread_id: &str) -> Result<ProgressLedger> {
        let thread = self.load_thread(thread_id)?;
        let raw = fs::read_to_string(&thread.progress_path).with_context(|| {
            format!(
                "读取 progress ledger 失败：{}",
                thread.progress_path.display()
            )
        })?;
        serde_json::from_str(&raw).with_context(|| {
            format!(
                "解析 progress ledger 失败：{}",
                thread.progress_path.display()
            )
        })
    }

    pub fn write_session_bootstrap(
        &self,
        thread_id: &str,
        run: &HarnessRunManifest,
        content: &str,
    ) -> Result<()> {
        let thread = self.load_thread(thread_id)?;
        fs::write(&run.bootstrap_path, content).with_context(|| {
            format!("写入 run bootstrap 失败：{}", run.bootstrap_path.display())
        })?;
        fs::write(&thread.bootstrap_path, content).with_context(|| {
            format!(
                "写入 thread bootstrap 失败：{}",
                thread.bootstrap_path.display()
            )
        })
    }

    pub fn read_session_bootstrap(&self, thread_id: &str) -> Result<String> {
        let thread = self.load_thread(thread_id)?;
        fs::read_to_string(&thread.bootstrap_path).with_context(|| {
            format!(
                "读取 session bootstrap 失败：{}",
                thread.bootstrap_path.display()
            )
        })
    }

    pub fn append_evaluation(
        &self,
        run: &HarnessRunManifest,
        decision: &EvaluationDecision,
    ) -> Result<()> {
        append_jsonl(&run.evaluation_log_path, decision)
    }

    pub fn list_evaluations(&self, run: &HarnessRunManifest) -> Result<Vec<EvaluationDecision>> {
        let mut items: Vec<EvaluationDecision> = read_jsonl(&run.evaluation_log_path)?;
        items.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(items)
    }
}
