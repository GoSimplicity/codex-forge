# `codex-forge` DeerFlow 化重构总计划

## 摘要

本计划不是在当前 `ExecutionGraph + worker + auto-safe apply` 架构上继续打补丁，而是按 `deer-flow 2.0` 的核心思想，把 `codex-forge` 重写为一个以 `thread/run + super agent runtime` 为中心的 Rust 宿主，并把 `Codex CLI` 作为默认且唯一的 agent backend。

目标形态：

- `codex-forge` 从“多 worker 编排器”升级为“可长期运行的 super agent harness”
- 核心抽象从 `plan/run session` 转为 `thread/run/message/event`
- `ExecutionGraph` 从主运行时降级为可选规划产物，不再主导执行流
- tool、skill、sandbox、approval、sub-agent、memory、replay 成为运行时一等公民
- 在保留现有 CLI/TUI 优势的基础上，为后续 Gateway/API/Web UI 留出标准接口
- 旧 orchestrator 不做长期兼容，只保留短期迁移桥，最终移除

默认采用一次重构、分阶段落地的方式，但每一阶段都必须可运行、可回放、可验证，避免“大爆炸式”重写后长期不可用。

## 目标架构

### 顶层分层

重构后拆为三层，依赖方向单向：

1. `forge-harness`
   - 纯运行时内核
   - 包含 thread/run 状态机、agent runtime、tool registry、approval、sandbox、skills、sub-agent、memory、artifact、replay event
   - 不依赖 CLI/TUI/Gateway
2. `forge-app`
   - 应用编排层
   - 提供 CLI 命令适配、TUI 状态装配、未来 Gateway HTTP API
   - 负责把外部输入转换为 harness 可消费的请求
3. `forge-clients`
   - 终端交互、未来 Web/Gateway SDK、外部聊天渠道适配
   - 全部只依赖 app/harness 暴露的稳定接口

原则：

- `harness` 永远不反向依赖 `app` 或 UI 层逻辑
- 线程、运行、事件、工具调用、审批、产物索引全部在 `harness` 内定义
- CLI/TUI/Gateway 只是不同入口，不得各自维护一套运行时

### 核心运行时对象

统一以下一等公民模型：

- `Thread`
  - 长期会话容器
  - 持有消息历史、thread metadata、title、memory refs、artifact refs、sandbox binding
- `Run`
  - 某次执行实例
  - 持有 model config、mode、turns、tool calls、subagent execution、approval records、final outcome
- `Message`
  - user / assistant / tool / system / summary / clarification
- `RuntimeEvent`
  - 统一 event stream
  - 覆盖 message append、tool requested、tool completed、approval requested、approval resolved、subagent spawned、memory updated、artifact emitted、run completed
- `RuntimeContext`
  - thread_id、run_id、workspace、uploads、outputs、model、thinking_enabled、plan_mode、subagent_enabled、sandbox_id、user metadata
- `Artifact`
  - 文件、diff、报告、图片、摘要、日志、handoff、verification result
- `ApprovalRequest`
  - 对 shell、write_file、apply_patch、git、delete、network 等高风险动作的显式授权请求
- `SubagentTask`
  - 角色、目标、输入上下文、工具白名单、工作目录、完成条件、结果摘要

### 运行模型

采用 DeerFlow 风格的 lead-agent 主循环，但宿主为 Rust：

1. 外部入口创建或载入 `thread`
2. 生成一个新的 `run`
3. 构造运行时上下文与工具可见性
4. 把 thread 消息、skills、memory、sandbox 信息拼成当前 turn prompt
5. 调用 `Codex CLI` 执行一个 turn
6. 解析 assistant 输出中的：
   - 普通文本
   - 结构化 tool call
   - clarification
   - sub-agent task
   - final response
7. tool 与 sub-agent 执行结果以 `ToolMessage` / `SubagentResult` 的形式回灌 thread
8. 达到终止条件后写入 final summary、artifact index、memory candidate、replay timeline

关键点：

- 运行时以“turn”为单位推进，而不是一次性生成整个执行图后盲跑
- sub-agent 是 runtime 动态行为，不是预先硬编码在 graph 里的静态节点
- 所有副作用动作必须被 event 化、可回放、可审计

## 关键接口与公共契约

### 稳定内部接口

计划新增并稳定以下 Rust 接口族：

- `Harness`
  - `create_thread`
  - `append_user_message`
  - `start_run`
  - `resume_run`
  - `cancel_run`
  - `stream_events`
  - `list_threads`
  - `get_thread`
  - `get_artifacts`
- `AgentBackend`
  - `run_turn(request) -> TurnResult`
  - 当前唯一实现为 `CodexBackend`
- `ToolExecutor`
  - `list_tools(context) -> Vec<ToolSpec>`
  - `execute(call, context) -> ToolOutcome`
- `ApprovalManager`
  - `evaluate(policy, tool_call) -> ApprovalDecision | PendingApproval`
- `SubagentExecutor`
  - `spawn(task) -> SubagentHandle`
  - `poll(handle) -> SubagentState`
- `MemoryStore`
  - `load_context(thread_id)`
  - `append_candidate(entries)`
  - `materialize_prompt_block(thread_id)`

### CLI / API 语义变更

现有命令将向 thread/run 语义收敛：

- 新主命令：
  - `codex-forge thread new`
  - `codex-forge thread list`
  - `codex-forge chat`
  - `codex-forge run resume`
  - `codex-forge approval`
  - `codex-forge artifacts`
  - `codex-forge replay`
- 兼容迁移命令：
  - `plan` 变为“显式进入 plan mode 的 run”
  - `run` 变为“对某个 thread 发起执行”
  - `continue` 合并到 `chat` / `run resume`
- 未来 Gateway API 默认围绕：
  - `/threads`
  - `/threads/{id}/messages`
  - `/threads/{id}/runs`
  - `/runs/{id}/events`
  - `/approvals`
  - `/artifacts`
  - `/skills`
  - `/agents`

### 持久化目录重构

当前 `.codex-forge/sessions/...` 需要迁移为 thread-first 结构：

```text
.codex-forge/
  threads/{thread_id}/
    thread.json
    messages.jsonl
    memory/
    artifacts/
    uploads/
    workspace/
    outputs/
    runs/{run_id}/
      run.json
      events.jsonl
      approvals.json
      tool-calls.json
      summary.md
```

迁移规则：

- 旧 `session` 不直接删除
- 先提供只读 importer，把旧 `SessionManifest` 映射为历史 thread/run
- 新老数据至少允许共存一个版本周期
- 当 TUI 与 replay 完全切到新结构后，再删除旧路径

## 子系统设计

### Agent Runtime

运行时内核需要提供 DeerFlow 风格 middleware pipeline，但以 Rust 实现。

建议中间件顺序：

1. `ThreadDataMiddleware`
2. `UploadsMiddleware`
3. `SandboxMiddleware`
4. `ToolSchemaMiddleware`
5. `ContextCompressionMiddleware`
6. `TodoMiddleware`
7. `MemoryMiddleware`
8. `SubagentPolicyMiddleware`
9. `ClarificationMiddleware`
10. `ToolErrorMiddleware`
11. `DanglingCallRepairMiddleware`

要求：

- 每个 middleware 输入输出必须是显式状态增量
- middleware 必须可测试，不能把逻辑散落到 CLI/TUI
- `CodexBackend` 不直接碰文件系统副作用，只消费上下文并返回结构化结果

### Tool / Approval / Sandbox

当前系统的 apply、verify、git、worktree 能力要统一进工具框架，不再作为 orchestrator 外挂流程。

工具分层：

- 只读工具
  - `read_file`
  - `search_files`
  - `inspect_tree`
  - `load_artifact`
  - `view_image`
  - `read_memory`
- 弱副作用工具
  - `create_todo`
  - `write_note`
  - `emit_artifact`
- 强副作用工具
  - `write_file`
  - `apply_patch`
  - `run_shell`
  - `git_status`
  - `git_commit`
  - `run_verification`
  - `spawn_subagent`

审批模型：

- `allow`
- `deny`
- `require_user`
- `require_policy_escalation`

默认策略：

- 只读工具默认放行
- 写文件、patch、shell、git、网络、删除动作默认至少经过 policy 检查
- 若处于 `auto` 模式，策略可自动放行低风险动作
- 若处于 `interactive` 模式，必须生成待审批队列，由 CLI/TUI/Gateway 显示后确认

sandbox 策略：

- v1 默认先使用本地 workspace sandbox
- 保留 provider trait，未来可扩展 Docker/K8s
- 每个 thread 绑定一个 workspace；sub-agent 可共享 thread sandbox，但拥有独立工作子目录
- 当前已有 `worktree` 机制保留，但只用于高风险代码修改型 sub-agent，不再是所有 worker 的默认执行方式

### Skill System

把当前 `.roles` 和 prompt 模板体系升级为 DeerFlow 风格 skill system。

定义：

- 每个 skill 为一个目录，至少包含 `SKILL.md`
- 可选包含 `references/`、`scripts/`、`assets/`、`manifest.toml`
- skill 支持元数据：
  - `name`
  - `description`
  - `triggers`
  - `compatibility`
  - `tool_requirements`
  - `agent_modes`

运行机制：

- 先做 skill discovery
- 再按用户请求、thread 上下文、agent mode 做按需加载
- 只把必要片段注入当前 turn，不整包塞入上下文
- 支持 public/custom 两类 skill 根目录
- 未来 Gateway 需要支持列出、启停、安装、校验 skill

迁移策略：

- 当前 `roles.rs` 的角色提示不要继续扩展
- 将现有 architect / implementer / reviewer / tester 的行为规范拆成内置 skill 或 sub-agent archetype
- 用户级自定义行为优先落到 skill，而非写死在 Rust 代码里

### Sub-Agent System

从“预分配 worker 节点”改为“lead agent 动态派生 sub-agents”。

子代理类型建议首批固定为：

- `explorer`
- `implementer`
- `reviewer`
- `tester`
- `summarizer`

每个 archetype 绑定：

- 默认 system prompt 片段
- 工具白名单
- 默认 completion contract
- 是否允许写代码
- 是否允许再派生子代理

执行机制：

- lead agent 发出 `spawn_subagent` tool call
- Rust runtime 创建独立 sub-run
- sub-run 继承 thread 的必要上下文，但使用裁剪后的局部上下文
- sub-run 结果只返回结构化摘要、风险、artifact refs、建议，不直接把全部上下文灌回主 agent
- reviewer / tester 可对 implementer 输出产物给出 gate decision

并发规则：

- 默认限制最大并发 sub-agents
- implementer 不得直接审批自己的高风险输出
- reviewer 和 tester 结果可作为是否进入应用阶段的 gate

### Memory / Replay / Artifact

当前 memory/session/replay 已有基础，应重构为 thread-first。

Memory：

- 短期记忆：thread messages + in-run summaries
- 中期记忆：thread scoped facts / preferences / repo facts
- 长期记忆：跨 thread 的稳定偏好与复用知识
- memory 提取必须异步化，不阻塞主 run 完成

Replay：

- 回放源从 `SessionManifest + timeline_events` 改为 `events.jsonl`
- replay 粒度提升到：
  - turn
  - tool call
  - approval decision
  - subagent lifecycle
  - artifact emission
- TUI replay 与未来 Web timeline 共用同一事件流协议

Artifact：

- 所有可交付物都进入 artifact index
- 包括文本摘要、diff、补丁、日志、截图、验证报告、handoff、最终输出文件
- artifact 必须有：
  - id
  - type
  - producer
  - thread_id / run_id
  - path 或 inline payload
  - mime/type
  - created_at

## 迁移步骤

### Phase 0：建立新内核边界

目标：

- 明确 `harness` / `app` / `clients` 分层
- 冻结旧 orchestrator 的新增功能开发
- 标记哪些现有模块可复用，哪些必须废弃

工作项：

- 补一版架构 ADR，确认新主模型为 `thread/run`
- 新建 `harness` 模块骨架和最小 domain types
- 为旧 `session`、`worker_result`、`memory` 写映射层
- 在编译和测试层面建立“harness 不反向依赖 app”的边界约束

验收：

- 可以编译出空壳 harness
- 旧 CLI 仍可运行
- 新 domain types 已落盘并有单测

### Phase 1：thread/run 持久化与事件流

目标：

- 把运行时状态从 `SessionManifest` 迁到 `ThreadStore + RunStore + EventStore`

工作项：

- 实现 thread/run/message/event JSON 持久化
- 定义 run lifecycle state machine
- 提供 event append 与 stream reader
- 让 replay 先能基于新 events 读取最小可视结果

验收：

- 能创建 thread、追加消息、创建 run、写事件、回放事件
- TUI 或 CLI 至少能显示 thread list 和 run status
- 旧 session 可导入为只读历史 thread

### Phase 2：Codex turn runtime

目标：

- 替换一次性 graph 生成，建立 lead agent turn loop

工作项：

- 实现 `AgentBackend::run_turn`
- 规范 Codex 返回格式，支持 assistant text、tool call、final、clarification
- 实现 middleware pipeline
- 接入 plan mode / chat mode / execution mode 三种 run profile

验收：

- 用户能对 thread 连续发消息
- 每轮可产生结构化 tool request
- clarification 能正确暂停和恢复

### Phase 3：工具、审批、sandbox 统一

目标：

- 把当前 apply/verify/worktree/shell 能力统一纳入 tool runtime

工作项：

- 建立 `ToolSpec`、`ToolCall`、`ToolOutcome`
- 接入 approval manager
- 实现本地 sandbox provider
- 把已有 `worktree.rs`、`verify.rs`、`apply.rs` 中的能力下沉为工具

验收：

- 只读工具、写文件工具、shell 工具、验证工具可统一执行
- 高风险动作能进入审批队列
- run 中断后能恢复未完成工具状态

### Phase 4：skills 与上下文工程

目标：

- 让 system prompt 和角色策略从代码内联迁移到 skills + runtime assembly

工作项：

- skill discovery / parser / loader
- built-in skills 首批迁移
- role set 到 archetype/skill 的映射器
- 上下文压缩、摘要、按需加载机制

验收：

- 同一 thread 可根据任务动态启用不同 skill
- 上下文长度可控
- architect/reviewer/tester 行为不再硬编码在 prompt 模板里

### Phase 5：sub-agents 与 gate

目标：

- 从静态 worker 切到动态 sub-agent delegation

工作项：

- sub-agent registry
- sub-run executor
- implementer / reviewer / tester archetype
- reviewer gate / tester gate 决策回流主 run
- worktree 子目录策略和冲突规约

验收：

- lead agent 可动态拉起至少两类 sub-agent
- reviewer/tester 可产出 gate decision
- 一个复杂任务可以并行拆解并汇总

### Phase 6：CLI/TUI 主路径切换

目标：

- 对用户暴露的新主入口从 session/worker 语义切到 thread/run 语义

工作项：

- CLI 重命名和兼容别名
- TUI 首页改为 threads / runs / approvals / artifacts / replay
- approval 队列可交互处理
- 历史详情改为 thread timeline 视图

验收：

- 用户不需要理解旧 orchestrator 也能完整跑通主流程
- TUI 可以查看 thread、消息、工具、审批、artifact、sub-agent 状态
- 旧命令只作为兼容入口存在

### Phase 7：Gateway/API/Web UI 预留或首版落地

目标：

- 为未来 DeerFlow 式产品化入口提供统一 API

工作项：

- 提供最小 HTTP Gateway
- 支持 thread/runs/messages/events/approvals/artifacts/skills/agents
- 先不追求完整前端，但 API contract 要稳定
- 若资源允许，补一版只读 Web timeline / thread inspector

验收：

- CLI/TUI 与 Gateway 共用同一 harness
- HTTP 接口可驱动核心 thread/run 生命周期
- API schema 有集成测试

### Phase 8：清理旧系统

目标：

- 移除旧 orchestrator 主路径，防止双系统长期并存

工作项：

- 删除旧 `ExecutionGraph` 主运行逻辑
- 保留 importer 和历史 replay 兼容
- 清理重复模型与过时命令
- 更新 README、架构文档、开发指南

验收：

- 新主路径覆盖原核心能力
- 旧路径只剩历史兼容，不再参与生产执行
- 文档与代码一致

## 测试与验收

### 单元测试

必须覆盖：

- thread/run/message/event 持久化
- middleware 顺序与状态变换
- tool schema 过滤
- approval 策略判定
- skill 解析与按需加载
- sub-agent 状态机
- memory 提取与 artifact index 写入

### 集成测试

必须覆盖：

- 单轮 chat + 只读工具调用
- clarification 中断与恢复
- 写文件工具触发审批
- implementer -> reviewer -> tester -> final 汇总链路
- replay 基于 events.jsonl 重放
- 旧 session 导入新 thread 的兼容路径
- Gateway API 与 CLI/TUI 共用同一存储目录

### 端到端场景

至少保留以下验收场景：

1. 仓库问答
   - 用户让 agent 解释代码结构
   - 只使用只读工具
   - 结果可回放、可查 artifact
2. 小型改码
   - implementer 修改文件
   - reviewer 判定通过
   - tester 跑最小验证
   - 最终输出修改摘要与验证报告
3. 多 sub-agent 复杂任务
   - 主 agent 拆解为多个并行子任务
   - 汇总为最终交付
4. 人工审批场景
   - shell / git / patch 动作进入待审批
   - 用户确认后继续执行
5. 长线程续聊
   - 同一 thread 多轮追加消息
   - memory 与摘要持续生效

### 非功能性验收

- 新运行时在无 Gateway 的本地 CLI/TUI 模式下可独立运行
- 任意 run 中断后，重启进程可恢复 thread 历史与未完成审批
- event log 足够完整，能支持 TUI replay 和未来 Web timeline
- 高风险工具无隐式执行路径

## 默认决策与假设

- 宿主语言固定为 Rust，不引入 Python runtime 作为内核依赖
- agent backend 固定为 Codex CLI，不做多 provider 抽象优先级竞争
- 重构以“新 runtime 取代旧 orchestrator”为终局，不长期维护双主路径
- 首版 sandbox 以本地 workspace 为主，Docker/K8s 只保留扩展点
- 首版 Web/Gateway 以 API 稳定为目标，可晚于 CLI/TUI 主路径切换
- 现有 `session`、`memory`、`worktree`、`verify` 中可复用的落盘与工具能力尽量复用，但其旧对象模型不保留为核心抽象
- `ExecutionGraph` 后续只用于 plan mode 的可视计划表达，不再承担真实调度职责
- 当前文档以“可直接指导多轮演进”为目标，后续每阶段完成后应同步更新阶段状态和验收结果
