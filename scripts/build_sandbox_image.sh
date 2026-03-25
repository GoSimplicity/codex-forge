#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
IMAGE_TAG="${IMAGE_TAG:-codex-forge-sandbox:latest}"
EXTRA_APT_PACKAGES="${EXTRA_APT_PACKAGES:-}"
SANDBOX_PLATFORM="${SANDBOX_PLATFORM:-$(docker version --format '{{.Server.Os}}/{{.Server.Arch}}')}"
DEFAULT_BASE_IMAGE_CANDIDATES=(
  "swr.cn-north-4.myhuaweicloud.com/ddn-k8s/docker.io/library/debian:bookworm-slim"
  "docker.m.daocloud.io/library/debian:bookworm-slim"
  "debian:bookworm-slim"
)

pick_base_image() {
  if [[ -n "${SANDBOX_BASE_IMAGE:-}" ]]; then
    echo "$SANDBOX_BASE_IMAGE"
    return 0
  fi

  local image
  local local_platform
  for image in "${DEFAULT_BASE_IMAGE_CANDIDATES[@]}"; do
    echo "尝试基础镜像: $image" >&2
    if docker image inspect "$image" >/dev/null 2>&1; then
      local_platform="$(docker image inspect "$image" --format '{{.Os}}/{{.Architecture}}')"
      if [[ "$local_platform" == "$SANDBOX_PLATFORM" ]]; then
        echo "命中本地镜像缓存: $image ($local_platform)" >&2
        echo "$image"
        return 0
      fi
      echo "本地缓存平台不匹配: $image ($local_platform)，尝试重新拉取 $SANDBOX_PLATFORM" >&2
    fi
    if docker pull --platform "$SANDBOX_PLATFORM" "$image" >/dev/null 2>&1; then
      echo "拉取成功: $image" >&2
      echo "$image"
      return 0
    fi
    echo "拉取失败，继续尝试下一个候选" >&2
  done

  echo "未找到可用基础镜像，请手动设置 SANDBOX_BASE_IMAGE" >&2
  return 1
}

SANDBOX_BASE_IMAGE="$(pick_base_image)"

cd "$REPO_ROOT"

echo "开始构建 Docker 沙箱镜像: $IMAGE_TAG"
echo "基础镜像: $SANDBOX_BASE_IMAGE"
echo "目标平台: $SANDBOX_PLATFORM"
if [[ -n "$EXTRA_APT_PACKAGES" ]]; then
  echo "附加 apt 包: $EXTRA_APT_PACKAGES"
fi

docker build \
  --platform "$SANDBOX_PLATFORM" \
  -f Dockerfile.sandbox \
  -t "$IMAGE_TAG" \
  --build-arg "BASE_IMAGE=$SANDBOX_BASE_IMAGE" \
  --build-arg "EXTRA_APT_PACKAGES=$EXTRA_APT_PACKAGES" \
  .

echo "构建完成: $IMAGE_TAG"
docker image inspect "$IMAGE_TAG" --format '镜像ID: {{.Id}}'
