# `codex-forge` DeerFlow 2.0 化改造计划

## 目标

把当前偏 `ExecutionGraph + worker` 的架构，逐步改造成 DeerFlow 2.0 风格的 **super agent harness**：

- 以 `thread/run` 作为长期协作主线
- 以 `tools/skills/sub-agents` 作为能力边界
- 以 `approval/sandbox` 作为安全边界
- 以 `Codex CLI` 作为唯一 agent backend
- 以 `Rust` 作为唯一宿主语言

## 分阶段实施

### Phase 1：只读 harness 骨架

- 新增 `harness` 模块，建立 `thread/run/event` 持久化模型
- 新增 `CodexBackend::run_turn` 风格的只读执行入口
- 引入首批只读工具定义：`read_file`、`search_files`、`inspect_repo`、`load_memory`
- 新增 `harness` CLI 命令与 `threads list`
- 保留旧 `orchestrator` 路径不动，先并行存在

### Phase 2：统一 action / approval

- 将写文件、patch、shell、git、验证统一到工具执行管线
- 所有 mutating 能力强制走审批模型
- 让 super agent 能显式请求下一步动作，而不是直接隐式推进

### Phase 3：sub-agent 与 skill system

- 引入 archetype：`super_agent`、`explorer`、`worker`、`reviewer`、`tester`
- 将 `.roles` 中的 skills 提升为运行时上下文一等公民
- 用 worktree 承载 mutating sub-agent 的隔离工作区

### Phase 4：memory / replay / verification 收口

- 统一短期上下文、历史摘要、长期记忆
- 把验证结果并入 thread memory
- 让 replay 基于 event stream，而不是只看最终 summary

### Phase 5：主路径切换

- 新 CLI/TUI 以 harness 语义为主
- `ExecutionGraph` 降级为可选 plan 表达
- 旧 orchestrator 退为兼容层

## 本轮已落地

- 新增 `src/harness/` 骨架
- 新增 `thread` / `run` / `event` JSON 持久化
- 新增只读 `harness` 命令
- 新增 `threads list`
- 新增基础测试覆盖

## 后续优先级

1. 给 harness 接入真实 tool call 执行器
2. 接入审批状态机与 mutating tools
3. 接入 sub-agent 生命周期管理
4. 把 summary / memory / replay 迁到新 runtime
5. 再考虑切换主入口
