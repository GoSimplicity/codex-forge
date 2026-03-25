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
    pub tools: &'a [ToolDescriptor],
    pub system_hint: &'a str,
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
            name: "run_shell",
            description: "在 Docker 沙箱中执行 shell 命令",
            requires_approval: true,
        },
        ToolDescriptor {
            name: "write_file",
            description: "在 Docker 工作区中写入文件",
            requires_approval: true,
        },
    ]
}
