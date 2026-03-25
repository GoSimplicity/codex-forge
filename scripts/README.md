# scripts

- `python3 scripts/tui_smoke_real_pty.py`
  - 用真实 PTY 拉起新的 `codex-forge tui`
  - 当前应围绕新交互验证：`Threads -> Chat -> Approvals -> Artifacts -> Events -> Composer`
  - 如果要扩展 smoke 范围，优先覆盖：
    - `chat`
    - `approval approve/deny`
    - `artifact` 面板
    - `replay` 事件流
  - 需要排障时可加 `--keep-temp`，保留 transcript 和临时仓库
