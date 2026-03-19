# codex-forge

`codex-forge` 是一个用 Rust 编写的多 Agent Codex 指挥台 CLI。  
它不是“把一个任务丢给多个 Agent 同时开跑”这么简单，而是试图把 AI 编码从**单 Agent 串行助手**升级成一条更接近真实工程协作的闭环：

- 先把用户任务整理成**面向用户可读的 todo 清单**
- 再派生内部 **ExecutionGraph**
- 用多个 worker 在隔离 worktree 中并行执行
- 通过 reviewer gate 控制自动收敛风险
- 在目标仓库按 **todo** 顺序完成验证与本地 commit
- 为整条链路保留完整 session 工件，便于 replay、审查与复盘

这个项目最重要的价值，不是“多开几个窗口”，而是把以下几件事情串成一个稳定工作流：

1. **规划**
2. **并行执行**
3. **审阅与收敛**
4. **验证与提交**
5. **可回放的过程留痕**

对于公司内部黑客松来说，`codex-forge` 的亮点在于：它不只是一个概念 demo，而是一套已经把**协作边界、自动应用风险、验证闭环、工件沉淀**都考虑进去的多 Agent 编码指挥台。

## 为什么做这个项目

大多数 AI 编码工具默认还是**单 Agent 工作流**：

- 任务大了以后，上下文会越来越乱
- 一个 Agent 同时兼任规划、实现、验证、审阅，角色职责混在一起
- 多轮修改后，哪些是计划、哪些是实现、哪些是验证、哪些是风险提示，边界不清
- 即使产出了代码，最后怎么安全集成、怎么验证、怎么留下可回放证据，常常还要靠人手工补齐

现实工程协作不是这样的。真实团队里通常会有：

- 做拆解和方案收束的人
- 真正写代码的人
- 负责验证的人
- 负责集成放行的人

`codex-forge` 想解决的，就是**AI 编码工作流里“角色混杂、过程不可控、收敛不可证、集成不安全”**的问题。

换句话说，我们开发这个项目，是为了把 AI 编码从：

> “一个大模型对着整个任务一路往前冲”

变成：

> “多个有边界的角色，在同一条可追踪、可验证、可收敛的流水线上协作”

## 核心亮点

### 1. 显式 ExecutionGraph，而不是隐式脑补

`codex-forge` 不把多 Agent 协作理解成“开多个 worker 就行”，而是先把任务转成显式执行图：

- 每个节点有明确角色
- 每个节点有显式依赖
- 每个节点有输入工件与输出工件
- 每个节点有完成标准与聚焦点

这样做的好处是：系统知道谁该先做、谁可以并行、谁必须等、谁负责收口，而不是靠 prompt 里模糊协商。

### 2. 用户 todo 与内部执行图分离

`plan` 会先生成**面向用户**的 todo 清单，再派生**面向系统**的执行图。

这意味着：

- 用户能看到任务是如何被拆解的
- 系统能按内部依赖高效调度
- 最终提交与验证可以回到用户关心的 todo 维度，而不是只停留在“哪个 agent 改了哪个文件”

### 3. Worktree 隔离并行，减少互相踩踏

每个 worker 默认在独立 `git worktree` 中工作。  
这让并行协作更接近真实工程团队，而不是所有 Agent 混在同一个工作区里相互污染：

- worker 可以独立修改
- patch 可以独立提取
- handoff 可以独立沉淀
- integration 阶段可以更可控地决定如何收敛

### 4. reviewer gate 不是装饰，而是自动应用前的安全阀

很多 AI 工作流的“review”只是多跑一个模型点评一下。  
`codex-forge` 的 reviewer 是真正参与自动收敛决策的 gate：

- `allow_full`
- `allow_partial`
- `block`

只有 reviewer 明确放行，系统才进入自动应用路径。  
这让“多 Agent 自动化”不至于失控，也让黑客松展示时的系统设计更完整：**不是只有并行执行，还有可解释的安全收口机制**。

### 5. todo 级验证与本地 commit

`run` 在 `auto-safe` 模式下，不是一次性把所有 patch 粗暴合并后结束，而是会按**用户 todo 顺序**推进：

1. 先收敛 patch
2. 再做 todo 级验证
3. 通过后再创建本地 commit
4. 记录 commit hash

这让最终结果更接近真实研发流程，也让回放和复盘更有意义。

### 6. 全链路可回放、可审计、可复盘

每次 session 都会保存：

- todo 清单
- execution graph
- worker prompt
- worker events
- handoff
- patch 快照
- apply 结果
- verification report
- summary

所以 `codex-forge` 不是“跑完即失忆”的黑盒，而是一个可回放的协作系统。

## 适合解决什么问题

`codex-forge` 最适合以下任务：

- 中等复杂度以上的功能开发
- 需要拆解、实现、验证、审阅分工的工程任务
- 需要多个 Agent 并行推进、但又不能失控的场景
- 需要保留完整工件、方便复盘和展示的项目
- 需要把“自动写代码”升级成“自动协作 + 自动收敛”的实验型系统

典型例子：

- “为这个已有项目补一个登录系统”
- “给 CLI 增加子命令与配置校验”
- “重构一个模块，但要控制回归风险”
- “修复一个跨模块 bug，并补最小可信验证”

## 不适合的场景

当前版本并不试图覆盖所有情况，以下场景并不是它的强项：

- 极小任务，单 Agent 已足够高效
- 需要远程分布式调度的大规模编排
- patch 冲突极其复杂、必须深度人工合并的任务
- 高依赖外部系统、环境极不稳定、验证成本很高的任务

也就是说，`codex-forge` 的目标不是替代所有编码方式，而是把**“多 Agent 编码协作”做成一个可信、可演示、可持续迭代的产品雏形**。

## 与其他项目相比的优势

这里的对比对象主要是：

- 单 Agent Codex CLI 工作流
- Claude Code / Aider / OpenCode 一类通用 AI 编码 CLI
- 以及只强调“多开 Agent”，但没有完整收敛闭环的工具

### 1. 不只会执行，还会编排

很多工具擅长“把 prompt 执行出来”，但 `codex-forge` 更强调：

- 先拆解
- 再调度
- 再交接
- 再审阅
- 再收敛

它的竞争点不只是执行能力，而是**执行组织能力**。

### 2. 不只会产出代码，还会控制集成风险

许多 AI CLI 最大的问题不是“写不出代码”，而是：

- 写出来的东西怎么安全落地
- 多次产出如何收敛
- 风险谁来兜底

`codex-forge` 用 reviewer gate、trust report、bundle 降级、todo 级验证这些机制，把“能写”升级成“能安全收口”。

### 3. 不只关注结果，还关注过程留痕

大多数工具对最终结果友好，但对中间过程沉淀不够。  
`codex-forge` 的 session 工件体系，让它更适合：

- 复盘
- 演示
- 人工审查
- 审计
- 黑客松展示

这在内部评审中很重要，因为评委不仅想看“跑出来了什么”，还想看“系统设计是否成熟”。

### 4. 更接近真实工程团队，而不是单模型人格分裂

很多单 Agent 工作流本质上是让一个模型在同一次会话里同时扮演 PM、架构师、程序员、测试和 reviewer。  
`codex-forge` 选择的路线是：

- 角色显式化
- 边界显式化
- 工件显式化
- 决策显式化

这让整个系统更像一个最小化工程团队，而不是一个“万能但不可控的单体 Agent”。

## 架构总览

从系统视角看，`codex-forge` 的主流程可以概括为：

```text
用户任务
  -> 生成用户 todo
  -> 派生 ExecutionGraph
  -> worker worktree 并行执行
  -> 产出 patch / handoff / events
  -> reviewer 做最终 gate
  -> integration 做 auto-safe apply 或 bundle 降级
  -> todo 级验证与本地 commit
  -> 保存 summary 与 replay 工件
```

### 核心模块

- `src/commander.rs`
  - 生成用户可读 todo
  - 派生内部 ExecutionGraph
  - 负责 fallback 规划与最终 summary 收敛

- `src/orchestrator.rs`
  - 协调整个 session 生命周期
  - 串起 plan、run、调度、integration、summary

- `src/codex.rs`
  - 对接 `codex exec`
  - 解析混合事件流
  - 处理重试、结构化输出与 handoff 提取

- `src/worktree.rs`
  - 管理独立 `git worktree`
  - 捕获 patch
  - 为 apply / commit 提供 Git 辅助能力

- `src/apply.rs`
  - 汇总 worker patch
  - 执行 `auto-safe` 收敛
  - 根据 reviewer gate 决定应用 / 降级 bundle
  - 负责 todo 级验证与本地 commit

- `src/verify.rs`
  - 执行 integration / final 验证
  - 区分通过、失败、环境阻塞

- `src/resources.rs`
  - 按层加载 `.roles` 与 `.rules`
  - 支持 forge / target / home 三层覆盖

- `src/workspace.rs`
  - 管理目标目录解析、目录记忆与 Git 预处理

- `src/replay.rs`
  - 回放 timeline 与 session 工件

## 角色设计

`codex-forge` 当前采用四核心角色，不追求角色数量，而追求角色边界清晰。

### `architect`

负责把用户任务变成稳定可执行的方案：

- 明确边界
- 锁定接口
- 定义依赖
- 提前识别风险
- 降低并行节点共享修改面

### `implementer`

负责把目标落成最小充分实现：

- 复用现有模式
- 修根因
- 控制改动半径
- 交付可集成结果

### `tester`

负责证明结果可信：

- 设计验证路径
- 补关键失败样例
- 区分“已验证 / 未验证 / 环境阻塞”

### `reviewer`

负责最终集成放行：

- 判断是否安全自动应用
- 检查范围漂移
- 评估验证可信度
- 做 `allow_full / allow_partial / block` 决策

## 角色集合（Role Sets）

当前内置四组 role set，用于不同任务节奏：

### `default`

完整主链路：

- `architect`
- `implementer`
- `tester`
- `reviewer`

适合大多数标准功能开发、复杂 bug 修复、黑客松主流程演示。

### `fast-path`

快速链路：

- `implementer`
- `reviewer`

适合小范围低风险修复、快速演示和高频迭代。

### `delivery`

偏交付链路：

- `architect`
- `implementer`
- `reviewer`

适合需求已经相对清晰、重点在实现与收口的任务。

### `hardening`

偏稳健链路：

- `implementer`
- `tester`
- `reviewer`

适合回归治理、上线前加固、验证补强与风险收口。

## 资源加载机制

V3 不再内置 `.skills/` 文本资源，而是按三层优先级加载 `.roles`、`.rules`：

1. `codex-forge` 仓库根目录
2. 目标项目根目录
3. 用户家目录 `~`

同名资源始终以更高优先级版本覆盖更低优先级版本。

最小目录结构如下：

```text
.roles/
  architect.toml
  implementer.toml
  reviewer.toml
  tester.toml
  sets.toml
.rules/
  global.md
  reviewer.md
```

其中：

- `.roles/*.toml`：角色定义
- `.roles/sets.toml`：角色集合定义
- `.rules/global.md`：所有 worker 通用规则
- `.rules/reviewer.md`：仅 reviewer 使用的 gate 规则

角色里的 `skills = [...]` 只声明**可使用的外部 Codex skill 名称**。  
实际 skill 目录位于：

```text
~/.codex/skills/<skill-name>/SKILL.md
```

这意味着：

- `codex-forge` 负责角色与规则编排
- Codex 自己负责外部 skills 的挂载与触发
- 仓库资源层不再重复实现一套本地 `.skills` 注入机制

## 快速开始

### 1. 查看当前可用角色

```bash
cargo run -- agents list
```

### 2. 先规划

```bash
cargo run -- plan "我现在要创建一个简单的 web 博客，给我规划" --ui minimal
```

### 3. 再执行

```bash
cargo run -- run "我现在要创建一个简单的 web 博客，给我规划" --workers 4 --ui rich
```

### 4. 指定目标目录执行

```bash
cargo run -- run "为这个项目补登录功能" --target-dir /path/to/project --ui minimal
```

第一次显式传入 `--target-dir` 后，后续 `plan`、`run`、`doctor`、`config validate`、`replay` 不传时会默认复用它。

### 5. 回放最近一次 session

```bash
cargo run -- replay --ui minimal
```

## 如何使用

### 典型工作流

1. 准备一个目标仓库
2. 根据任务规模选择合适的 `role_set`
3. 先运行 `plan`
4. 检查 todo 清单与 execution graph
5. 再运行 `run`
6. 查看 apply 结果、verification report 和 summary
7. 必要时用 `replay` 回放过程

### 常用命令

```bash
cargo run -- agents list
cargo run -- plan "创建一个简单博客" --ui minimal
cargo run -- run "创建一个简单博客" --workers 4 --ui rich
cargo run -- run "创建一个简单博客" --apply-mode auto-safe --max-retries 2
cargo run -- doctor
cargo run -- config validate
cargo run -- replay --ui minimal
```

### 常用参数

- `--config <path>`：显式指定项目配置文件
- `--workers <n>`：并发 worker 数量
- `--role-set <name>`：角色集合标识，默认 `default`
- `--model <name>`：统一指定 Codex model
- `--apply-mode auto-safe|bundle|none`：控制是否自动收敛并落地
- `--max-retries <n>`：worker / structured Codex 调用最大重试次数
- `--fail-fast`：节点失败后尽快停止调度
- `--ui rich|minimal`：终端展示模式
- `--target-dir <path>`：目标仓库路径；显式传入后会被记住
- `--cleanup-success`：成功完成后自动清理 worker worktree

## 默认自动化行为

### 工作目录

- 优先使用显式传入的 `--target-dir`
- 未传时优先复用最近一次记住的目录
- 如果没有项目记忆，则回退到当前目录

### Git 预处理

当目标目录不是 Git 仓库时：

1. 自动执行 `git init`
2. 若仓库级 `user.name` / `user.email` 缺失，则自动补齐本地配置
3. 后续 session、worktree、patch、commit 全部围绕该仓库运行

### 自动提交策略

在 `auto-safe` 下：

1. 先生成用户 todo 和内部执行图
2. worker 在独立 worktree 中执行并产出 patch / handoff
3. integration 阶段先做 patch 自动收敛
4. 再按**用户 todo**顺序执行：
   - todo 级验证
   - 本地 `git commit`
   - 记录 commit hash
5. 绝不自动 `push`

### 失败收口

- reviewer 阻止应用：自动降级为 `bundle`
- patch 冲突 / 范围漂移 / 不安全变更：自动降级为 `bundle`
- todo 验证失败：停止后续 todo 提交，保留现场和工件
- 已成功创建的本地 commit 不会自动回滚

## Session 工件与可回放性

每次执行都会在目标仓库下生成：

```text
.codex-forge/sessions/<session-id>/
```

其中常见工件包括：

- `manifest.json`：完整 session 元信息
- `timeline.jsonl`：全局事件流，可用于 replay
- `commander/plan-todo.json` / `commander/plan-todo.md`：面向用户的计划清单
- `commander/todo-state.json`：用户 todo 的实时状态、完成节点与 commit hash
- `commander/execution-graph.json`：显式执行图
- `workers/<agent-id>/prompt.md`：下发给 worker 的 prompt
- `workers/<agent-id>/events.jsonl`：worker 原始事件
- `workers/<agent-id>/final.md`：worker 最终输出
- `workers/<agent-id>/handoff.json`：结构化交接工件
- `workers/<agent-id>/changes.patch`：差异补丁快照
- `artifact-manifest.json`：工件索引
- `integration/apply-plan.json`：候选 patch 应用顺序
- `integration/apply-result.json`：自动应用结果与冲突信息
- `integration/verification-report.json`：worker / integration / final 验证报告
- `summary.json` / `summary.md`：最终收敛摘要

`.codex-forge` 采用**按需生成**策略；没有实际内容的空目录会在流程结束后自动清理。

## 配置示例

```toml
[defaults]
workers = 4
role_set = "default"
apply_mode = "auto-safe"
max_retries = 2
verification_commands = [
  "cargo fmt --check",
  "cargo clippy --all-targets --all-features -- -D warnings",
  "cargo test",
]
```

## 开发与验证

如果目标仓库没有显式配置 `verification_commands`，当前 Rust 项目的默认验证命令为：

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## 当前限制 / 边界

- 当前仍然是本地单机 CLI，不做远程分布式 worker
- `auto-safe` 不会强制解决真实冲突；一旦 patch 无法应用，会自动降级为 `bundle`
- 资源体系支持项目级覆盖与用户级覆盖，但默认以仓库内通用资源包为基础
- integration / final 验证命令依赖本地环境；如外部工具缺失，可先用 `doctor` 预检
- 当前 reviewer gate 仍然只做 apply 前决策，不负责自动返工闭环

## 为什么它适合黑客松展示

从黑客松评委视角看，`codex-forge` 的亮点不只是“做了一个 AI 工具”，而是：

- 它有明确要解决的真实协作问题
- 它的系统设计能讲清楚
- 它的自动化路径有安全边界
- 它的结果不是黑盒，过程可以 replay
- 它已经具备进一步产品化和工程化的可能性

也就是说，`codex-forge` 展示的不是某个单点功能，而是一条完整的、多 Agent 协作产品方向。

## FAQ

### 为什么不直接让一个 Agent 全做完？

因为单 Agent 在复杂任务里容易角色混杂、上下文污染、验证不可信，最后集成风险高。  
`codex-forge` 的重点是把协作边界和收敛机制显式化。

### 为什么默认不自动 `git push`？

因为自动推远端属于高风险动作。  
当前版本的定位是自动规划、自动执行、自动验证、自动本地提交，但保留最终远端发布的人类决策权。

### 为什么需要 reviewer？

因为并行执行不是难点，**安全收敛**才是难点。  
reviewer 的存在，是为了让系统在自动应用前有明确 gate。

### 什么情况下应该用 `bundle`？

当 reviewer 阻止放行、patch 冲突明显、范围漂移较大、自动应用可信度不足时，`bundle` 是更安全的降级策略。
