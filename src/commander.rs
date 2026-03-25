use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::codex::run_json_once;
use crate::model::{
    ApplyDecision, ApplyStatus, DriftPolicy, ExecutionContract, ExecutionGraph, ExecutionNode,
    FinalSummary, NodeContract, PlanTodo, PlanTodoItem, RepoSnapshot, ResultStatus, RoleConfig,
    SchedulerHint, ScopeDrift, SessionConfig, SessionManifest, TrustLevel,
};

const PLAN_TODO_SCHEMA: &str = r#"{
  "type": "object",
  "required": ["summary", "approach", "todos", "risks"],
  "properties": {
    "summary": {"type": "string"},
    "approach": {"type": "string"},
    "risks": {"type": "array", "items": {"type": "string"}},
    "todos": {
      "type": "array",
      "minItems": 1,
      "items": {
        "type": "object",
        "required": [
          "title",
          "goal",
          "details",
          "dependencies",
          "completion_criteria"
        ],
        "properties": {
          "title": {"type": "string"},
          "goal": {"type": "string"},
          "details": {"type": "array", "items": {"type": "string"}},
          "dependencies": {"type": "array", "items": {"type": "string"}},
          "completion_criteria": {"type": "array", "items": {"type": "string"}}
        }
      }
    }
  }
}"#;

const PLAN_SCHEMA: &str = r#"{
  "type": "object",
  "required": ["summary", "strategy", "nodes"],
  "properties": {
    "summary": {"type": "string"},
    "strategy": {"type": "string"},
    "nodes": {
      "type": "array",
      "minItems": 1,
      "items": {
        "type": "object",
        "required": [
          "title",
          "role",
          "objective",
          "deliverables",
          "dependencies",
          "prompt_focus",
          "input_artifacts",
          "output_artifacts",
          "completion_criteria"
        ],
        "properties": {
          "title": {"type": "string"},
          "todo_ref": {"type": "string"},
          "role": {"type": "string"},
          "objective": {"type": "string"},
          "deliverables": {"type": "array", "items": {"type": "string"}},
          "dependencies": {"type": "array", "items": {"type": "string"}},
          "prompt_focus": {"type": "string"},
          "input_artifacts": {"type": "array", "items": {"type": "string"}},
          "output_artifacts": {"type": "array", "items": {"type": "string"}},
          "completion_criteria": {"type": "array", "items": {"type": "string"}}
        }
      }
    }
  }
}"#;

#[derive(Debug, Deserialize)]
struct PlannerOutput {
    summary: String,
    strategy: String,
    nodes: Vec<PlannerNode>,
}

#[derive(Debug, Deserialize)]
struct PlanTodoOutput {
    summary: String,
    approach: String,
    todos: Vec<PlanTodoOutputItem>,
    risks: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PlanTodoOutputItem {
    title: String,
    goal: String,
    details: Vec<String>,
    dependencies: Vec<String>,
    completion_criteria: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PlannerNode {
    title: String,
    #[serde(default)]
    todo_ref: Option<String>,
    role: String,
    objective: String,
    deliverables: Vec<String>,
    dependencies: Vec<String>,
    prompt_focus: String,
    input_artifacts: Vec<String>,
    output_artifacts: Vec<String>,
    completion_criteria: Vec<String>,
}

pub async fn build_plan(
    config: &SessionConfig,
    repo: &RepoSnapshot,
    roles: &[RoleConfig],
    commander_dir: &Path,
    plan_todo: Option<&PlanTodo>,
    memory_prompt: Option<&str>,
) -> Result<ExecutionGraph> {
    fs::create_dir_all(commander_dir)
        .with_context(|| format!("创建 commander 目录失败：{}", commander_dir.display()))?;

    let schema_path = commander_dir.join("plan-schema.json");
    let prompt_path = commander_dir.join("planner-prompt.md");
    let output_path = commander_dir.join("planner-output.json");
    let log_path = commander_dir.join("planner.log");
    fs::write(&schema_path, PLAN_SCHEMA)
        .with_context(|| format!("写入计划 schema 失败：{}", schema_path.display()))?;

    let prompt = build_planner_prompt(config, repo, roles, plan_todo, memory_prompt);
    fs::write(&prompt_path, &prompt)
        .with_context(|| format!("写入 planner prompt 失败：{}", prompt_path.display()))?;

    let mut notes = Vec::new();
    let json_prompt = bind_json_schema(&prompt, PLAN_SCHEMA);
    match run_json_once(
        &json_prompt,
        &repo.repo_root,
        config.model.as_deref(),
        config.thinking_mode,
        &output_path,
        &log_path,
        config.max_retries,
    )
    .await
    {
        Ok(raw) => match serde_json::from_str::<PlannerOutput>(&raw) {
            Ok(output) => {
                match normalize_plan(output, roles, config.workers, plan_todo, notes.clone()) {
                    Ok(graph) => Ok(graph),
                    Err(error) => {
                        notes.push(format!(
                            "Codex planner 结果不可用，改走内置回退：{}",
                            summarize_error(&error.to_string())
                        ));
                        fallback_plan(config, roles, notes)
                    }
                }
            }
            Err(error) => {
                notes.push(format!(
                    "Codex planner JSON 解析失败，改走内置回退：{}",
                    summarize_error(&error.to_string())
                ));
                fallback_plan(config, roles, notes)
            }
        },
        Err(error) => {
            notes.push(format!(
                "Codex planner 调用失败，改走内置回退：{}",
                summarize_error(&error.to_string())
            ));
            fallback_plan(config, roles, notes)
        }
    }
}

pub async fn build_plan_todo(
    config: &SessionConfig,
    repo: &RepoSnapshot,
    commander_dir: &Path,
    memory_prompt: Option<&str>,
) -> Result<PlanTodo> {
    fs::create_dir_all(commander_dir)
        .with_context(|| format!("创建 commander 目录失败：{}", commander_dir.display()))?;

    let schema_path = commander_dir.join("plan-todo-schema.json");
    let prompt_path = commander_dir.join("plan-todo-prompt.md");
    let output_path = commander_dir.join("plan-todo-output.json");
    let log_path = commander_dir.join("plan-todo.log");
    fs::write(&schema_path, PLAN_TODO_SCHEMA)
        .with_context(|| format!("写入 todo schema 失败：{}", schema_path.display()))?;

    let prompt = build_plan_todo_prompt(config, repo, memory_prompt);
    fs::write(&prompt_path, &prompt)
        .with_context(|| format!("写入 todo prompt 失败：{}", prompt_path.display()))?;

    let mut notes = Vec::new();
    let json_prompt = bind_json_schema(&prompt, PLAN_TODO_SCHEMA);
    match run_json_once(
        &json_prompt,
        &repo.repo_root,
        config.model.as_deref(),
        config.thinking_mode,
        &output_path,
        &log_path,
        config.max_retries,
    )
    .await
    {
        Ok(raw) => match serde_json::from_str::<PlanTodoOutput>(&raw) {
            Ok(output) => match normalize_plan_todo(output) {
                Ok(plan_todo) => Ok(enrich_plan_todo(config, plan_todo)),
                Err(error) => {
                    notes.push(format!(
                        "Codex todo 计划结果不可用，改走内置回退：{}",
                        summarize_error(&error.to_string())
                    ));
                    Ok(fallback_plan_todo(config, notes))
                }
            },
            Err(error) => {
                notes.push(format!(
                    "Codex todo 计划 JSON 解析失败，改走内置回退：{}",
                    summarize_error(&error.to_string())
                ));
                Ok(fallback_plan_todo(config, notes))
            }
        },
        Err(error) => {
            notes.push(format!(
                "Codex todo 计划调用失败，改走内置回退：{}",
                summarize_error(&error.to_string())
            ));
            Ok(fallback_plan_todo(config, notes))
        }
    }
}

pub fn derive_execution_contract(
    config: &SessionConfig,
    graph: &ExecutionGraph,
) -> ExecutionContract {
    let review_allowed_paths = config
        .continuation
        .as_ref()
        .and_then(|item| item.review_fix.as_ref())
        .map(|item| vec![item.target_file.clone()]);
    let forbidden_paths = vec![
        ".git/**".to_string(),
        ".codex-forge/**".to_string(),
        "target/**".to_string(),
    ];
    let node_contracts = graph
        .nodes
        .iter()
        .map(|node| NodeContract {
            node_id: node.id.clone(),
            allowed_paths: if node.allow_code_changes {
                review_allowed_paths
                    .clone()
                    .unwrap_or_else(|| vec!["*".to_string()])
            } else {
                Vec::new()
            },
            forbidden_paths: forbidden_paths.clone(),
            expected_artifacts: if node.expected_artifacts.is_empty() {
                node.output_artifacts.clone()
            } else {
                node.expected_artifacts.clone()
            },
            required_verifications: if node.required_verifications.is_empty() {
                node.completion_criteria.clone()
            } else {
                node.required_verifications.clone()
            },
            acceptable_drift: node.acceptable_drift,
        })
        .collect::<Vec<_>>();

    ExecutionContract {
        task_fingerprint: task_fingerprint(config, graph),
        allowed_paths: review_allowed_paths.unwrap_or_else(|| vec!["*".to_string()]),
        forbidden_paths,
        node_contracts,
        drift_policy: DriftPolicy::default(),
        summary_contract: vec![
            "result_status".to_string(),
            "review_gate".to_string(),
            "apply_status".to_string(),
            "accepted_files".to_string(),
            "manual_review_files".to_string(),
            "rejected_files".to_string(),
            "verified_capabilities".to_string(),
            "blocked_verifications".to_string(),
            "recommended_next_action".to_string(),
        ],
        compatibility_notes: if let Some(review_fix) = config
            .continuation
            .as_ref()
            .and_then(|item| item.review_fix.as_ref())
        {
            vec![format!(
                "人工审查返修模式：仅允许修改 `{}`。",
                review_fix.target_file
            )]
        } else {
            vec!["V3 默认契约使用保守禁止列表和宽松允许列表。".to_string()]
        },
    }
}

pub async fn summarize_run(
    _config: &SessionConfig,
    manifest: &SessionManifest,
    _roles: &[RoleConfig],
    commander_dir: &Path,
) -> Result<FinalSummary> {
    fs::create_dir_all(commander_dir)
        .with_context(|| format!("创建 commander 目录失败：{}", commander_dir.display()))?;

    Ok(build_local_summary(manifest))
}

fn build_plan_todo_prompt(
    config: &SessionConfig,
    repo: &RepoSnapshot,
    memory_prompt: Option<&str>,
) -> String {
    let readme = repo
        .readme_excerpt
        .clone()
        .unwrap_or_else(|| "无 README 摘要".to_string());
    let continuation_block = render_continuation_prompt_block(config);

    format!(
        "你现在是 codex-forge V6 的 iterative planner，需要先输出一份**面向用户可读**的计划 TODO 清单。\n\
请只输出符合 schema 的 JSON，不要输出 Markdown，也不要输出执行角色。\n\n\
全局任务：{}\n\
思考强度：{}（{}）\n\
目标仓库：{}\n\
技术栈：{}\n\n\
仓库摘要：\n\
- 顶层目录：{}\n\
- README 摘要：\n{}\n\n\
{}\
{}\
规划要求：\n\
- todo 必须直接对应用户要推进的工作，而不是角色名。\n\
- 优先给出 3 到 6 个可执行步骤，顺序清晰，避免空话。\n\
- 每个 todo 需要写清目标、细节、依赖和完成标准。\n\
- 风险要简洁，优先列真正会阻塞执行的点。\n\
- 如果这是延续迭代，要优先吸收上一轮反馈，输出更适合当前轮次的最小调整方案。\n\
- 保持最小可执行规划，不要过度设计。\n",
        config.task,
        config.thinking_mode.title(),
        config.thinking_mode.label(),
        repo.display_name,
        if repo.detected_stacks.is_empty() {
            "未知".to_string()
        } else {
            repo.detected_stacks.join(" / ")
        },
        repo.top_level_entries.join("、"),
        readme,
        memory_prompt.unwrap_or(""),
        continuation_block,
    )
}

fn build_planner_prompt(
    config: &SessionConfig,
    repo: &RepoSnapshot,
    roles: &[RoleConfig],
    plan_todo: Option<&PlanTodo>,
    memory_prompt: Option<&str>,
) -> String {
    let roles_text = roles
        .iter()
        .map(|role| {
            format!(
                "- {}（{}）：{}；擅长：{}；风格：{}；可编辑：{}",
                role.title,
                role.key,
                role.mission,
                role.skills.join("、"),
                role.working_style,
                role.can_edit
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let readme = repo
        .readme_excerpt
        .clone()
        .unwrap_or_else(|| "无 README 摘要".to_string());
    let todo_context = plan_todo
        .map(|plan| {
            let todos = plan
                .todos
                .iter()
                .map(|item| {
                    format!(
                        "- {}：{}；完成标准：{}",
                        item.title,
                        item.goal,
                        item.completion_criteria.join("；")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "\n用户规划清单：\n- 摘要：{}\n- 策略：{}\n- Todo：\n{}\n",
                plan.summary, plan.approach, todos
            )
        })
        .unwrap_or_default();

    let continuation_block = render_continuation_prompt_block(config);
    format!(
        "你现在是 codex-forge V6 的 commander agent，需要为多个 Codex worker 规划显式执行图。\n\
请只输出符合 schema 的 JSON，不要输出 Markdown。\n\n\
全局任务：{}\n\
思考强度：{}（{}）\n\
思考模式要求：\n{}\n\
目标仓库：{}\n\
技术栈：{}\n\
期望 worker 数量上限：{}\n\
角色集合：{}\n\
默认应用模式：{}\n\n\
可用角色：\n{}\n\n\
仓库摘要：\n\
- 顶层目录：{}\n\
- README 摘要：\n{}\n\n\
{}\
{}\
{}\
规划要求：\n\
- 输出 1 到 {} 个节点。\n\
- 每个节点必须绑定一个可用角色。\n\
- 如果能明确映射到用户 todo，请写 `todo_ref`，优先使用 `todo-1` 这类 id；拿不准可以省略。\n\
- 节点依赖必须显式列出，用节点 id 归一化前的逻辑前驱来表达。\n\
- 默认要形成“分析/实现 → 审阅 gate → apply”的最小收敛闭环。\n\
- 若可用角色中包含 reviewer，应把 reviewer 节点放在准备应用之前，默认只读。\n\
- input_artifacts / output_artifacts / completion_criteria 必须具体，便于下游 handoff。\n\
- 优先保证可执行性和自动收敛，不要设计过度。\n",
        config.task,
        config.thinking_mode.title(),
        config.thinking_mode.label(),
        thinking_mode_prompt_guidance(config.thinking_mode),
        repo.display_name,
        if repo.detected_stacks.is_empty() {
            "未知".to_string()
        } else {
            repo.detected_stacks.join(" / ")
        },
        config.workers,
        config.role_set,
        config.apply_mode,
        roles_text,
        repo.top_level_entries.join("、"),
        readme,
        memory_prompt.unwrap_or(""),
        continuation_block,
        todo_context,
        config.workers,
    )
}

fn render_continuation_prompt_block(config: &SessionConfig) -> String {
    let Some(continuation) = &config.continuation else {
        return String::new();
    };
    let latest_feedback = continuation.latest_feedback_summary();
    let previous_summary = continuation
        .parent_summary_overview
        .clone()
        .or_else(|| continuation.parent_plan_summary.clone())
        .unwrap_or_else(|| "无".to_string());
    let next_actions = if continuation.parent_recommended_next_action.is_empty() {
        "无".to_string()
    } else {
        continuation.parent_recommended_next_action.join("；")
    };
    format!(
        "延续迭代上下文：\n\
- 当前轮次：V{}\n\
- 来源会话：{}\n\
- 延续类型：{}\n\
- 上一轮摘要：{}\n\
- 人类最新反馈：{}\n\
- 上一轮建议下一步：{}\n\n",
        continuation.iteration_index,
        continuation.parent_session_id,
        continuation.kind.label(),
        previous_summary,
        latest_feedback,
        next_actions
    )
}

fn thinking_mode_prompt_guidance(mode: crate::model::ThinkingMode) -> &'static str {
    match mode {
        crate::model::ThinkingMode::Quick => {
            "- 用最短路径完成拆解。\n- 降低节点数量和分析成本，优先可执行性。\n- 非关键风险不必展开成长篇说明。"
        }
        crate::model::ThinkingMode::Balanced => {
            "- 兼顾拆解质量和执行效率。\n- 对关键依赖、验证和风险做适度显式化。\n- 不过度拆分节点，也不省略必要 gate。"
        }
        crate::model::ThinkingMode::HardThink => {
            "- 更深入分析边界、依赖、失败模式和验证路径。\n- 对 reviewer gate、apply 风险和 handoff 要更具体。\n- 宁可更细致，也不要为了省节点而牺牲收敛安全。"
        }
    }
}

fn bind_json_schema(prompt: &str, schema: &str) -> String {
    format!(
        "{prompt}\n\
输出规则（必须严格遵守）：\n\
- 最终输出必须是一个合法 JSON 对象。\n\
- 不要输出 Markdown、代码块、解释文字、前后缀。\n\
- 字段名、层级和类型必须满足下面的 JSON Schema。\n\
- 如果你想解释，请把解释写进 JSON 字段，而不是写在 JSON 外。\n\n\
JSON Schema：\n{schema}\n"
    )
}

fn normalize_plan(
    output: PlannerOutput,
    roles: &[RoleConfig],
    max_workers: usize,
    plan_todo: Option<&PlanTodo>,
    mut notes: Vec<String>,
) -> Result<ExecutionGraph> {
    let available_roles = roles
        .iter()
        .map(|role| role.key.as_str())
        .collect::<HashSet<_>>();
    let role_map = roles
        .iter()
        .map(|role| (role.key.as_str(), role))
        .collect::<HashMap<_, _>>();
    let mut role_counts: HashMap<String, usize> = HashMap::new();

    let todo_alias_map = plan_todo.map(build_plan_todo_alias_map).unwrap_or_default();
    let mut nodes = output
        .nodes
        .into_iter()
        .take(max_workers.max(1))
        .map(|node| {
            let deliverables = node.deliverables.clone();
            let role_key = if available_roles.contains(node.role.as_str()) {
                node.role
            } else {
                notes.push(format!(
                    "planner 给出了未知角色 `{}`，已回退到 implementer",
                    node.role
                ));
                "implementer".to_string()
            };
            let index = role_counts
                .entry(role_key.clone())
                .and_modify(|item| *item += 1)
                .or_insert(1);
            let role = role_map.get(role_key.as_str()).context("缺少角色定义")?;
            let dependencies = node.dependencies.clone();
            Ok(ExecutionNode {
                id: format!("{role_key}-{index}"),
                title: node.title,
                todo_id: node
                    .todo_ref
                    .as_deref()
                    .and_then(|todo_ref| resolve_explicit_todo_ref(todo_ref, &todo_alias_map)),
                role: role_key,
                objective: node.objective,
                deliverables: deliverables.clone(),
                dependencies: dependencies.clone(),
                prompt_focus: node.prompt_focus,
                input_artifacts: node.input_artifacts,
                output_artifacts: node.output_artifacts,
                completion_criteria: node.completion_criteria,
                allow_code_changes: role.can_edit,
                expected_artifacts: deliverables,
                required_verifications: Vec::new(),
                scope_guard_ref: None,
                scheduler_hint: infer_scheduler_hint(role, &dependencies),
                acceptable_drift: if role.can_edit {
                    ScopeDrift::Minor
                } else {
                    ScopeDrift::None
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;

    ensure_reviewer_gate(&mut nodes, roles);
    normalize_dependencies(&mut nodes)?;
    let mut graph = ExecutionGraph {
        summary: output.summary,
        strategy: output.strategy,
        nodes,
        used_fallback: false,
        planning_notes: notes,
    };
    let ordered = graph.topological_order()?;
    if let Some(plan_todo) = plan_todo {
        let mut graph_notes = graph.planning_notes.clone();
        assign_todo_ids(&mut graph.nodes, &ordered, plan_todo, &mut graph_notes);
        graph.planning_notes = graph_notes;
    }
    Ok(graph)
}

fn build_plan_todo_alias_map(plan_todo: &PlanTodo) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for item in &plan_todo.todos {
        map.insert(item.id.to_lowercase(), item.id.clone());
        map.insert(item.title.to_lowercase(), item.id.clone());
        map.insert(slugify(&item.title), item.id.clone());
    }
    map
}

fn resolve_explicit_todo_ref(
    todo_ref: &str,
    alias_map: &HashMap<String, String>,
) -> Option<String> {
    let key = todo_ref.trim().to_lowercase();
    alias_map
        .get(&key)
        .cloned()
        .or_else(|| alias_map.get(&slugify(&key)).cloned())
}

fn assign_todo_ids(
    nodes: &mut [ExecutionNode],
    ordered_ids: &[String],
    plan_todo: &PlanTodo,
    notes: &mut Vec<String>,
) {
    let todo_ids = plan_todo
        .todos
        .iter()
        .map(|item| item.id.clone())
        .collect::<Vec<_>>();
    let node_index = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.clone(), index))
        .collect::<HashMap<_, _>>();

    for node_id in ordered_ids {
        let Some(index) = node_index.get(node_id).copied() else {
            continue;
        };
        if nodes[index].todo_id.is_none()
            && let Some(inherited) =
                infer_todo_from_dependencies(nodes, &node_index, &nodes[index].dependencies)
        {
            nodes[index].todo_id = Some(inherited);
            continue;
        }

        if nodes[index].todo_id.is_none()
            && let Some(best) = best_matching_todo_id(&nodes[index], plan_todo)
        {
            nodes[index].todo_id = Some(best);
        }
    }

    let mut covered_code_todos = nodes
        .iter()
        .filter(|node| node.allow_code_changes)
        .filter_map(|node| node.todo_id.clone())
        .collect::<HashSet<_>>();
    let fallback_todo = todo_ids.last().cloned();
    for node_id in ordered_ids {
        let Some(index) = node_index.get(node_id).copied() else {
            continue;
        };
        if !nodes[index].allow_code_changes || nodes[index].todo_id.is_some() {
            continue;
        }
        let next = todo_ids
            .iter()
            .find(|todo_id| !covered_code_todos.contains(*todo_id))
            .cloned()
            .or_else(|| fallback_todo.clone());
        if let Some(todo_id) = next {
            covered_code_todos.insert(todo_id.clone());
            nodes[index].todo_id = Some(todo_id);
        }
    }

    for node_id in ordered_ids {
        let Some(index) = node_index.get(node_id).copied() else {
            continue;
        };
        if nodes[index].todo_id.is_none()
            && let Some(inherited) =
                infer_todo_from_dependencies(nodes, &node_index, &nodes[index].dependencies)
        {
            nodes[index].todo_id = Some(inherited);
        }
    }

    let unmatched = todo_ids
        .into_iter()
        .filter(|todo_id| {
            !nodes
                .iter()
                .any(|node| node.todo_id.as_deref() == Some(todo_id.as_str()))
        })
        .collect::<Vec<_>>();
    if !unmatched.is_empty() {
        notes.push(format!(
            "以下 todo 没有映射到执行节点，将在运行时按无代码步骤处理：{}",
            unmatched.join("、")
        ));
    }
}

fn infer_todo_from_dependencies(
    nodes: &[ExecutionNode],
    node_index: &HashMap<String, usize>,
    dependencies: &[String],
) -> Option<String> {
    let mut inherited = dependencies
        .iter()
        .filter_map(|dep| node_index.get(dep))
        .filter_map(|index| nodes[*index].todo_id.clone())
        .collect::<Vec<_>>();
    inherited.dedup();
    if inherited.len() == 1 {
        inherited.into_iter().next()
    } else {
        None
    }
}

fn best_matching_todo_id(node: &ExecutionNode, plan_todo: &PlanTodo) -> Option<String> {
    let mut best = None::<(usize, String)>;
    for todo in &plan_todo.todos {
        let score = score_todo_match(node, todo);
        if score == 0 {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, todo.id.clone()));
        }
    }
    best.map(|(_, todo_id)| todo_id)
}

fn score_todo_match(node: &ExecutionNode, todo: &PlanTodoItem) -> usize {
    let node_text = format!(
        "{} {} {} {}",
        node.title,
        node.objective,
        node.prompt_focus,
        node.completion_criteria.join(" ")
    )
    .to_lowercase();
    let todo_text = format!(
        "{} {} {} {}",
        todo.title,
        todo.goal,
        todo.details.join(" "),
        todo.completion_criteria.join(" ")
    )
    .to_lowercase();

    let mut score = 0;
    if node_text.contains(&todo.title.to_lowercase())
        || todo_text.contains(&node.title.to_lowercase())
    {
        score += 8;
    }
    if node_text.contains(&todo.goal.to_lowercase())
        || todo_text.contains(&node.objective.to_lowercase())
    {
        score += 4;
    }
    for token in tokenize_match_text(&todo_text) {
        if token.len() >= 2 && node_text.contains(&token) {
            score += 1;
        }
    }
    score
}

fn tokenize_match_text(text: &str) -> Vec<String> {
    let mut token = String::new();
    let mut tokens = Vec::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(&ch) {
            token.push(ch);
        } else if !token.is_empty() {
            tokens.push(token.clone());
            token.clear();
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

fn normalize_plan_todo(output: PlanTodoOutput) -> Result<PlanTodo> {
    let mut alias_map = HashMap::<String, Vec<String>>::new();
    let mut items = output
        .todos
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let id = format!("todo-{}", index + 1);
            push_alias(&mut alias_map, &id, &id);
            push_alias(&mut alias_map, &item.title, &id);
            let title_slug = slugify(&item.title);
            if !title_slug.is_empty() {
                push_alias(&mut alias_map, &title_slug, &id);
            }
            PlanTodoItem {
                id,
                title: item.title,
                goal: item.goal,
                details: item.details,
                dependencies: item.dependencies,
                completion_criteria: item.completion_criteria,
            }
        })
        .collect::<Vec<_>>();

    for item in &mut items {
        let raw_dependencies = item.dependencies.clone();
        let mut normalized = Vec::new();
        for dep in raw_dependencies {
            let resolved = resolve_plan_todo_dependency(&dep, &alias_map)?;
            if resolved == item.id {
                anyhow::bail!("todo `{}` 依赖自身：`{dep}`", item.id);
            }
            if !normalized.contains(&resolved) {
                normalized.push(resolved);
            }
        }
        item.dependencies = normalized;
    }

    Ok(PlanTodo {
        summary: output.summary,
        approach: output.approach,
        todos: items,
        risks: output.risks,
        used_fallback: false,
        planning_notes: Vec::new(),
        iteration_index: 1,
        source_session_id: None,
        feedback_summary: Vec::new(),
        delta_summary: Vec::new(),
    })
}

fn enrich_plan_todo(config: &SessionConfig, mut plan_todo: PlanTodo) -> PlanTodo {
    plan_todo.iteration_index = config
        .continuation
        .as_ref()
        .map(|item| item.iteration_index.max(1))
        .unwrap_or(1);
    plan_todo.source_session_id = config
        .continuation
        .as_ref()
        .map(|item| item.parent_session_id.clone());
    if let Some(continuation) = &config.continuation {
        let latest_feedback = continuation.latest_feedback_summary();
        plan_todo.feedback_summary = vec![latest_feedback.clone()];
        plan_todo.delta_summary = vec![
            format!(
                "基于 session `{}` 进入 V{} 迭代。",
                continuation.parent_session_id, continuation.iteration_index
            ),
            format!("本轮优先吸收的人类反馈：{latest_feedback}"),
        ];
        plan_todo.planning_notes.push(
            "这是一次 continuation 规划；需要优先保留已验证基线，只在反馈影响范围内调整。"
                .to_string(),
        );
    }
    plan_todo
}

fn fallback_plan(
    config: &SessionConfig,
    roles: &[RoleConfig],
    mut notes: Vec<String>,
) -> Result<ExecutionGraph> {
    let role_map = roles
        .iter()
        .map(|role| (role.key.as_str(), role))
        .collect::<HashMap<_, _>>();
    let ordered_roles = fallback_role_order(config.workers, roles);
    let mut role_counts: HashMap<String, usize> = HashMap::new();

    let mut nodes = ordered_roles
        .into_iter()
        .map(|role_key| {
            let count = role_counts
                .entry(role_key.clone())
                .and_modify(|item| *item += 1)
                .or_insert(1);
            let role = role_map
                .get(role_key.as_str())
                .context("fallback 角色缺失")?;
            let dependencies = fallback_dependencies(&role_key, *count);
            Ok(ExecutionNode {
                id: format!("{role_key}-{count}"),
                title: fallback_title(role, *count),
                todo_id: None,
                role: role_key.clone(),
                objective: fallback_objective(role, *count, &config.task),
                deliverables: fallback_deliverables(role),
                dependencies: dependencies.clone(),
                prompt_focus: fallback_focus(role, *count),
                input_artifacts: fallback_inputs(role),
                output_artifacts: fallback_outputs(role),
                completion_criteria: fallback_completion(role),
                allow_code_changes: role.can_edit,
                expected_artifacts: fallback_deliverables(role),
                required_verifications: Vec::new(),
                scope_guard_ref: None,
                scheduler_hint: infer_scheduler_hint(role, &dependencies),
                acceptable_drift: if role.can_edit {
                    ScopeDrift::Minor
                } else {
                    ScopeDrift::None
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;

    ensure_reviewer_gate(&mut nodes, roles);
    drop_invalid_dependencies(&mut nodes);
    notes.push("planner 不可用，已基于外置角色集合生成保守回退执行图。".to_string());
    let graph = ExecutionGraph {
        summary: "基于已加载角色集合和 worker 数量生成的最小可执行图。".to_string(),
        strategy: "优先沿角色集合顺序推进，并在缺少明确 planner 结果时保守保持 reviewer gate。"
            .to_string(),
        nodes,
        used_fallback: true,
        planning_notes: notes,
    };
    graph.topological_order()?;
    Ok(graph)
}

fn fallback_plan_todo(config: &SessionConfig, mut notes: Vec<String>) -> PlanTodo {
    let keyword_blog = config.task.contains("博客")
        || config.task.to_ascii_lowercase().contains("blog")
        || config.task.to_ascii_lowercase().contains("web");

    let todos = if keyword_blog {
        vec![
            PlanTodoItem {
                id: "todo-1".to_string(),
                title: "明确博客 MVP 范围".to_string(),
                goal: format!("先把 `{}` 收敛成最小可交付范围。", config.task),
                details: vec![
                    "明确是否需要首页、文章详情页、后台或静态发布".to_string(),
                    "确定技术栈、部署方式和内容来源".to_string(),
                ],
                dependencies: vec![],
                completion_criteria: vec![
                    "功能边界明确".to_string(),
                    "技术选型可直接进入实现".to_string(),
                ],
            },
            PlanTodoItem {
                id: "todo-2".to_string(),
                title: "设计页面与数据结构".to_string(),
                goal: "先定义博客页面、文章字段与导航结构，减少返工。".to_string(),
                details: vec![
                    "梳理首页、文章列表、文章详情的结构".to_string(),
                    "定义文章标题、摘要、正文、标签、日期等字段".to_string(),
                ],
                dependencies: vec!["todo-1".to_string()],
                completion_criteria: vec![
                    "页面结构清晰".to_string(),
                    "数据字段足够支撑后续实现".to_string(),
                ],
            },
            PlanTodoItem {
                id: "todo-3".to_string(),
                title: "实现博客主链路".to_string(),
                goal: "完成可运行的博客页面主干，让用户可以浏览内容。".to_string(),
                details: vec![
                    "搭建基础布局、路由和文章展示".to_string(),
                    "优先保证首页与详情页可用".to_string(),
                ],
                dependencies: vec!["todo-2".to_string()],
                completion_criteria: vec![
                    "核心页面可访问".to_string(),
                    "文章展示链路跑通".to_string(),
                ],
            },
            PlanTodoItem {
                id: "todo-4".to_string(),
                title: "补齐样式与验证".to_string(),
                goal: "在主链路可用后补最小样式、错误处理与发布前验证。".to_string(),
                details: vec![
                    "补基础视觉样式与响应式体验".to_string(),
                    "检查空数据、错误页与部署配置".to_string(),
                ],
                dependencies: vec!["todo-3".to_string()],
                completion_criteria: vec![
                    "页面具备基本可用性".to_string(),
                    "至少一条可信验证路径通过".to_string(),
                ],
            },
        ]
    } else {
        vec![
            PlanTodoItem {
                id: "todo-1".to_string(),
                title: "明确目标与边界".to_string(),
                goal: format!("先把 `{}` 的范围、限制和交付标准讲清楚。", config.task),
                details: vec![
                    "确认影响模块、输入输出和非目标范围".to_string(),
                    "明确必须保留的现有约束".to_string(),
                ],
                dependencies: vec![],
                completion_criteria: vec![
                    "任务边界清晰".to_string(),
                    "没有关键前提缺失".to_string(),
                ],
            },
            PlanTodoItem {
                id: "todo-2".to_string(),
                title: "拆最小实现路径".to_string(),
                goal: "把任务拆成能连续推进的最小步骤，而不是一次性大改。".to_string(),
                details: vec![
                    "先做主链路，再补边界和验证".to_string(),
                    "尽量减少跨模块同时改动".to_string(),
                ],
                dependencies: vec!["todo-1".to_string()],
                completion_criteria: vec![
                    "每步都有明确产出".to_string(),
                    "依赖顺序可执行".to_string(),
                ],
            },
            PlanTodoItem {
                id: "todo-3".to_string(),
                title: "完成主干实现".to_string(),
                goal: "优先把能证明价值的主链路做通。".to_string(),
                details: vec![
                    "只改当前任务直接相关的模块".to_string(),
                    "优先修根因，不做顺手重构".to_string(),
                ],
                dependencies: vec!["todo-2".to_string()],
                completion_criteria: vec![
                    "主目标可运行或可验证".to_string(),
                    "改动范围受控".to_string(),
                ],
            },
            PlanTodoItem {
                id: "todo-4".to_string(),
                title: "补验证与交付收敛".to_string(),
                goal: "补最小可信验证，并明确残余风险与下一步。".to_string(),
                details: vec![
                    "运行最小必要测试或 smoke check".to_string(),
                    "整理未验证点和潜在回归".to_string(),
                ],
                dependencies: vec!["todo-3".to_string()],
                completion_criteria: vec![
                    "至少一条验证路径通过".to_string(),
                    "交付说明完整".to_string(),
                ],
            },
        ]
    };

    notes.push("planner 不可用，已生成保守的用户可读 todo 清单。".to_string());
    enrich_plan_todo(
        config,
        PlanTodo {
            summary: format!("围绕 `{}` 生成的最小可执行计划。", config.task),
            approach: "先收敛范围，再完成主链路，最后补验证和交付说明。".to_string(),
            todos,
            risks: vec![
                "如果技术栈、部署方式或内容来源未提前定清，后续实现容易返工。".to_string(),
                "如果任务实际跨多个模块或服务，执行前仍需再确认边界。".to_string(),
            ],
            used_fallback: true,
            planning_notes: notes,
            iteration_index: 1,
            source_session_id: None,
            feedback_summary: Vec::new(),
            delta_summary: Vec::new(),
        },
    )
}

fn build_local_summary(manifest: &SessionManifest) -> FinalSummary {
    let success = manifest
        .worker_results
        .iter()
        .filter(|result| result.status == crate::model::WorkerStatus::Succeeded)
        .count();
    let failed_workers = manifest
        .worker_results
        .iter()
        .filter(|result| result.status == crate::model::WorkerStatus::Failed)
        .count();
    let apply_status = manifest
        .apply_result
        .as_ref()
        .map(|item| item.status)
        .unwrap_or(ApplyStatus::Skipped);
    let review_gate = manifest
        .apply_result
        .as_ref()
        .and_then(|item| item.review_gate)
        .or_else(|| latest_reviewer_gate(manifest));
    let trust_level = manifest
        .apply_result
        .as_ref()
        .map(|item| item.trust_level)
        .unwrap_or(TrustLevel::Low);
    let scope_drift = manifest
        .apply_result
        .as_ref()
        .map(|item| item.scope_drift)
        .unwrap_or(ScopeDrift::None);
    let accepted_files = manifest
        .apply_result
        .as_ref()
        .map(|item| item.accepted_files.clone())
        .unwrap_or_default();
    let manual_review_files = manifest
        .apply_result
        .as_ref()
        .map(|item| item.manual_review_files.clone())
        .unwrap_or_default();
    let rejected_files = manifest
        .apply_result
        .as_ref()
        .map(|item| item.rejected_files.clone())
        .unwrap_or_default();
    let verified_capabilities = manifest
        .verification_report
        .as_ref()
        .map(|item| item.verified_capabilities.clone())
        .unwrap_or_default();
    let blocked_verifications = manifest
        .verification_report
        .as_ref()
        .map(|item| item.blocked_verifications.clone())
        .unwrap_or_default();
    let review_report = manifest
        .apply_result
        .as_ref()
        .and_then(|item| item.review_report.clone())
        .or_else(|| latest_reviewer_report(manifest));
    let open_risks = collect_open_risks(manifest);
    let wrote_to_target = manifest.wrote_to_target();
    let result_status = if failed_workers > 0
        || matches!(
            apply_status,
            ApplyStatus::SyncFailed
                | ApplyStatus::Bundled
                | ApplyStatus::WrittenNeedsFix
                | ApplyStatus::VerificationFailed
                | ApplyStatus::Skipped
        )
        || !wrote_to_target
    {
        ResultStatus::Failed
    } else if !manual_review_files.is_empty()
        || !rejected_files.is_empty()
        || !blocked_verifications.is_empty()
    {
        ResultStatus::CompletedWithManualReview
    } else {
        ResultStatus::Completed
    };
    let evidence_summary = build_evidence_summary(
        manifest,
        &accepted_files,
        &verified_capabilities,
        &blocked_verifications,
    );
    let latest_feedback = manifest
        .feedback_history
        .last()
        .map(|item| item.intent_summary.clone());
    let completed_this_iteration =
        build_iteration_completion_summary(manifest, &accepted_files, &verified_capabilities);

    FinalSummary {
        overview: format!(
            "{} V{}：本次运行共调度 {} 个节点，成功 {} 个、失败 {} 个；apply 状态为 `{}`，可信度为 `{}`，范围漂移为“{}”。",
            if matches!(apply_status, ApplyStatus::Applied) {
                "代码已写入目标目录并通过验证，当前可用版本为"
            } else if wrote_to_target {
                "代码已写入目标目录，但当前版本仍需继续修复"
            } else {
                "本次 session 已结束，但本轮尚未写入目标目录"
            },
            manifest.iteration_index_value(),
            manifest.worker_results.len(),
            success,
            failed_workers,
            apply_status,
            trust_level.label(),
            scope_drift.label(),
        ),
        result_status,
        review_gate,
        apply_status,
        trust_level,
        accepted_files,
        manual_review_files,
        rejected_files,
        verified_capabilities,
        blocked_verifications,
        open_risks,
        recommended_next_action: recommended_next_actions(manifest, result_status),
        todo_states: manifest.todo_states.clone(),
        used_fallback: false,
        review_report,
        evidence_summary,
        iteration_index: manifest.iteration_index_value(),
        based_on_session_id: manifest.parent_session_id.clone(),
        feedback_summary: latest_feedback.into_iter().collect(),
        delta_summary: build_iteration_delta_summary(manifest),
        completed_this_iteration,
        unaccepted_feedback: Vec::new(),
    }
}

fn detect_conflicts(manifest: &SessionManifest) -> Vec<String> {
    let node_allow_map = manifest
        .execution_graph
        .as_ref()
        .map(|graph| {
            graph
                .nodes
                .iter()
                .map(|node| (node.id.as_str(), node.allow_code_changes))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let mut touched_by_file: HashMap<&str, Vec<&str>> = HashMap::new();
    for result in &manifest.worker_results {
        if node_allow_map
            .get(result.agent_id.as_str())
            .is_some_and(|allow| !allow)
        {
            continue;
        }
        for file in &result.changed_files {
            touched_by_file
                .entry(file.as_str())
                .or_default()
                .push(result.agent_id.as_str());
        }
    }

    let mut conflicts = touched_by_file
        .into_iter()
        .filter_map(|(file, workers)| {
            if workers.len() > 1 {
                Some(format!(
                    "文件 `{file}` 被多个 worker 触达：{}",
                    workers.join("、")
                ))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    if let Some(apply_result) = &manifest.apply_result {
        conflicts.extend(apply_result.conflicts.clone());
    }

    conflicts
}

fn collect_open_risks(manifest: &SessionManifest) -> Vec<String> {
    let mut risks = manifest
        .worker_results
        .iter()
        .filter(|result| result.status == crate::model::WorkerStatus::Failed)
        .map(|result| {
            format!(
                "{} 执行失败：{}",
                result.agent_id,
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "未知错误".to_string())
            )
        })
        .collect::<Vec<_>>();

    if let Some(report) = &manifest.verification_report {
        risks.extend(
            report
                .failed_capabilities
                .iter()
                .map(|item| format!("验证失败：{item}")),
        );
    }

    if !manifest.wrote_to_target() {
        risks.push("本轮尚未写入目标目录。".to_string());
    } else if manifest
        .apply_result
        .as_ref()
        .is_some_and(|result| matches!(result.status, ApplyStatus::WrittenNeedsFix))
    {
        risks.push("代码已写入目标目录，但仍有验证或提交问题待处理。".to_string());
    }

    risks.extend(detect_conflicts(manifest));
    risks.sort();
    risks.dedup();
    if risks.is_empty() {
        risks.push("未发现必须立即阻断的开放风险。".to_string());
    }
    risks
}

fn recommended_next_actions(
    manifest: &SessionManifest,
    result_status: ResultStatus,
) -> Vec<String> {
    let mut actions = Vec::new();
    if !manifest.wrote_to_target() {
        if let Some(bundle_dir) = manifest
            .apply_result
            .as_ref()
            .and_then(|result| result.bundle_dir.as_ref())
        {
            actions.push(format!(
                "自动落地未完成；请先检查 bundle 或 apply 日志：`{}`。",
                bundle_dir.display()
            ));
        }
        return actions;
    }
    match manifest.apply_mode {
        crate::model::ApplyMode::None => {
            actions.push(
                format!(
                    "先阅读 `{}`
，按 accepted/manual/rejected 三类清单做人工接收。",
                    manifest.summary_markdown_path.display()
                )
                .replace('\n', ""),
            );
        }
        crate::model::ApplyMode::AutoSafe => {
            if matches!(result_status, ResultStatus::Completed) {
                actions.push("可直接检查目标工作区 diff 并继续提交。".to_string());
            } else {
                actions.push("代码已在目标目录，可直接查看现场并继续修复。".to_string());
            }
        }
        crate::model::ApplyMode::InPlace => {
            if matches!(result_status, ResultStatus::Completed) {
                actions.push("代码已直接写入目标目录，可继续检查 diff 或提交。".to_string());
            } else {
                actions.push(
                    "代码已直接写入目标目录；请先在目标目录修复问题，再决定是否继续 run。"
                        .to_string(),
                );
            }
        }
        crate::model::ApplyMode::Bundle => {
            actions.push("检查 bundle/patch 输出，选择需要人工接收的文件。".to_string());
        }
    }
    actions.push(format!(
        "查看应用报告：`{}`",
        manifest.apply_result_path.display()
    ));
    actions.push(format!(
        "查看验证报告：`{}`",
        manifest.verification_report_path.display()
    ));
    actions.push(format!(
        "如需继续优化，执行：`codex-forge continue --session {} --feedback \"...\"`",
        manifest.id
    ));
    actions
}

fn build_iteration_delta_summary(manifest: &SessionManifest) -> Vec<String> {
    let mut items = Vec::new();
    if let Some(parent) = &manifest.parent_session_id {
        items.push(format!("相对上一轮 session `{parent}` 继续推进。"));
    }
    if let Some(kind) = manifest.continuation_kind {
        items.push(format!("本轮延续类型：{}。", kind.label()));
    }
    if let Some(feedback) = manifest.feedback_history.last() {
        items.push(format!(
            "本轮优先吸收的人类反馈：{}",
            feedback.intent_summary
        ));
    }
    if items.is_empty() {
        items.push("这是根会话的首轮结果。".to_string());
    }
    items
}

fn build_iteration_completion_summary(
    manifest: &SessionManifest,
    accepted_files: &[String],
    verified_capabilities: &[String],
) -> Vec<String> {
    let mut items = Vec::new();
    if !accepted_files.is_empty() {
        items.push(format!("自动接收文件 {} 个。", accepted_files.len()));
    }
    if !verified_capabilities.is_empty() {
        items.push(format!("完成验证能力 {} 项。", verified_capabilities.len()));
    }
    if manifest.worker_results.is_empty() {
        items.push("本轮主要产出为规划与反馈闭环工件。".to_string());
    } else {
        items.push(format!(
            "完成 worker 节点 {} 个。",
            manifest.worker_results.len()
        ));
    }
    items
}

fn latest_reviewer_gate(manifest: &SessionManifest) -> Option<ApplyDecision> {
    manifest
        .worker_results
        .iter()
        .filter(|result| result.role == "reviewer")
        .filter_map(|result| {
            result
                .handoff
                .as_ref()
                .and_then(|handoff| handoff.apply_decision)
        })
        .next_back()
}

fn latest_reviewer_report(manifest: &SessionManifest) -> Option<crate::model::ReviewGateReport> {
    manifest
        .worker_results
        .iter()
        .filter(|result| result.role == "reviewer")
        .filter_map(|result| {
            result.handoff.as_ref().and_then(|handoff| {
                handoff
                    .apply_decision
                    .map(|decision| crate::model::ReviewGateReport {
                        decision,
                        blocking_findings: handoff.blocking_findings.clone(),
                        accepted_scopes: handoff.accepted_scopes.clone(),
                        rejected_scopes: handoff.rejected_scopes.clone(),
                        confidence_reasoning: handoff.confidence_reasoning.clone(),
                    })
            })
        })
        .next_back()
}

fn build_evidence_summary(
    manifest: &SessionManifest,
    accepted_files: &[String],
    verified_capabilities: &[String],
    blocked_verifications: &[String],
) -> Vec<String> {
    let mut evidence = Vec::new();
    if let Some(report) = manifest
        .apply_result
        .as_ref()
        .and_then(|item| item.review_report.as_ref())
    {
        evidence.push(format!("reviewer 结论：{}", report.decision.label()));
        if let Some(reason) = &report.confidence_reasoning {
            evidence.push(format!("reviewer 说明：{reason}"));
        }
    }
    if !accepted_files.is_empty() {
        evidence.push(format!("自动接收文件 {} 个。", accepted_files.len()));
    }
    if !verified_capabilities.is_empty() {
        evidence.push(format!("验证通过 {} 项能力。", verified_capabilities.len()));
    }
    if !blocked_verifications.is_empty() {
        evidence.push(format!("环境阻塞 {} 项验证。", blocked_verifications.len()));
    }
    if evidence.is_empty() {
        evidence.push("本次运行未沉淀出强证据，建议查看原始工件。".to_string());
    }
    evidence
}

fn task_fingerprint(config: &SessionConfig, graph: &ExecutionGraph) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.task.hash(&mut hasher);
    config.role_set.hash(&mut hasher);
    config.workers.hash(&mut hasher);
    graph.summary.hash(&mut hasher);
    for node in &graph.nodes {
        node.id.hash(&mut hasher);
        node.role.hash(&mut hasher);
        node.title.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

fn fallback_role_order(workers: usize, roles: &[RoleConfig]) -> Vec<String> {
    let workers = workers.max(1);
    let mut ordered = roles
        .iter()
        .take(workers)
        .map(|role| role.key.clone())
        .collect::<Vec<_>>();

    while ordered.len() < workers {
        if let Some(role) = roles.iter().find(|item| item.key == "implementer") {
            ordered.push(role.key.clone());
        } else if let Some(role) = roles.iter().find(|item| item.can_edit) {
            ordered.push(role.key.clone());
        } else if let Some(role) = roles.first() {
            ordered.push(role.key.clone());
        } else {
            break;
        }
    }

    ordered
}

fn fallback_title(role: &RoleConfig, count: usize) -> String {
    if count == 1 {
        format!("{}主任务", role.title)
    } else {
        format!("{}任务 {}", role.title, count)
    }
}

fn fallback_objective(role: &RoleConfig, count: usize, task: &str) -> String {
    if count == 1 {
        format!(
            "围绕 `{task}` 以 {} 身份推进：{}。",
            role.title, role.mission
        )
    } else {
        format!("围绕 `{task}` 继续推进第 {count} 个 {} 节点。", role.title)
    }
}

fn fallback_deliverables(role: &RoleConfig) -> Vec<String> {
    let mut deliverables = vec!["结构化交付摘要".to_string(), "风险与后续建议".to_string()];
    if role.can_edit {
        deliverables.insert(0, "直接可消费的代码或配置改动".to_string());
    } else {
        deliverables.insert(0, "只读分析结论".to_string());
    }
    deliverables
}

fn fallback_dependencies(role_key: &str, count: usize) -> Vec<String> {
    if count > 1 {
        vec![format!("{role_key}-{}", count - 1)]
    } else {
        Vec::new()
    }
}

fn fallback_focus(role: &RoleConfig, count: usize) -> String {
    if count == 1 {
        role.working_style.clone()
    } else {
        format!("延续 `{}` 的工作风格，减少和前序节点冲突。", role.title)
    }
}

fn fallback_inputs(role: &RoleConfig) -> Vec<String> {
    if role.can_edit {
        vec!["仓库摘要".to_string(), "上游 handoff".to_string()]
    } else {
        vec!["仓库摘要".to_string(), "候选改动".to_string()]
    }
}

fn fallback_outputs(role: &RoleConfig) -> Vec<String> {
    if role.key == "reviewer" {
        vec!["reviewer handoff".to_string()]
    } else if role.can_edit {
        vec!["代码改动".to_string(), "handoff".to_string()]
    } else {
        vec!["分析结论".to_string(), "handoff".to_string()]
    }
}

fn fallback_completion(role: &RoleConfig) -> Vec<String> {
    let mut completion = vec!["最终输出含完整 handoff 小节".to_string()];
    if role.key == "reviewer" {
        completion.push("给出应用前结论".to_string());
    }
    if role.can_edit {
        completion.push("代码改动聚焦当前节点".to_string());
    } else {
        completion.push("结论足够支撑下游继续执行".to_string());
    }
    completion
}

fn ensure_reviewer_gate(nodes: &mut Vec<ExecutionNode>, roles: &[RoleConfig]) {
    if nodes.iter().any(|node| node.role == "reviewer") {
        return;
    }
    if !roles.iter().any(|role| role.key == "reviewer") {
        return;
    }

    let dependencies = nodes
        .iter()
        .filter(|node| node.role != "architect")
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();

    let reviewer_role = roles
        .iter()
        .find(|role| role.key == "reviewer")
        .expect("reviewer role should exist");

    nodes.push(ExecutionNode {
        id: "reviewer-1".to_string(),
        title: format!("{}关卡", reviewer_role.title),
        todo_id: None,
        role: "reviewer".to_string(),
        objective: format!(
            "在自动应用前以 {} 身份完成最终审阅：{}。",
            reviewer_role.title, reviewer_role.mission
        ),
        deliverables: fallback_deliverables(reviewer_role),
        dependencies,
        prompt_focus: reviewer_role.working_style.clone(),
        input_artifacts: vec!["所有上游 handoff".to_string(), "所有候选 patch".to_string()],
        output_artifacts: fallback_outputs(reviewer_role),
        completion_criteria: fallback_completion(reviewer_role),
        allow_code_changes: false,
        expected_artifacts: vec!["reviewer handoff".to_string()],
        required_verifications: vec!["检查范围漂移与冲突".to_string()],
        scope_guard_ref: None,
        scheduler_hint: Some(SchedulerHint::Closure),
        acceptable_drift: ScopeDrift::None,
    });
}

fn infer_scheduler_hint(role: &RoleConfig, dependencies: &[String]) -> Option<SchedulerHint> {
    if !role.can_edit {
        Some(SchedulerHint::Closure)
    } else if dependencies.is_empty() {
        Some(SchedulerHint::CriticalPath)
    } else {
        Some(SchedulerHint::Unlock)
    }
}

fn normalize_dependencies(nodes: &mut [ExecutionNode]) -> Result<()> {
    let mut alias_map = HashMap::<String, Vec<String>>::new();
    let mut role_map = HashMap::<String, Vec<String>>::new();

    for node in nodes.iter() {
        push_alias(&mut alias_map, &node.id, &node.id);
        push_alias(&mut alias_map, &node.title, &node.id);
        let title_slug = slugify(&node.title);
        if !title_slug.is_empty() {
            push_alias(&mut alias_map, &title_slug, &node.id);
        }
        role_map
            .entry(node.role.clone())
            .or_default()
            .push(node.id.clone());
    }

    for node in nodes.iter_mut() {
        let raw_dependencies = node.dependencies.clone();
        let mut normalized = Vec::new();
        for dep in raw_dependencies {
            let resolved = resolve_dependency(&dep, &alias_map, &role_map)?;
            if resolved == node.id {
                anyhow::bail!("节点 `{}` 依赖自身：`{dep}`", node.id);
            }
            if !normalized.contains(&resolved) {
                normalized.push(resolved);
            }
        }
        node.dependencies = normalized;
    }

    Ok(())
}

fn drop_invalid_dependencies(nodes: &mut [ExecutionNode]) {
    let existing = nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<HashSet<_>>();
    for node in nodes {
        node.dependencies.retain(|dep| existing.contains(dep));
    }
}

fn resolve_dependency(
    raw: &str,
    alias_map: &HashMap<String, Vec<String>>,
    role_map: &HashMap<String, Vec<String>>,
) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("planner 产生了空依赖");
    }

    let normalized = slugify(trimmed);
    let candidates = alias_map
        .get(trimmed)
        .or_else(|| alias_map.get(&normalized))
        .cloned()
        .or_else(|| role_map.get(trimmed).filter(|ids| ids.len() == 1).cloned())
        .ok_or_else(|| anyhow::anyhow!("planner 依赖 `{trimmed}` 无法解析到节点 id"))?;

    if candidates.len() != 1 {
        anyhow::bail!(
            "planner 依赖 `{trimmed}` 映射到多个节点：{}",
            candidates.join("、")
        );
    }

    Ok(candidates[0].clone())
}

fn resolve_plan_todo_dependency(
    raw: &str,
    alias_map: &HashMap<String, Vec<String>>,
) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("todo 产生了空依赖");
    }

    let normalized = slugify(trimmed);
    let candidates = alias_map
        .get(trimmed)
        .or_else(|| alias_map.get(&normalized))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("todo 依赖 `{trimmed}` 无法解析到节点 id"))?;

    if candidates.len() != 1 {
        anyhow::bail!(
            "todo 依赖 `{trimmed}` 映射到多个节点：{}",
            candidates.join("、")
        );
    }

    Ok(candidates[0].clone())
}

fn push_alias(alias_map: &mut HashMap<String, Vec<String>>, alias: &str, node_id: &str) {
    let key = alias.trim();
    if key.is_empty() {
        return;
    }
    let entry = alias_map.entry(key.to_string()).or_default();
    if !entry.iter().any(|item| item == node_id) {
        entry.push(node_id.to_string());
    }
}

fn slugify(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter(|ch| ch.is_alphanumeric() || *ch == '-' || *ch == '_')
        .collect()
}

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        trimmed.to_string()
    } else {
        format!("{}...", trimmed.chars().take(max).collect::<String>())
    }
}

fn summarize_error(text: &str) -> String {
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !is_error_noise_line(line))
        .collect::<Vec<_>>();

    let mut preferred = lines
        .iter()
        .copied()
        .filter(|line| is_preferred_error_line(line))
        .map(normalize_error_line)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    preferred.dedup();

    if !preferred.is_empty() {
        return truncate(&preferred.join("；"), 220);
    }

    let fallback = lines
        .into_iter()
        .map(normalize_error_line)
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| text.trim().to_string());
    truncate(&fallback, 220)
}

fn is_error_noise_line(line: &str) -> bool {
    matches!(
        line,
        "user" | "assistant" | "mcp startup: no servers" | "--------"
    ) || line.starts_with("OpenAI Codex v")
        || line.starts_with("workdir:")
        || line.starts_with("model:")
        || line.starts_with("provider:")
        || line.starts_with("approval:")
        || line.starts_with("sandbox:")
        || line.starts_with("reasoning effort:")
        || line.starts_with("reasoning summaries:")
        || line.starts_with("session id:")
        || line.starts_with("全局任务：")
        || line.starts_with("目标仓库：")
        || line.starts_with("技术栈：")
        || line.starts_with("期望 worker 数量上限：")
        || line.starts_with("角色集合：")
        || line.starts_with("默认应用模式：")
        || line.starts_with("可用角色：")
        || line.starts_with("仓库摘要：")
        || line.starts_with("规划要求：")
        || line.starts_with("你现在是 codex-forge")
        || line.starts_with("请只输出符合 schema")
        || line.starts_with('#')
        || line.starts_with("```")
        || line.starts_with("- ")
}

fn is_preferred_error_line(line: &str) -> bool {
    line.starts_with("ERROR:")
        || line.starts_with("Warning:")
        || line.contains("unexpected status")
        || line.contains("调用失败")
        || line.contains("解析失败")
        || line.contains("结果不可用")
        || line.contains("缺少结果文件")
        || line.contains("Operation not permitted")
        || line.contains("Permission denied")
        || line.contains("os error")
        || line.contains("fatal:")
}

fn normalize_error_line(line: &str) -> String {
    let normalized = line
        .trim_start_matches("ERROR:")
        .trim_start_matches("Warning:")
        .trim_start_matches("Codex 结构化调用失败：")
        .trim();

    if normalized.starts_with("OpenAI Codex v") {
        return String::new();
    }

    if let Some(index) = normalized.find("unexpected status") {
        return normalized[index..]
            .trim()
            .split(", cf-ray:")
            .next()
            .unwrap_or(normalized)
            .trim()
            .trim_end_matches(')')
            .trim()
            .to_string();
    }
    if let Some(index) = normalized.find("os error") {
        return normalized[index..].trim().to_string();
    }
    if let Some(index) = normalized.find("fatal:") {
        return normalized[index..].trim().to_string();
    }
    if let Some(index) = normalized.find("Operation not permitted") {
        return normalized[index..].trim().to_string();
    }
    if let Some(index) = normalized.find("Permission denied") {
        return normalized[index..].trim().to_string();
    }

    normalized.to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        PlanTodoOutput, PlanTodoOutputItem, PlannerNode, PlannerOutput, fallback_plan,
        normalize_plan, normalize_plan_todo, summarize_error,
    };
    use crate::model::{ApplyMode, RoleConfig, SessionConfig, ThinkingMode, UiMode};
    use std::path::PathBuf;

    fn sample_roles() -> Vec<RoleConfig> {
        vec![
            RoleConfig {
                key: "architect".to_string(),
                title: "架构师".to_string(),
                mission: "拆解边界".to_string(),
                skills: vec!["system-design".to_string()],
                working_style: "先拆后做".to_string(),
                can_edit: false,
                max_concurrency: Some(1),
                dependency_policy: Some("fan_out".to_string()),
                prompt_preamble: None,
            },
            RoleConfig {
                key: "implementer".to_string(),
                title: "实现者".to_string(),
                mission: "完成实现".to_string(),
                skills: vec!["coding".to_string()],
                working_style: "直接推进".to_string(),
                can_edit: true,
                max_concurrency: None,
                dependency_policy: Some("ready_only".to_string()),
                prompt_preamble: None,
            },
            RoleConfig {
                key: "reviewer".to_string(),
                title: "审阅者".to_string(),
                mission: "做最终 gate".to_string(),
                skills: vec!["review".to_string()],
                working_style: "先找问题".to_string(),
                can_edit: false,
                max_concurrency: Some(1),
                dependency_policy: Some("gate_before_apply".to_string()),
                prompt_preamble: None,
            },
        ]
    }

    #[test]
    fn fallback_plan_respects_worker_count() {
        let config = SessionConfig {
            task: "实现多 agent CLI".to_string(),
            workers: 3,
            role_set: "default".to_string(),
            model: None,
            thinking_mode: ThinkingMode::Balanced,
            ui_mode: UiMode::Minimal,
            target_dir: PathBuf::from("."),
            cleanup_success: false,
            apply_mode: ApplyMode::AutoSafe,
            max_retries: 1,
            fail_fast: false,
            verification_commands: vec!["cargo test".to_string()],
            config_path: None,
            global_rule_prompt: "全局规则".to_string(),
            reviewer_rule_prompt: Some("reviewer 规则".to_string()),
            plan_only: false,
            preset: None,
            source_plan_session_id: None,
            resume_session_id: None,
            continuation: None,
        };
        let roles = sample_roles();
        let graph = fallback_plan(&config, &roles, Vec::new()).expect("fallback graph");
        assert!(graph.nodes.len() >= 3);
        assert!(graph.topological_order().is_ok());
    }

    #[test]
    fn normalize_plan_resolves_title_dependencies() {
        let roles = sample_roles();
        let graph = normalize_plan(
            PlannerOutput {
                summary: "x".to_string(),
                strategy: "y".to_string(),
                nodes: vec![
                    PlannerNode {
                        title: "架构设计".to_string(),
                        todo_ref: None,
                        role: "architect".to_string(),
                        objective: "拆解".to_string(),
                        deliverables: vec![],
                        dependencies: vec![],
                        prompt_focus: "聚焦拆解".to_string(),
                        input_artifacts: vec![],
                        output_artifacts: vec![],
                        completion_criteria: vec![],
                    },
                    PlannerNode {
                        title: "实现主干".to_string(),
                        todo_ref: None,
                        role: "implementer".to_string(),
                        objective: "实现".to_string(),
                        deliverables: vec![],
                        dependencies: vec!["架构设计".to_string()],
                        prompt_focus: "聚焦实现".to_string(),
                        input_artifacts: vec![],
                        output_artifacts: vec![],
                        completion_criteria: vec![],
                    },
                ],
            },
            &roles,
            2,
            None,
            Vec::new(),
        )
        .expect("normalized graph");

        let implementer = graph
            .nodes
            .iter()
            .find(|node| node.role == "implementer")
            .expect("implementer");
        assert_eq!(implementer.dependencies, vec!["architect-1".to_string()]);
    }

    #[test]
    fn normalize_plan_todo_resolves_title_dependencies() {
        let plan_todo = normalize_plan_todo(PlanTodoOutput {
            summary: "x".to_string(),
            approach: "y".to_string(),
            risks: vec![],
            todos: vec![
                PlanTodoOutputItem {
                    title: "明确范围".to_string(),
                    goal: "澄清任务".to_string(),
                    details: vec![],
                    dependencies: vec![],
                    completion_criteria: vec![],
                },
                PlanTodoOutputItem {
                    title: "实现主干".to_string(),
                    goal: "实现功能".to_string(),
                    details: vec![],
                    dependencies: vec!["明确范围".to_string()],
                    completion_criteria: vec![],
                },
            ],
        })
        .expect("todo should normalize");

        assert_eq!(plan_todo.todos[1].dependencies, vec!["todo-1".to_string()]);
    }

    #[test]
    fn summarize_error_filters_prompt_noise() {
        let error = "OpenAI Codex v0.115.0 (research preview)\nworkdir: /tmp/demo\nuser\n全局任务：做个博客\nReconnecting... 1/5 (unexpected status 502 Bad Gateway)\nERROR: unexpected status 502 Bad Gateway";
        assert_eq!(summarize_error(error), "unexpected status 502 Bad Gateway");
    }
}
