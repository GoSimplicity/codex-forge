#!/usr/bin/env python3

from __future__ import annotations

import argparse
import fcntl
import os
import pty
import re
import select
import shutil
import signal
import subprocess
import struct
import sys
import tempfile
import termios
import textwrap
import time
from pathlib import Path


ANSI_OSC_RE = re.compile(rb"\x1b\][^\x07]*(?:\x07|\x1b\\)")
ANSI_CSI_RE = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")
ANSI_SINGLE_RE = re.compile(rb"\x1b[@-_]")
CONTROL_RE = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f]")


class SmokeFailure(RuntimeError):
    pass


class PtySession:
    def __init__(self, argv: list[str], env: dict[str, str], transcript_path: Path) -> None:
        self.argv = argv
        self.env = env
        self.transcript_path = transcript_path
        self.pid: int | None = None
        self.master_fd: int | None = None
        self._clean_text = ""
        self._exit_status: int | None = None

    def start(self) -> None:
        pid, master_fd = pty.fork()
        if pid == 0:
            os.execvpe(self.argv[0], self.argv, self.env)
            raise SystemExit(127)

        self.pid = pid
        self.master_fd = master_fd
        os.set_blocking(master_fd, False)
        set_pty_size(
            master_fd,
            columns=int(self.env.get("COLUMNS", "140")),
            lines=int(self.env.get("LINES", "42")),
        )

    def mark(self) -> int:
        return len(self._clean_text)

    def send(self, text: str, pause: float = 0.08) -> None:
        if self.master_fd is None:
            raise RuntimeError("PTY 尚未启动")
        os.write(self.master_fd, text.encode("utf-8"))
        if pause > 0:
            time.sleep(pause)

    def read_some(self, timeout: float = 0.2) -> bool:
        if self.master_fd is None:
            raise RuntimeError("PTY 尚未启动")

        ready, _, _ = select.select([self.master_fd], [], [], timeout)
        if not ready:
            return False

        try:
            chunk = os.read(self.master_fd, 65536)
        except OSError:
            return False

        if not chunk:
            return False

        with self.transcript_path.open("ab") as handle:
            handle.write(chunk)
        self._clean_text += normalize_screen_text(chunk)
        if len(self._clean_text) > 400_000:
            self._clean_text = self._clean_text[-400_000:]
        return True

    def wait_for(
        self,
        label: str,
        patterns: list[str],
        timeout: float,
        since: int,
    ) -> int:
        deadline = time.time() + timeout
        while time.time() < deadline:
            recent = self._clean_text[since:]
            if all(pattern in recent for pattern in patterns):
                log(f"[ok] {label}")
                return len(self._clean_text)

            self.read_some(timeout=0.2)

            if self.is_exited():
                recent = self._clean_text[since:]
                if all(pattern in recent for pattern in patterns):
                    log(f"[ok] {label}")
                    return len(self._clean_text)
                raise SmokeFailure(
                    f"{label} 失败：进程提前退出，未等到关键文本 {patterns!r}\n"
                    f"{self.debug_tail(since)}"
                )

        raise SmokeFailure(
            f"{label} 超时：{timeout:.1f}s 内未等到关键文本 {patterns!r}\n"
            f"{self.debug_tail(since)}"
        )

    def wait_for_exit(self, timeout: float = 5.0) -> int:
        if self._exit_status is not None:
            return self._exit_status

        deadline = time.time() + timeout
        while time.time() < deadline:
            self.read_some(timeout=0.1)
            status = self.poll_exit()
            if status is not None:
                return status
        raise SmokeFailure("TUI 进程在发送退出键后仍未结束")

    def pump(self, duration: float) -> None:
        deadline = time.time() + duration
        while time.time() < deadline:
            self.read_some(timeout=0.2)

    def is_exited(self) -> bool:
        return self.poll_exit() is not None

    def poll_exit(self) -> int | None:
        if self._exit_status is not None:
            return self._exit_status
        if self.pid is None:
            self._exit_status = 0
            return self._exit_status
        waited, status = os.waitpid(self.pid, os.WNOHANG)
        if waited != self.pid:
            return None

        if os.WIFEXITED(status):
            self._exit_status = os.WEXITSTATUS(status)
        elif os.WIFSIGNALED(status):
            self._exit_status = 128 + os.WTERMSIG(status)
        else:
            self._exit_status = status
        return self._exit_status

    def terminate(self) -> None:
        if self.pid is None:
            return
        try:
            os.kill(self.pid, signal.SIGTERM)
        except ProcessLookupError:
            return

    def debug_tail(self, since: int, lines: int = 40) -> str:
        recent = self._clean_text[since:]
        excerpt = "\n".join(recent.splitlines()[-lines:])
        return f"---- 最近屏幕文本 ----\n{excerpt}\n----------------------"


def normalize_screen_text(chunk: bytes) -> str:
    data = ANSI_OSC_RE.sub(b"", chunk)
    data = ANSI_CSI_RE.sub(b"", data)
    data = ANSI_SINGLE_RE.sub(b"", data)
    text = data.decode("utf-8", errors="ignore").replace("\r", "\n")
    text = CONTROL_RE.sub("", text)
    return text


def log(message: str) -> None:
    print(message, flush=True)


def set_pty_size(fd: int, columns: int, lines: int) -> None:
    size = struct.pack("HHHH", lines, columns, 0, 0)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, size)


def run(cmd: list[str], cwd: Path | None = None, env: dict[str, str] | None = None) -> None:
    subprocess.run(cmd, cwd=cwd, env=env, check=True)


def ensure_binary(repo_root: Path, explicit_bin: str | None) -> Path:
    if explicit_bin:
        path = Path(explicit_bin).expanduser().resolve()
        if not path.is_file():
            raise SmokeFailure(f"指定的二进制不存在：{path}")
        return path

    log("[run] cargo build --quiet")
    run(["cargo", "build", "--quiet"], cwd=repo_root)
    path = repo_root / "target" / "debug" / "codex-forge"
    if not path.is_file():
        raise SmokeFailure(f"构建完成后未找到二进制：{path}")
    return path


def create_fixture_workspace(base_dir: Path) -> tuple[Path, Path]:
    repo_dir = base_dir / "target-repo"
    bin_dir = base_dir / "fake-bin"
    repo_dir.mkdir(parents=True, exist_ok=True)
    bin_dir.mkdir(parents=True, exist_ok=True)

    run(["git", "init"], cwd=repo_dir)
    run(["git", "config", "user.email", "codex@example.com"], cwd=repo_dir)
    run(["git", "config", "user.name", "Codex"], cwd=repo_dir)

    (repo_dir / "README.md").write_text("seed\n", encoding="utf-8")
    (repo_dir / "file-a.txt").write_text("alpha\n", encoding="utf-8")
    (repo_dir / "codex-forge.toml").write_text(
        textwrap.dedent(
            """
            [defaults]
            workers = 3
            apply_mode = "auto-safe"
            max_retries = 2
            verification_commands = ["git status --short >/dev/null"]
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )

    fake_codex = bin_dir / "codex"
    fake_codex.write_text(build_fake_codex_script(), encoding="utf-8")
    fake_codex.chmod(0o755)

    run(["git", "add", "."], cwd=repo_dir)
    run(["git", "commit", "-m", "init"], cwd=repo_dir)

    return repo_dir, bin_dir


def build_fake_codex_script() -> str:
    return textwrap.dedent(
        r"""
        #!/bin/sh
        set -eu

        cwd="$PWD"
        output=""
        json_mode=0
        prompt=""

        while [ "$#" -gt 0 ]; do
          case "$1" in
            exec) shift ;;
            -C) cwd="$2"; shift 2 ;;
            -o) output="$2"; shift 2 ;;
            --json) json_mode=1; shift ;;
            --skip-git-repo-check|--ephemeral|--full-auto) shift ;;
            --color|-m) shift 2 ;;
            *) prompt="$1"; shift ;;
          esac
        done

        cd "$cwd"
        mkdir -p "$(dirname "$output")"
        agent_id="$(basename "$cwd")"

        if printf "%s" "${prompt:-}" | grep -q "最终输出必须是一个合法 JSON 对象"; then
          if printf "%s" "${prompt:-}" | grep -q "面向用户可读"; then
            cat >"$output" <<'JSON'
{"summary":"todo summary","approach":"todo approach","risks":["r1"],"todos":[
  {"title":"明确范围","goal":"先明确边界","details":["梳理目标"],"dependencies":[],"completion_criteria":["完成范围确认"]},
  {"title":"实现主干","goal":"完成主路径","details":["搭建主功能"],"dependencies":["明确范围"],"completion_criteria":["主链路可运行"]}
]}
JSON
          else
            cat >"$output" <<'JSON'
{"summary":"plan","strategy":"success","nodes":[
  {"title":"实现主干","role":"implementer","objective":"改 file-a","deliverables":["patch"],"dependencies":[],"prompt_focus":"改主文件","input_artifacts":[],"output_artifacts":["handoff"],"completion_criteria":["完成"]},
  {"title":"审阅","role":"reviewer","objective":"审阅","deliverables":["review"],"dependencies":["implementer-1"],"prompt_focus":"找问题","input_artifacts":["handoff"],"output_artifacts":["review"],"completion_criteria":["完成"]}
]}
JSON
          fi
          echo '{"type":"planner.done","message":"ok"}'
          echo 'planner stderr noise' >&2
          exit 0
        fi

        if [ "$agent_id" = "implementer-1" ]; then
          printf 'from implementer 1\n' >> file-a.txt
        fi

        if [ "$agent_id" = "reviewer-1" ]; then
          cat >"$output" <<'EOF'
# 交付摘要
reviewer allow
# 变更清单
- reviewed worker outputs
# 风险
- 无
# 验证
- fake verify
# 交接
- APPLY_DECISION: allow
- reviewer allow
EOF
          if [ "$json_mode" -eq 1 ]; then
            echo '{"type":"turn.started","message":"review"}'
            echo 'reviewer stderr noise' >&2
          fi
          exit 0
        fi

        cat >"$output" <<'EOF'
# 交付摘要
implementer done
# 变更清单
- touched file-a.txt
# 风险
- 无
# 验证
- fake verify
# 交接
- downstream ok
EOF

        if [ "$json_mode" -eq 1 ]; then
          echo '{"type":"turn.started","message":"running"}'
          echo 'worker stderr noise' >&2
        fi
        """
    ).lstrip()


def run_smoke(binary: Path, repo_dir: Path, bin_dir: Path, transcript_path: Path) -> None:
    env = os.environ.copy()
    env["TERM"] = env.get("TERM", "xterm-256color")
    env["COLUMNS"] = "140"
    env["LINES"] = "42"
    env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"

    argv = [
        str(binary),
        "tui",
        "--target-dir",
        str(repo_dir),
    ]

    log(f"[run] {' '.join(argv)}")
    session = PtySession(argv, env, transcript_path)
    session.start()

    try:
        start_mark = session.mark()
        session.wait_for("启动首页", ["任务主路径", "默认摘要"], timeout=15, since=start_mark)

        mark = session.mark()
        session.send("e")
        session.wait_for("打开任务编辑", ["编辑字段：任务描述"], timeout=5, since=mark)

        session.send("真实 PTY smoke 脚本")
        mark = session.mark()
        session.send("\x1b", pause=0.15)
        session.wait_for("保存任务", ["任务已保存"], timeout=5, since=mark)

        mark = session.mark()
        session.send("d")
        session.wait_for("进入 Doctor", ["动作：检查环境"], timeout=5, since=mark)
        session.wait_for("Doctor 完成", ["检查环境 已结束：成功"], timeout=20, since=mark)

        mark = session.mark()
        session.send("\x1b", pause=0.15)
        session.wait_for(
            "Doctor 后返回开始页",
            ["任务主路径", "最近环境检查：绿色"],
            timeout=8,
            since=mark,
        )

        mark = session.mark()
        session.send("p")
        session.wait_for("进入 Plan", ["动作：先看方案"], timeout=5, since=mark)
        session.wait_for(
            "Plan 完成并进入历史页",
            ["先看方案 已结束：成功", "历史会话", "历史结果详情"],
            timeout=20,
            since=mark,
        )

        mark = session.mark()
        session.send("\x1b", pause=0.15)
        session.wait_for(
            "从历史页回到执行页",
            ["执行状态/事件", "动作：先看方案", "已返回执行页"],
            timeout=8,
            since=mark,
        )

        mark = session.mark()
        session.send("\x1b", pause=0.15)
        session.wait_for("从执行页回到开始页", ["任务主路径"], timeout=8, since=mark)

        mark = session.mark()
        session.send("r")
        session.wait_for("进入 Run", ["动作：开始执行"], timeout=5, since=mark)
        session.wait_for(
            "Run 完成并进入历史页",
            ["开始执行 已结束：成功", "历史会话", "历史结果详情"],
            timeout=25,
            since=mark,
        )

        mark = session.mark()
        session.send("l")
        session.wait_for("进入 Replay", ["动作：回放过程"], timeout=5, since=mark)
        session.pump(5.0)

        mark = session.mark()
        session.send("\x1b", pause=0.15)
        session.wait_for(
            "Replay 后回到历史页",
            ["历史会话", "历史结果详情"],
            timeout=8,
            since=mark,
        )

        session.send("q", pause=0.1)
        exit_code = session.wait_for_exit(timeout=5)
        if exit_code != 0:
            raise SmokeFailure(f"TUI 退出码异常：{exit_code}")
        log("[ok] 真实 PTY smoke 全链路通过")
    except Exception:
        session.terminate()
        raise


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="用真实 PTY 自动复走 codex-forge TUI 的 Start → Doctor → Plan → Run → History → Replay smoke。"
    )
    parser.add_argument("--bin", help="显式指定 codex-forge 二进制路径；默认会先 cargo build")
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        help="保留临时仓库和 PTY transcript，便于排障",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    repo_root = Path(__file__).resolve().parents[1]
    temp_dir = Path(tempfile.mkdtemp(prefix="codex-forge-pty-smoke-"))
    transcript_path = temp_dir / "pty-transcript.log"
    success = False

    try:
        binary = ensure_binary(repo_root, args.bin)
        repo_dir, bin_dir = create_fixture_workspace(temp_dir)
        home_dir = temp_dir / "home"
        home_dir.mkdir(parents=True, exist_ok=True)
        os.environ["HOME"] = str(home_dir)
        log(f"[info] 临时仓库：{repo_dir}")
        log(f"[info] fake codex：{bin_dir / 'codex'}")
        log(f"[info] transcript：{transcript_path}")
        run_smoke(binary, repo_dir, bin_dir, transcript_path)
        success = True
        return 0
    except subprocess.CalledProcessError as error:
        log(f"[fail] 子进程失败：{error}")
        return error.returncode or 1
    except SmokeFailure as error:
        log(f"[fail] {error}")
        return 1
    finally:
        if success and not args.keep_temp:
            shutil.rmtree(temp_dir, ignore_errors=True)
        else:
            log(f"[info] 临时目录已保留：{temp_dir}")


if __name__ == "__main__":
    sys.exit(main())
