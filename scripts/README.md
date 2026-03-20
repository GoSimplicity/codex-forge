# scripts

- `python3 scripts/tui_smoke_real_pty.py`
  - 用真实 PTY 拉起 `codex-forge tui`，自动复走 `Start -> Doctor -> Plan -> Run -> History -> Replay`
  - 脚本会自动 `cargo build --quiet`、创建临时 Git 仓库、注入 fake `codex`、并对关键屏幕文本做断言
  - 需要排障时可加 `--keep-temp`，保留 transcript 和临时仓库
