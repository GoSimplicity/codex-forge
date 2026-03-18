# codex-forge

`codex-forge` 是一个用 Rust 编写的多 Agent Codex 指挥台 CLI。它的目标是把单个 Agent 的串行开发，升级为「一个 commander + 多个 worker」的并行协同流程，并进一步收敛为“显式执行图 + handoff 工件 + integration worktree + 全量验证”的真实协作闭环。

## V2 能力

- 用 `codex exec` 作为真实执行后端，并对瞬时失败、输出污染、缺少结果文件等场景做重试与诊断
- 从粗粒度 shard 升级为显式 `ExecutionGraph`：每个节点包含角色、依赖、输入工件、输出工件、完成条件
- `plan` 会先生成面向用户的 todo 清单，再派生内部 `ExecutionGraph`
- worker 默认输出标准化 handoff 工件：摘要、变更意图、触达文件、风险、验证、给下游 agent 的建议
- commander 采用多阶段调度：规划 → 依赖驱动分发 → reviewer gate → integration apply → integration/final verification → summary
- 默认支持 `auto-safe` 自动收敛：无冲突 patch 自动应用；冲突时自动降级为 `bundle`
- reviewer gate 采用显式放行：reviewer 必须输出 `APPLY_DECISION: allow` 才会进入自动应用，否则默认降级为 `bundle`
- 使用独立 `git worktree` 隔离 worker，并新增 integration worktree 负责汇总、验证和最终同步
- 支持项目级 `codex-forge.toml`：覆盖角色模板、skills 描述、默认 model、并发数、重试参数、验证命令、应用策略
- 支持 `doctor`、`config validate`、`replay`，并保存执行图、handoff、apply result、verification report 等完整 session 工件
- `run` 默认会复用最近一次匹配的 `plan` 会话（同 task / workers / role_set），把规划结果直接带入执行
- 支持 rich / minimal 两种 CLI 展示模式，minimal 模式适合 CI / 非 TTY 场景

## 命令

```bash
cargo run -- agents list
cargo run -- plan "我现在要创建一个简单的web博客，给我规划" --ui minimal
cargo run -- run "我现在要创建一个简单的web博客，给我规划" --workers 4 --ui rich
cargo run -- run "我现在要创建一个简单的web博客，给我规划" --apply-mode auto-safe --max-retries 2
cargo run -- doctor
cargo run -- config validate
cargo run -- replay --ui minimal
```

`plan` 会输出用户可读的 todo 清单；随后如果用相同 task 再执行 `run`，会优先复用最近一次匹配的规划结果。

### 常用参数

- `--config <path>`：显式指定项目配置文件
- `--workers <n>`：并发 worker 数量
- `--role-set <preset>`：角色模板集合，默认 `core`
- `--model <name>`：统一指定 Codex model
- `--apply-mode auto-safe|bundle|none`：控制是否自动收敛并落地
- `--max-retries <n>`：worker / structured Codex 调用最大重试次数
- `--fail-fast`：节点失败后停止分发后续节点
- `--ui rich|minimal`：终端展示模式
- `--target-dir <path>`：目标仓库路径
- `--cleanup-success`：成功后自动清理 worker worktree

## Session 输出

每次执行都会在目标仓库下生成：

```text
.codex-forge/sessions/<session-id>/
```

其中包含：

- `manifest.json`：完整 session 元信息
- `timeline.jsonl`：全局事件流，可用于 replay
- `commander/plan-todo.json` / `commander/plan-todo.md`：面向用户的计划清单
- `commander/execution-graph.json`：显式执行图
- `workers/<agent-id>/prompt.md`：下发给 worker 的 prompt
- `workers/<agent-id>/events.jsonl`：该 worker 的原始事件
- `workers/<agent-id>/final.md`：该 worker 的最终输出
- `workers/<agent-id>/handoff.json`：结构化交接工件
- `workers/<agent-id>/changes.patch`：差异补丁快照
- `artifact-manifest.json`：工件索引
- `integration/apply-plan.json`：候选 patch 应用顺序
- `integration/apply-result.json`：自动应用结果与冲突信息
- `integration/verification-report.json`：worker / integration / final 验证报告
- `summary.json` / `summary.md`：最终收敛摘要

## 架构概览

- `src/commander.rs`：todo 规划、ExecutionGraph 派生、fallback 拆图与 summary 收敛
- `src/codex.rs`：`codex exec` 适配、混合事件流解析、重试、handoff 提取
- `src/worktree.rs`：`git worktree` 生命周期、patch 捕获与 apply 辅助
- `src/apply.rs`：integration worktree 汇总、auto-safe 应用、bundle 降级
- `src/verify.rs`：integration / final 验证执行与报告生成
- `src/config.rs`：`codex-forge.toml` 解析与默认值覆盖
- `src/doctor.rs`：环境预检
- `src/orchestrator.rs`：session 主流程编排与依赖驱动调度
- `src/ui.rs`：rich / minimal CLI 可视化
- `src/replay.rs`：timeline 回放

## 配置示例

```toml
[defaults]
workers = 4
apply_mode = "auto-safe"
max_retries = 2
verification_commands = [
  "cargo fmt --check",
  "cargo clippy --all-targets --all-features -- -D warnings",
  "cargo test",
]

[roles.reviewer]
can_edit = false
working_style = "默认只读，先找问题再给修正建议"
```

## 当前限制 / 边界

- 当前仍然是本地单机 CLI，不做远程分布式 worker
- `auto-safe` 不会强制解决真实冲突；一旦 patch 无法应用，会自动降级为 `bundle`
- 角色体系支持项目级覆盖，但仍以内置 `core` 模板为基础
- integration/final 验证命令依赖本地环境；如外部工具缺失，可先用 `doctor` 预检
