# codex-forge

`codex-forge` 是一个用 Rust 编写的本地多 backend harness。它面向“围绕单个代码仓库持续协作”的场景，把一次性问答扩展成可持久化的 `thread / run / task graph` 执行系统，并提供 Docker 沙箱、审批、artifact、memory、skill 发现以及 CLI/TUI 入口。

当前默认定位不是 Web 服务，也不是远程 orchestrator，而是一个本地可运行、可回放、可恢复的仓库内代理壳层。

当前有两条不同的执行线：

- `codex`
  - 走 `Codex 自主执行`。harness 负责 run 生命周期、沙箱、日志和状态持久化，任务本身直接交给 Codex CLI 自主完成。
- `openai_compatible`
  - 走 `编排式执行`。harness 负责 `task graph / planner / generator / evaluator / approval / checkpoint` 这套长期任务状态机。

## 当前能力概览

- `thread` 级对话与状态持久化
- `run` 级执行实例、事件流和结果回放
- `task graph / task node` 级任务拆分、节点查看与节点重试
- `execution contract / progress ledger / evaluation / session bootstrap` 长任务交付闭环
- 每个 `run` 独立 Docker 沙箱
- `approval` 审批队列
- `artifact` 产物查看
- 线程级 `working memory / project memory`
- 本地 skill 发现与 `SKILL.md` 读取
- CLI 与全屏 TUI
- 两类 backend：
  - `codex`
  - `openai_compatible`

## 核心概念

- `thread`
  - 围绕同一个仓库长期协作的容器，保存消息、run 历史、审批、artifact、memory、contract、progress。
- `run`
  - 一次具体执行，绑定一个独立沙箱、日志和输出文件；如果是 `openai_compatible`，还会绑定任务图与节点状态。
- `task graph`
  - 当前 run 的任务图。节点支持查看、恢复、取消和按节点重试。
- `execution contract`
  - 对长任务目标、非目标、约束、feature 切片和验收条件的结构化约束。
- `progress ledger`
  - 记录当前阶段、已完成 feature、当前 feature、决策、失败原因和下一步。
- `evaluation`
  - evaluator 对 feature 的通过/失败判断。
- `session bootstrap`
  - 给后续 run 接手时使用的最小上下文摘要。
- `memory`
  - `working` 用于短期上下文滚动，`project` 用于较稳定的项目事实。
- `artifact`
  - 工具结果、计划快照、contract/progress/evaluation 快照、session bootstrap 等持久化输出。

## 运行前提

至少需要以下环境：

- Rust 工具链
- Docker
- 一种可用 backend
  - `codex`：本机 `PATH` 中需要有 `codex` 命令
  - `openai_compatible`：需要可访问的 Chat Completions 兼容接口

仓库内开发时可以直接用：

```bash
cargo run -- --help
```

如果希望像普通命令一样使用：

```bash
cargo install --path .
codex-forge --help
```

下文命令示例统一使用 `codex-forge`；在仓库根目录下可等价替换为 `cargo run --`。

## 快速开始

### 1. 构建沙箱镜像

```bash
./scripts/build_sandbox_image.sh
```

默认镜像名：

```text
codex-forge-sandbox:latest
```

脚本会优先尝试以下基础镜像：

```text
swr.cn-north-4.myhuaweicloud.com/ddn-k8s/docker.io/library/debian:bookworm-slim
-> docker.m.daocloud.io/library/debian:bookworm-slim
-> debian:bookworm-slim
```

常见覆盖方式：

```bash
IMAGE_TAG=my-sandbox:latest ./scripts/build_sandbox_image.sh
SANDBOX_BASE_IMAGE=claude-sdk:latest ./scripts/build_sandbox_image.sh
SANDBOX_PLATFORM=linux/arm64 ./scripts/build_sandbox_image.sh
EXTRA_APT_PACKAGES="nodejs npm" ./scripts/build_sandbox_image.sh
```

默认沙箱镜像内置：

- `sh` / `bash`
- `git`
- `ripgrep`
- `python3`
- `curl`
- `jq`
- `make`

### 2. 初始化配置

项目级配置：

```bash
codex-forge config init
codex-forge config show
codex-forge config validate
```

全局 backend 配置：

```bash
codex-forge config init --global
codex-forge config show --global
codex-forge config validate --global
```

快捷切换 backend provider：

```bash
codex-forge config set --global backend.provider codex
codex-forge config set --global backend.provider openai_compatible
```

### 3. 直接进入 TUI 或发起一次 chat

不带子命令启动时会直接进入 TUI：

```bash
codex-forge
```

也可以显式进入：

```bash
codex-forge tui
```

或者直接对当前仓库发起一次执行：

```bash
codex-forge chat "请总结这个仓库目前的用途"
```

TUI 内默认使用 `Codex` 作为执行模式；在浏览模式下按 `m` 可在 `Codex` 和 `OpenAI Compatible` 间切换。切换会立即生效：当前视图和后续新 run 会直接切到对应 mode 的独立命名空间，不复用另一种 mode 的 thread / run / memory / skill。

## 命令行用法

### `--target-dir`

多数命令支持 `--target-dir`。传入后会严格使用该目录，不会再自动上卷到 Git 根目录。

```bash
codex-forge thread list --target-dir ./apps/demo
```

### Thread

```bash
codex-forge thread new --title "仓库梳理"
codex-forge thread list
codex-forge thread show <thread-id>
```

### Chat

不传 `--thread` 时会自动创建新 thread：

```bash
codex-forge chat "请总结这个仓库目前的用途"
```

指定已有 thread：

```bash
codex-forge chat --thread <thread-id> "请修改 README 中的快速开始"
```

覆盖 model 或 thinking mode：

```bash
codex-forge chat --thread <thread-id> --model gpt-5-codex --thinking-mode hard-think "继续完成剩余任务"
```

### Run

```bash
codex-forge run list --thread <thread-id>
codex-forge run show --thread <thread-id> <run-id>
codex-forge run resume --thread <thread-id> <run-id>
codex-forge run confirm-plan --thread <thread-id> <run-id> <task-node-id>
codex-forge run cancel --thread <thread-id> <run-id>
codex-forge run retry-node --thread <thread-id> --run <run-id> <task-node-id>
codex-forge run node --thread <thread-id> --run <run-id> <task-node-id>
```

`run show` 当前会输出：

- run 基本状态
- backend / model / thinking mode
- execution mode
- task graph 与 success criteria（仅编排式执行）
- tool call / artifact / subagent 数量
- contract / progress 摘要（仅编排式执行）
- 最新 evaluation（仅编排式执行）
- 当前激活节点
- 如处于等待态，直接给出下一步恢复命令
- 沙箱信息

`codex` 默认走自主执行，不使用 planner/generator/evaluator，也不受 `runtime.max_turns` / `runtime.max_generator_turns` 限制。

`openai_compatible` 默认走长期任务编排；如果显式开启 `runtime.interactive_plan_confirmation = true`，则会停在计划确认节点，此时可用 `run confirm-plan` 或 TUI 中的 Enter 继续。

### Replay

```bash
codex-forge replay --thread <thread-id> <run-id>
```

### Approval

```bash
codex-forge approval list
codex-forge approval list --thread <thread-id>
codex-forge approval approve --thread <thread-id> <approval-id>
codex-forge approval deny --thread <thread-id> <approval-id>
```

### Artifact

```bash
codex-forge artifact list
codex-forge artifact list --thread <thread-id>
codex-forge artifact list --thread <thread-id> --run <run-id>
codex-forge artifact show <artifact-id>
codex-forge artifact show <artifact-id> --thread <thread-id>
```

### Config

```bash
codex-forge config init
codex-forge config show
codex-forge config validate
codex-forge config init --global
codex-forge config show --global
codex-forge config validate --global
codex-forge config set --global backend.provider codex
```

## 配置说明

### 项目级配置：`codex-forge.toml`

默认结构如下：

```toml
[sandbox]
docker_image = "codex-forge-sandbox:latest"
mount_strategy = "direct_rw"
privileged = true
run_as_root = true
repair_owner_on_exit = true

[runtime]
max_turns = 6
max_generator_turns = 16
max_feature_retries = 2
max_evaluator_loops = 3
bootstrap_message_limit = 8
enable_long_running_delivery = true
interactive_plan_confirmation = false
require_tool_approval = false
auto_approve_readonly = true
```

字段含义：

- `sandbox.docker_image`
  - run 使用的 Docker 镜像
- `sandbox.mount_strategy`
  - 当前默认 `direct_rw`
- `sandbox.privileged`
  - 是否以特权模式启动容器
- `sandbox.run_as_root`
  - 是否在容器中使用 root
- `sandbox.repair_owner_on_exit`
  - 退出时是否修复宿主文件 owner
- `runtime.max_turns`
  - 编排式执行主循环最大轮次，仅对 `openai_compatible` 生效
- `runtime.max_generator_turns`
  - generator 预算，仅对 `openai_compatible` 生效
- `runtime.max_feature_retries`
  - 单个 feature 重试上限，仅对 `openai_compatible` 生效
- `runtime.max_evaluator_loops`
  - evaluator 最大循环次数，仅对 `openai_compatible` 生效
- `runtime.bootstrap_message_limit`
  - bootstrap 汇总时保留的消息数
- `runtime.enable_long_running_delivery`
  - 是否启用长任务交付路径，仅对 `openai_compatible` 生效
- `runtime.interactive_plan_confirmation`
  - 是否在计划检查后等待人工确认；默认关闭，计划生成后自动继续执行，仅对 `openai_compatible` 生效
- `runtime.require_tool_approval`
  - 是否显式开启人工审批；默认关闭；当前主要用于编排式执行
- `runtime.auto_approve_readonly`
  - 开启人工审批后，是否自动放行只读工具；当前主要用于编排式执行

### 全局配置：`~/.codex-forge/config.toml`

也支持通过 `CODEX_FORGE_HOME/config.toml` 覆盖位置。

`openai_compatible` 示例：

```toml
[backend]
provider = "openai_compatible"
key = "sk-..."
base_url = "https://example.com/v1"
model = "gpt-4o-mini"
turn_timeout_secs = 600
```

`codex` 示例：

```toml
[backend]
provider = "codex"
model = "gpt-5-codex"
turn_timeout_secs = 600
```

说明：

- `config show --global` 会对 `backend.key` 做脱敏显示
- `config set` 目前只支持设置 `backend.provider`
- `openai_compatible` 必须提供 `key`、`base_url`、`model`

## 当前内置工具

当前 runtime 会向 backend 暴露以下工具：

- `list_tree`
- `read_file`
- `search_files`
- `apply_patch`
- `run_shell`
- `write_file`
- `list_artifacts`
- `read_artifact`
- `inspect_run`
- `create_plan_snapshot`
- `read_contract`
- `write_contract`
- `read_progress`
- `update_progress`
- `record_evaluation`
- `create_session_bootstrap`
- `read_memory`
- `remember_memory`
- `list_skills`
- `read_skill`

默认情况下，工具会直接在沙箱/工作区内执行；如果显式开启 `runtime.require_tool_approval = true`，以下写操作会进入审批：

- `apply_patch`
- `run_shell`
- `write_file`

## Skill 与 Memory

### Skill 发现

默认按当前 backend mode 扫描 `SKILL.md`：

- `codex` -> `~/.codex/skills`
- `openai_compatible` -> `~/.agents/skills`

### Memory 分层

- `working memory`
  - 短期上下文，最多保留最近 32 条
- `project memory`
  - 面向较稳定项目事实；相同内容会去重

## 持久化目录

所有运行状态默认写在目标仓库下的 `.codex-forge/`，并按 backend mode 隔离：

```text
.codex-forge/
  modes/
    codex/
      threads/{thread_id}/
        ...
    openai_compatible/
      threads/{thread_id}/
        ...
```

skill 发现同样按 mode 隔离：

- `codex` 只读取 `~/.codex/skills`
- `openai_compatible` 只读取 `~/.agents/skills`

## 运行时主路径

当前主路径可以概括为：

```text
用户消息
  -> thread
  -> run
  -> Docker 沙箱
  -> backend turn
  -> task graph / task node
  -> tools / approvals / artifacts
  -> contract / progress / evaluation / bootstrap
  -> 最终回复或等待恢复
```

backend 优先返回结构化 JSON envelope；如果返回普通文本，runtime 会把它当作最终回复处理。

## 仓库结构

```text
src/
  cli/                 # clap 命令定义
  commands/            # CLI 子命令入口与格式化
  harness/
    backend/           # codex / openai_compatible backend 与 envelope 解析
    runtime/           # chat、engine、subagent、恢复逻辑
    store/             # thread/run/approval/artifact/memory 持久化
    tools/             # 工具注册、归一化与执行
    sandbox.rs         # Docker 沙箱
    skills.rs          # 本地 skill 发现
    types.rs           # 领域模型
  tui/                 # 全屏 TUI
  config.rs            # 项目级与全局配置
  codex.rs             # Codex CLI 调用封装
  workspace.rs         # target-dir 解析
tests/
  thread_run_flow.rs
  approval_and_artifact.rs
  config_and_alias.rs
scripts/
  build_sandbox_image.sh
  tui_smoke_real_pty.py
docs/
  release-checklist.md
```

## TUI

```bash
codex-forge tui
```

当前快捷键：

- `q` 退出
- `j/k` 或上下键切换 thread
- `i` 进入输入模式
- `Enter` 发送消息
- `Esc` 返回浏览模式
- `a` 通过当前第一个待审批项
- `x` 拒绝当前第一个待审批项
- `s` 恢复当前等待中的 run
- `n` 新建 thread
- `r` 刷新
- `Tab` 切换消息 / 运行 / 审批 / 产物 / 事件面板

用于真实 PTY 冒烟验证：

```bash
python3 scripts/tui_smoke_real_pty.py
```

## 开发与验证

本地检查：

```bash
cargo fmt --check
cargo check
cargo test -- --nocapture
```

当前测试已覆盖的主线包括：

- thread 创建、列出、展示
- chat 主路径
- run show / replay
- approval 流程
- artifact 流程
- config 命令
- backend envelope 解析
- store roundtrip
- contract / progress / evaluation roundtrip
- target-dir 严格解析
- memory 持久化

## 当前边界

当前版本已经形成完整本地闭环，但仍明确不包含：

- HTTP API / Gateway
- Web UI
- 跨仓库共享 memory
- 多 provider 的完整生产级适配层
- 分布式调度或远程执行编排
