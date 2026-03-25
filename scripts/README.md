# scripts

- `./scripts/build_sandbox_image.sh`
  - 构建默认 Docker 沙箱镜像 `codex-forge-sandbox:latest`
  - 默认会按顺序尝试国内可拉基础镜像：
    `swr.cn-north-4.myhuaweicloud.com/ddn-k8s/docker.io/library/debian:bookworm-slim`
    -> `docker.m.daocloud.io/library/debian:bookworm-slim`
    -> `debian:bookworm-slim`
  - 可用 `IMAGE_TAG=...` 覆盖镜像名
  - 可用 `SANDBOX_PLATFORM=linux/arm64` 或 `linux/amd64` 指定目标平台
  - 可用 `SANDBOX_BASE_IMAGE=claude-sdk:latest` 复用本机已有基础镜像
  - 可用 `EXTRA_APT_PACKAGES="nodejs npm"` 为本机补额外工具
- `python3 scripts/tui_smoke_real_pty.py`
  - 用真实 PTY 拉起新的 `codex-forge tui`
  - 当前应围绕新交互验证：`Threads -> Chat -> Approvals -> Artifacts -> Events -> Composer`
  - 如果要扩展 smoke 范围，优先覆盖：
    - `chat`
    - `approval approve/deny`
    - `artifact` 面板
    - `replay` 事件流
  - 需要排障时可加 `--keep-temp`，保留 transcript 和临时仓库
