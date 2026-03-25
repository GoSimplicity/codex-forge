mod parser;
mod prompt;

use std::path::Path;

use anyhow::Result;

use crate::codex::{ensure_codex_available, run_text_once};
use crate::model::ThinkingMode;

use super::types::{HarnessMessage, HarnessThreadManifest, TurnEnvelope};

pub use parser::parse_turn_envelope;
pub use prompt::render_lead_turn_prompt;

#[derive(Debug, Clone)]
pub struct BackendTurnRequest<'a> {
    pub thread: &'a HarnessThreadManifest,
    pub messages: &'a [HarnessMessage],
    pub thinking_mode: ThinkingMode,
    pub model: Option<&'a str>,
    pub timeout_secs: u64,
    pub tools: &'a [ToolDescriptor],
    pub system_hint: &'a str,
    pub memory_context: &'a str,
    pub skills_context: &'a str,
    pub session_context: &'a str,
}

#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub requires_approval: bool,
}

pub trait AgentBackend {
    fn render_prompt(&self, request: &BackendTurnRequest<'_>) -> String;
    async fn execute_turn(
        &self,
        repo_root: &Path,
        request: &BackendTurnRequest<'_>,
        output_path: &Path,
        log_path: &Path,
    ) -> Result<TurnEnvelope>;
}

#[derive(Debug, Default)]
pub struct CodexBackend;

impl AgentBackend for CodexBackend {
    fn render_prompt(&self, request: &BackendTurnRequest<'_>) -> String {
        render_lead_turn_prompt(request)
    }

    async fn execute_turn(
        &self,
        repo_root: &Path,
        request: &BackendTurnRequest<'_>,
        output_path: &Path,
        log_path: &Path,
    ) -> Result<TurnEnvelope> {
        ensure_codex_available()?;
        let prompt = self.render_prompt(request);
        let raw = run_text_once(
            &prompt,
            repo_root,
            request.model,
            request.thinking_mode,
            request.timeout_secs,
            output_path,
            log_path,
            1,
        )
        .await?;
        parse_turn_envelope(&raw)
    }
}

pub fn built_in_tools() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: "list_tree",
            description: "列出仓库中的目录和文件概览",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "read_file",
            description: "读取单个文件内容",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "search_files",
            description: "按关键字搜索文件内容",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "apply_patch",
            description: "按 search/replace 方式对单个文件做精确补丁",
            requires_approval: true,
        },
        ToolDescriptor {
            name: "run_shell",
            description: "在 Docker 沙箱中执行 shell 命令",
            requires_approval: true,
        },
        ToolDescriptor {
            name: "write_file",
            description: "在 Docker 工作区中写入文件",
            requires_approval: true,
        },
        ToolDescriptor {
            name: "list_artifacts",
            description: "列出当前 thread 或 run 的 artifacts",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "read_artifact",
            description: "读取某个 artifact 的内容与元数据",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "inspect_run",
            description: "查看当前 run 的状态、节点和阻塞信息",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "create_plan_snapshot",
            description: "把当前 task graph 与节点摘要写成 plan artifact",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "read_contract",
            description: "读取当前 thread 的 execution contract",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "write_contract",
            description: "写入当前 thread 的 execution contract",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "read_progress",
            description: "读取当前 thread 的 progress ledger",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "update_progress",
            description: "更新当前 thread 的 progress ledger",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "record_evaluation",
            description: "记录当前 feature 的 evaluator 结论",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "create_session_bootstrap",
            description: "生成供后续 run 接手的 bootstrap 文本",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "read_memory",
            description: "读取线程的 working/project memory",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "remember_memory",
            description: "把重要事实写入 working 或 project memory",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "list_skills",
            description: "列出本地可用 skills",
            requires_approval: false,
        },
        ToolDescriptor {
            name: "read_skill",
            description: "读取某个 skill 的 SKILL.md 内容",
            requires_approval: false,
        },
    ]
}
