use std::collections::HashMap;

use crate::config::RoleOverride;
use crate::model::{ExecutionNode, HandoffArtifact, RepoSnapshot, RoleConfig, SessionConfig};

pub fn resolve_role_set(name: &str, overrides: &HashMap<String, RoleOverride>) -> Vec<RoleConfig> {
    let base = match name {
        "core" => core_role_set(),
        _ => core_role_set(),
    };

    base.into_iter()
        .map(|role| {
            let key = role.key.clone();
            apply_override(role, overrides.get(&key))
        })
        .collect()
}

pub fn find_role(roles: &[RoleConfig], name: &str) -> Option<RoleConfig> {
    roles.iter().find(|role| role.key == name).cloned()
}

pub fn agents_overview() -> Vec<RoleConfig> {
    core_role_set()
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
        .unwrap_or_else(|| "你在 codex-forge V2 中承担一个显式依赖节点。".to_string());
    let reviewer_gate_rule = if role.key == "reviewer" {
        "\n- 你是 apply 前的最终 gatekeeper。\n- 必须在 `# 交接` 小节首行输出且只能输出一条决策：`- APPLY_DECISION: allow_full`、`- APPLY_DECISION: allow_partial` 或 `- APPLY_DECISION: block`。\n- 若只建议接收部分文件，必须写 `allow_partial`，并在交接中明确哪些文件需要人工复核。\n- 只有在你确认当前结果适合整体自动应用时才写 `allow_full`；否则根据风险写 `allow_partial` 或 `block`。"
    } else {
        "\n- 非 reviewer 节点不要输出 `APPLY_DECISION`。"
    };

    format!(
        "{prompt_preamble}\n\
角色：{}（{}）\n\
职责：{}\n\
擅长：{}\n\
工作风格：{}\n\
编辑权限：{}\n\n\
全局任务：{}\n\
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
- reviewer 节点重点挑战集成风险；tester 节点重点验证最小可信路径。{}\n\n\
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
        role.skills.join("、"),
        role.working_style,
        edit_policy,
        config.task,
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
        reviewer_gate_rule,
    )
}

fn core_role_set() -> Vec<RoleConfig> {
    vec![
        RoleConfig {
            key: "architect".to_string(),
            title: "架构师".to_string(),
            mission: "负责拆解需求、识别模块边界、定义接口与依赖关系。".to_string(),
            skills: vec![
                "系统设计".to_string(),
                "依赖分析".to_string(),
                "风险识别".to_string(),
            ],
            working_style: "先明确图结构、handoff 契约和落地顺序，再给实现建议。".to_string(),
            can_edit: false,
            max_concurrency: Some(1),
            dependency_policy: Some("fan_out".to_string()),
            prompt_preamble: None,
        },
        RoleConfig {
            key: "implementer".to_string(),
            title: "实现者".to_string(),
            mission: "负责把节点目标实现成最小充分代码，并保证与依赖契约一致。".to_string(),
            skills: vec![
                "编码实现".to_string(),
                "工程落地".to_string(),
                "集成配合".to_string(),
            ],
            working_style: "直接改代码，优先修根因，不做与目标无关的重构。".to_string(),
            can_edit: true,
            max_concurrency: None,
            dependency_policy: Some("ready_only".to_string()),
            prompt_preamble: None,
        },
        RoleConfig {
            key: "tester".to_string(),
            title: "测试员".to_string(),
            mission: "负责补齐验证路径、构造失败样例并增强可验证性。".to_string(),
            skills: vec![
                "测试设计".to_string(),
                "失败复现".to_string(),
                "验证报告".to_string(),
            ],
            working_style: "优先最小可信验证；必要时补测试或验证性改动。".to_string(),
            can_edit: true,
            max_concurrency: Some(1),
            dependency_policy: Some("after_impl".to_string()),
            prompt_preamble: None,
        },
        RoleConfig {
            key: "reviewer".to_string(),
            title: "审阅者".to_string(),
            mission: "负责在应用前做集成审阅，优先发现冲突、遗漏与回归风险。".to_string(),
            skills: vec![
                "代码审阅".to_string(),
                "冲突判断".to_string(),
                "集成收敛".to_string(),
            ],
            working_style: "默认只读，先找问题和证据，再给精确修正建议。".to_string(),
            can_edit: false,
            max_concurrency: Some(1),
            dependency_policy: Some("gate_before_apply".to_string()),
            prompt_preamble: None,
        },
    ]
}

fn apply_override(mut role: RoleConfig, override_item: Option<&RoleOverride>) -> RoleConfig {
    if let Some(override_item) = override_item {
        if let Some(title) = &override_item.title {
            role.title = title.clone();
        }
        if let Some(mission) = &override_item.mission {
            role.mission = mission.clone();
        }
        if let Some(skills) = &override_item.skills {
            role.skills = skills.clone();
        }
        if let Some(working_style) = &override_item.working_style {
            role.working_style = working_style.clone();
        }
        if let Some(can_edit) = override_item.can_edit {
            role.can_edit = can_edit;
        }
        if let Some(max_concurrency) = override_item.max_concurrency {
            role.max_concurrency = Some(max_concurrency);
        }
        if let Some(policy) = &override_item.dependency_policy {
            role.dependency_policy = Some(policy.clone());
        }
        if let Some(prompt_preamble) = &override_item.prompt_preamble {
            role.prompt_preamble = Some(prompt_preamble.clone());
        }
    }
    role
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
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(limit).collect::<String>())
    }
}
