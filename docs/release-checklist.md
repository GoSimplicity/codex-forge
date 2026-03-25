# codex-forge 上线检查清单

## 发布前代码

- `git status --short` 只保留本次准备发布的改动，没有临时调试文件、缓存文件或无关变更。
- `cargo fmt --all` 已执行。
- `cargo clippy --all-targets --all-features -- -D warnings` 已通过。
- `cargo test` 已通过。
- `README.md`、配置说明、运行时路径和实际行为一致。

## 运行时与配置

- 本机可访问 Docker daemon，`docker ps` 正常。
- `codex` CLI 已安装并可执行。
- `codex-forge config validate` 已通过。
- `codex-forge.toml` 中 `sandbox.docker_image`、`runtime.*`、`backend.turn_timeout_secs` 已按生产环境确认。
- `codex-forge-sandbox:latest` 已构建并可启动。

## 真实回归

- 真实只读任务回归通过：thread 创建、run 完成、最终回复落盘。
- 真实写入任务回归通过：进入审批、审批后继续执行、artifact 与 run 状态正确。
- TUI 启动正常，输入模式、线程列表、run 列表、详情面板可用。
- 超时保护生效：后端长时间无返回时，能得到明确错误而不是无限挂起。

## 发布产物

- 没有提交 `.codex-forge/`、`__pycache__/`、`.pyc` 等运行期或缓存产物。
- 若要打 tag 或发 release，先记录本次真实回归结果、已知限制和回滚方式。

## 当前已知限制

- 真实 TUI 的自动化端到端仍以 PTY smoke 为主，完整人工消息流更适合发布前人工点检。
- feature 切分仍为启发式逻辑，复杂需求下需要持续观察 planner 输出质量。
