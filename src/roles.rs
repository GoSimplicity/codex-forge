use crate::model::{ExecutionNode, HandoffArtifact, RepoSnapshot, RoleConfig, SessionConfig};

pub fn find_role(roles: &[RoleConfig], name: &str) -> Option<RoleConfig> {
    roles.iter().find(|role| role.key == name).cloned()
}

pub fn render_worker_prompt(
    role: &RoleConfig,
    node: &ExecutionNode,
    config: &SessionConfig,
    repo: &RepoSnapshot,
    upstream_handoffs: &[HandoffArtifact],
) -> String {
    let deliverables = render_bullets(&node.deliverables, "无");
    let dependencies = render_bullets(&node.dependencies, "无显式依赖");
    let input_artifacts = render_bullets(&node.input_artifacts, "无");
    let output_artifacts = render_bullets(&node.output_artifacts, "无");
    let completion = render_bullets(&node.completion_criteria, "无");
    let handoffs = if upstream_handoffs.is_empty() {
        "无上游 handoff，可直接推进。".to_string()
    } else {
        upstream_handoffs
            .iter()
            .map(|artifact| {
                format!(
                    "## {} / {}\n- 摘要：{}\n- 变更意图：{}\n- 触达文件：{}\n- 风险：{}\n- 建议：{}",
                    artifact.agent_id,
                    artifact.role,
                    artifact.summary,
                    compact(&artifact.change_intent, 160),
                    if artifact.touched_files.is_empty() {
                        "无".to_string()
                    } else {
                        artifact.touched_files.join("、")
                    },
                    if artifact.risks.is_empty() {
                        "无".to_string()
                    } else {
                        artifact.risks.join("；")
                    },
                    if artifact.downstream_suggestions.is_empty() {
                        "无".to_string()
                    } else {
                        artifact.downstream_suggestions.join("；")
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    let stacks = if repo.detected_stacks.is_empty() {
        "未识别".to_string()
    } else {
        repo.detected_stacks.join(" / ")
    };

    let edit_policy = if role.can_edit && node.allow_code_changes {
        "允许改代码，但只限当前节点直接相关范围。"
    } else {
        "本节点默认只读分析，不要提交代码改动。"
    };

    let prompt_preamble = role
        .prompt_preamble
        .clone()
        .unwrap_or_else(|| "你在 codex-forge V3 中承担一个显式依赖节点。".to_string());

    let global_rules = config.global_rule_prompt.trim();
    let reviewer_rules = if role.key == "reviewer" {
        config.reviewer_rule_prompt.as_deref().unwrap_or("").trim()
    } else {
        ""
    };
    let thinking_mode = config.thinking_mode;
    let continuation_block = config
        .continuation
        .as_ref()
        .map(|item| {
            let latest_feedback = item
                .feedback_history
                .last()
                .map(|record| record.intent_summary.clone())
                .unwrap_or_else(|| item.feedback.clone());
            format!(
                "延续迭代上下文：\n\
- 当前轮次：V{}\n\
- 来源会话：{}\n\
- 延续类型：{}\n\
- 本轮反馈：{}\n\n",
                item.iteration_index,
                item.parent_session_id,
                item.kind.label(),
                latest_feedback
            )
        })
        .unwrap_or_default();
    format!(
        "{prompt_preamble}\n\
角色：{}（{}）\n\
职责：{}\n\
可用 Codex Skills：{}\n\
工作风格：{}\n\
思考强度：{}（{}）\n\
编辑权限：{}\n\n\
全局规则：\n{}\n\n\
角色专项规则：\n{}\n\n\
思考模式要求：\n{}\n\n\
全局任务：{}\n\
{}\
当前节点：{}\n\
节点目标：{}\n\
聚焦点：{}\n\n\
输入工件：\n{}\n\n\
期望输出：\n{}\n\n\
完成条件：\n{}\n\n\
交付物：\n{}\n\n\
依赖节点：\n{}\n\n\
上游 handoff：\n{}\n\n\
仓库上下文：\n\
- 仓库：{}\n\
- 技术栈：{}\n\
- 顶层目录：{}\n\n\
执行约束：\n\
- 你工作在独立 worktree 内，与其他 worker 并行协作。\n\
- 必须显式吸收上游 handoff，再决定是否继续实现。\n\
- 不要假设未声明依赖的节点已经完成。\n\
- 如果遇到阻塞或不确定性，写进风险与交接。\n\
- 在 `# 交接` 中补充你认为自己实际被授权修改的路径范围、额外越界说明和后续接手建议。\n\
- reviewer 节点重点挑战集成风险；tester 节点重点验证最小可信路径。\n\n\
最终请严格使用以下 Markdown 小节输出：\n\
# 交付摘要\n\
# 变更清单\n\
# 风险\n\
# 验证\n\
# 交接\n\n\
输出语言默认使用中文。若涉及公共协议、命令名或代码标识，可保留英文。",
        role.title,
        role.key,
        role.mission,
        if role.skills.is_empty() {
            "无".to_string()
        } else {
            role.skills.join("、")
        },
        role.working_style,
        thinking_mode.title(),
        thinking_mode.label(),
        edit_policy,
        if global_rules.is_empty() {
            "无".to_string()
        } else {
            global_rules.to_string()
        },
        if reviewer_rules.is_empty() {
            "无".to_string()
        } else {
            reviewer_rules.to_string()
        },
        thinking_mode_instructions(thinking_mode),
        config.task,
        continuation_block,
        node.title,
        node.objective,
        node.prompt_focus,
        input_artifacts,
        output_artifacts,
        completion,
        deliverables,
        dependencies,
        handoffs,
        repo.display_name,
        stacks,
        repo.top_level_entries.join("、"),
    )
}

fn thinking_mode_instructions(mode: crate::model::ThinkingMode) -> &'static str {
    match mode {
        crate::model::ThinkingMode::Quick => {
            "- 优先快速定位最小可行方案。\n- 减少铺陈，尽快给出可执行结论。\n- 只有在明显阻塞时才扩展分析。"
        }
        crate::model::ThinkingMode::Balanced => {
            "- 平衡速度、质量和验证证据。\n- 先做最小充分分析，再推进实现。\n- 对风险给出简洁但明确的说明。"
        }
        crate::model::ThinkingMode::HardThink => {
            "- 先充分拆解边界、依赖和失败模式，再做实现判断。\n- 主动审视潜在回归、契约漂移和验证缺口。\n- 输出更强调推理链路、风险判断和收敛策略。"
        }
    }
}

fn render_bullets(items: &[String], empty_text: &str) -> String {
    if items.is_empty() {
        format!("- {empty_text}")
    } else {
        items
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn compact(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let preview = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}
