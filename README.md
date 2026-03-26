# codex-forge

`codex-forge` 现在是一个 **Rust 编写的本地多 backend harness 产品**。  
当前主路径已经演进为 long-running harness：保留 `thread / message / run / event` 外壳，同时在运行时内核中引入 **execution contract / progress ledger / evaluator gating / session bootstrap**，并以 **Docker run 级沙箱**、**结构化 backend 协议**、**approval**、**artifact**、**CLI/TUI** 为核心能力。

当前支持两类 backend：

- `codex`
  - 继续通过本地 `codex` CLI 执行
- `openai_compatible`
  - 通过 HTTP 调用 OpenAI-compatible `Chat Completions` 接口执行

当前仓库不再保留旧 `orchestrator / session / plan-run-continue / Brain Agent` 的兼容实现。

## 当前架构

项目的主路径是一个聊天优先的本地闭环：

```text
用户消息
  -> thread
  -> run
  -> Docker 沙箱
  -> backend turn
  -> planner / generator / evaluator
  -> tool calls / approvals
  -> contract / progress / evaluation / bootstrap
  -> artifacts / replay events
  -> 最终回复
```

关键组件：

- `thread`
  - 长期协作容器
- `message`
  - 用户、assistant、tool、summary 消息历史
- `run`
  - 某次执行实例，绑定一个独立 Docker 沙箱
- `approval`
  - 高风险工具调用的人类确认队列
- `artifact`
  - 工具结果、日志、文件产物
- `replay`
  - 基于事件流的回放
- `execution contract`
  - 面向长期任务的结构化执行契约
- `progress ledger`
  - 记录已完成 feature、当前 feature、决策与下一步
- `evaluation`
  - evaluator 对单个 feature 的通过/失败结论
- `session bootstrap`
  - 供下一次 run 接手的最小上下文摘要

## 已实现能力

- `thread new/list/show`
- `chat` 主入口
- `run list/show`
- `replay`
- `approval list/approve/deny`
- `artifact list/show`
- `config init/show/validate`
- 全屏 TUI
- 每个 run 独立 Docker 容器沙箱
- 结构化 backend envelope
- 内置工具：
  - `list_tree`
  - `read_file`
  - `search_files`
  - `run_shell`
  - `write_file`
  - `read_contract` / `write_contract`
  - `read_progress` / `update_progress`
  - `record_evaluation`
  - `create_session_bootstrap`
- 基础 sub-agent 调度：
  - `planner`
  - `generator`
  - `evaluator`

## 目录结构

```text
src/
  cli/
    mod.rs          # 命令定义
  commands/
    entry.rs        # CLI 入口分发
    format.rs       # CLI/TUI 共用展示格式化
    ...             # 各子命令处理
  config.rs         # 项目配置
  codex.rs          # Codex CLI 调用封装
  model.rs          # 通用枚举和最小模型
  tui/
    mod.rs          # TUI 入口
    app.rs          # 状态对象
    input.rs        # 按键处理
    data.rs         # 数据刷新与动作
    render.rs       # 界面渲染
  workspace.rs      # 目标仓库定位
  harness/
    mod.rs
    backend/
      mod.rs
      prompt.rs
      parser.rs
    runtime/
      mod.rs
      chat.rs
      engine.rs
      subagent.rs
    sandbox.rs      # Docker 沙箱
    store/
      mod.rs
      threads.rs
      runs.rs
      artifacts.rs
      jsonl.rs
      ids.rs
    tools/
      mod.rs
      catalog.rs
      executor.rs
      fs_tools.rs
      search.rs
      shell.rs
    types.rs        # 领域模型
tests/
  support/
    mod.rs
  thread_run_flow.rs
  approval_and_artifact.rs
  config_and_alias.rs
```

运行时持久化目录：

```text
.codex-forge/
  threads/{thread_id}/
    thread.json
    messages.jsonl
    thread-events.jsonl
    approvals/
      pending.jsonl
      resolved.jsonl
    artifacts/
      index.jsonl
    contract.json
    progress.json
    session-bootstrap.md
    runs/{run_id}/
      run.json
      events.jsonl
      tool-calls.jsonl
      approvals.jsonl
      artifacts.jsonl
      subagents.jsonl
      evaluations.jsonl
      assistant.md
      codex.log
      session-bootstrap.md
      sandbox/
```

## CLI 用法

### 配置

```bash
codex-forge config init
codex-forge config show
codex-forge config validate
codex-forge config init --global
codex-forge config show --global
codex-forge config validate --global
```

仓库级 `codex-forge.toml` 包含：

- Docker 镜像
- runtime 最大 turn 次数
- generator 子代理最大 turn 次数
- feature 重试上限
- evaluator 最大轮次

全局 backend 配置位于 `~/.codex-forge/config.toml`，至少支持：

- `backend.provider`
- `backend.key`
- `backend.base_url`
- `backend.model`
- `backend.turn_timeout_secs`

示例：

```toml
[backend]
provider = "openai_compatible"
key = "sk-..."
base_url = "https://example.com/v1"
model = "gpt-4o-mini"
turn_timeout_secs = 600
```

如果继续使用 Codex：

```toml
[backend]
provider = "codex"
model = "gpt-5-codex"
turn_timeout_secs = 600
```

### Thread

```bash
codex-forge thread new --title "仓库梳理"
codex-forge thread list
codex-forge thread show <thread-id>
```

### Chat

不指定 `--thread` 会自动创建新的 thread：

```bash
codex-forge chat "请总结这个仓库目前的用途"
```

指定已有 thread：

```bash
codex-forge chat --thread <thread-id> "请修改 file-a.txt 为 beta"
```

### Run

```bash
codex-forge run list --thread <thread-id>
codex-forge run show --thread <thread-id> <run-id>
```

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
codex-forge artifact show --thread <thread-id> <artifact-id>
```

### TUI

```bash
codex-forge tui
```

TUI 当前快捷键：

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

## Docker 沙箱

当前使用 **每个 run 独立 Docker 容器**：

- 启动 run 时创建独立容器
- 容器工作目录为 run 级工作区快照
- `run_shell` 在容器内执行
- `write_file` / `apply_patch` 先修改 run 工作区，再立即同步回目标目录
- run 完成或失败后自动销毁容器

默认镜像来自 `codex-forge.toml`：

```toml
[sandbox]
docker_image = "codex-forge-sandbox:latest"
```

首次在本机准备默认镜像：

```bash
./scripts/build_sandbox_image.sh
```

默认会优先尝试当前已验证可拉的国内基础镜像：

```text
swr.cn-north-4.myhuaweicloud.com/ddn-k8s/docker.io/library/debian:bookworm-slim
-> docker.m.daocloud.io/library/debian:bookworm-slim
-> debian:bookworm-slim
```

如果你想固定使用某个基础镜像，也可以手工覆盖：

```bash
SANDBOX_BASE_IMAGE="claude-sdk:latest" ./scripts/build_sandbox_image.sh
```

如果你的机器需要固定平台，也可以显式指定：

```bash
SANDBOX_PLATFORM="linux/arm64" ./scripts/build_sandbox_image.sh
```

如果本机需要额外工具，可以在构建时附加 apt 包：

```bash
EXTRA_APT_PACKAGES="nodejs npm" ./scripts/build_sandbox_image.sh
```

默认镜像基于 `debian:bookworm-slim`，内置：

- `sh` / `bash`
- `git`
- `ripgrep`
- `python3`
- `curl`
- `jq`
- `make`

## Backend 协议

backend 现在不再返回自由文本，而是优先返回结构化 JSON envelope：

```json
{
  "assistant_message": "给用户或工具前的说明",
  "tool_calls": [
    { "name": "read_file", "arguments": { "path": "README.md" } }
  ],
  "subagent_calls": [
    { "kind": "planner", "task": "分析当前模块" }
  ],
  "final_response": false,
  "selected_feature_id": "feature-1",
  "evaluation": {
    "passed": true,
    "reason": "当前 feature 已满足 done_when",
    "follow_up_actions": [],
    "retryable": false,
    "feature_id": "feature-1"
  }
}
```

如果 backend 返回非 JSON 文本，runtime 会把它视为最终回复。

## 开发与测试

### 本地检查

```bash
cargo fmt
cargo check
cargo test -- --nocapture
```

### 当前测试覆盖

- thread 创建/列出
- chat 主路径
- run show / replay
- approval 流程
- artifact 流程
- config 命令
- backend envelope 解析
- store roundtrip
- contract / progress / evaluation roundtrip
- 工具参数兼容与文件截断行为

关键测试文件：

- [tests/thread_run_flow.rs](/Users/wangzijian/RustProject/codex-forge/tests/thread_run_flow.rs)
- [tests/approval_and_artifact.rs](/Users/wangzijian/RustProject/codex-forge/tests/approval_and_artifact.rs)
- [src/harness/store/mod.rs](/Users/wangzijian/RustProject/codex-forge/src/harness/store/mod.rs)
- [src/harness/backend/mod.rs](/Users/wangzijian/RustProject/codex-forge/src/harness/backend/mod.rs)

## 说明

当前版本是本地产品闭环，不包含：

- HTTP / Gateway / Web UI
- 全局跨仓库 memory
- 多 backend 的完整生产级 provider 实现

但本地 CLI/TUI、Docker 沙箱、approval、artifact、replay、基础 sub-agent 已经在同一条新主路径上闭环。
