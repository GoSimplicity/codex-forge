use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

pub fn make_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    run(dir.path(), &["git", "init", "-b", "main"]);
    run(
        dir.path(),
        &["git", "config", "user.email", "codex@example.com"],
    );
    run(dir.path(), &["git", "config", "user.name", "Codex"]);
    fs::create_dir_all(dir.path().join("src")).expect("mkdir src");
    fs::write(dir.path().join("README.md"), "seed\n").expect("write readme");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write cargo");
    fs::write(
        dir.path().join("src").join("main.rs"),
        "fn main() {\n    println!(\"demo\");\n}\n",
    )
    .expect("write main");
    fs::write(dir.path().join("file-a.txt"), "alpha\n").expect("write file-a");
    install_fake_binaries(dir.path());
    run(dir.path(), &["git", "add", "."]);
    run(dir.path(), &["git", "commit", "-m", "init"]);
    dir
}

pub fn command(bin: &str, repo: &Path) -> Command {
    let mut cmd = Command::new(bin);
    let path = std::env::var("PATH").expect("PATH");
    let docker_state = docker_state_dir(repo);
    let config_home = fake_config_home_dir(repo);
    let _ = fs::create_dir_all(&docker_state);
    let _ = fs::create_dir_all(&config_home);
    cmd.env(
        "PATH",
        format!("{}:{}", repo.join(".fake-bin").display(), path),
    );
    cmd.env("CODEX_FAKE_DOCKER_STATE", &docker_state);
    cmd.env("CODEX_FORGE_HOME", &config_home);
    cmd.current_dir(repo);
    cmd
}

pub fn docker_state_dir(repo: &Path) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "codex-forge-fake-docker-{}",
        repo.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repo")
    ))
}

pub fn fake_home_dir(repo: &Path) -> std::path::PathBuf {
    repo.join(".fake-home")
}

pub fn fake_config_home_dir(repo: &Path) -> std::path::PathBuf {
    fake_home_dir(repo).join(".codex-forge")
}

#[allow(dead_code)]
pub fn write_global_config(repo: &Path, content: &str) {
    let path = fake_config_home_dir(repo).join("config.toml");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir global config dir");
    }
    fs::write(path, content).expect("write global config");
}

fn install_fake_binaries(repo: &Path) {
    let bin_dir = repo.join(".fake-bin");
    fs::create_dir_all(&bin_dir).expect("create bin dir");
    write_fake_codex(&bin_dir);
    write_fake_docker(&bin_dir);
}

fn write_fake_codex(bin_dir: &Path) {
    let script_path = bin_dir.join("codex");
    fs::write(
        &script_path,
        r#"#!/bin/sh
set -eu

cwd="$PWD"
output=""
prompt=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    exec) shift ;;
    -C) cwd="$2"; shift 2 ;;
    -o) output="$2"; shift 2 ;;
    -m|-c) shift 2 ;;
    --skip-git-repo-check|--ephemeral) shift ;;
    --color) shift 2 ;;
    *)
      prompt="$1"
      shift ;;
  esac
done

case "$prompt" in
  *"执行模式：autonomous_codex"* )
    case "$prompt" in
      *"请修改 file-a.txt 为 beta"* )
        printf 'beta\n' > "$cwd/file-a.txt"
        printf '%s\n' "file-a.txt 已修改为 beta，并完成本次任务。" > "$output"
        ;;
      *"请在沙箱里生成 out/result.txt"* )
        mkdir -p "$cwd/out"
        printf 'artifact' > "$cwd/out/result.txt"
        printf '%s\n' "out/result.txt 已生成。" > "$output"
        ;;
      *"请总结这个仓库目前的用途"* )
        printf '%s\n' "这是一个本地 Codex harness，用 thread/run 管理对话与执行，并带有长期任务状态机。" > "$output"
        ;;
      *"请解释当前线程的作用"* )
        printf '%s\n' "这个线程用于承载当前任务的连续对话和运行历史。" > "$output"
        ;;
      * )
        printf '%s\n' "默认回复。" > "$output"
        ;;
    esac
    ;;
  *"请修改 file-a.txt 为 beta"* )
    if printf "%s" "$prompt" | grep -q 'write_file `file-a.txt` 成功'; then
      cat > "$output" <<'JSON'
{"assistant_message":"文件已经改成 beta，建议进入 evaluator 验收。","tool_calls":[],"subagent_calls":[{"kind":"evaluator","task":"请确认 file-a.txt 已改为 beta，且本次修改已满足目标。"}],"final_response":false,"needs_handoff":true}
JSON
    else
      cat > "$output" <<'JSON'
{"assistant_message":"我准备修改 file-a.txt。","tool_calls":[{"name":"write_file","arguments":{"path":"file-a.txt","content":"beta\n"}}],"subagent_calls":[],"final_response":false}
JSON
    fi
    ;;
  *"请在沙箱里执行 pwd（使用cmd键）"* )
    if printf "%s" "$prompt" | grep -q "run_shell 结果" || printf "%s" "$prompt" | grep -q '$ pwd'; then
      cat > "$output" <<'JSON'
{"assistant_message":"pwd 已执行完成。","tool_calls":[],"subagent_calls":[],"final_response":true}
JSON
    else
      cat > "$output" <<'JSON'
{"assistant_message":"我先执行 pwd。","tool_calls":[{"name":"run_shell","arguments":{"cmd":"pwd"}}],"subagent_calls":[],"final_response":false}
JSON
    fi
    ;;
  *"请在沙箱里生成 out/result.txt"* )
    if printf "%s" "$prompt" | grep -q "run_shell 结果"; then
      cat > "$output" <<'JSON'
{"assistant_message":"产物已经生成，建议进入 evaluator 验收。","tool_calls":[],"subagent_calls":[{"kind":"evaluator","task":"请确认 out/result.txt 已生成且内容符合预期。"}],"final_response":false,"needs_handoff":true}
JSON
    else
      cat > "$output" <<'JSON'
{"assistant_message":"我先在沙箱里生成产物文件。","tool_calls":[{"name":"run_shell","arguments":{"command":"mkdir -p out && printf artifact > out/result.txt"}}],"subagent_calls":[],"final_response":false}
JSON
    fi
    ;;
  *"请总结这个仓库目前的用途"* )
    if printf "%s" "$prompt" | grep -q 'README.md' \
      && printf "%s" "$prompt" | grep -q 'Cargo.toml' \
      && printf "%s" "$prompt" | grep -q 'src/main.rs'; then
      cat > "$output" <<'JSON'
{"assistant_message":"这是一个本地 Codex harness，用 thread/run 管理对话与执行，并带有长期任务状态机。","tool_calls":[],"subagent_calls":[],"final_response":true}
JSON
    else
      cat > "$output" <<'JSON'
{"assistant_message":"我先读取 README、Cargo.toml 和入口源码，再总结仓库用途。","tool_calls":[{"name":"read_file","arguments":{"path":"README.md"}},{"name":"read_file","arguments":{"path":"Cargo.toml"}},{"name":"read_file","arguments":{"path":"src/main.rs"}}],"subagent_calls":[],"final_response":false}
JSON
    fi
    ;;
  *"请解释当前线程的作用"* )
    cat > "$output" <<'JSON'
{"assistant_message":"这个线程用于承载当前任务的连续对话和运行历史。","tool_calls":[],"subagent_calls":[],"final_response":true}
JSON
    ;;
  *"请查看 README"* )
    if printf "%s" "$prompt" | grep -q "read_file"; then
      cat > "$output" <<'JSON'
{"assistant_message":"README 已查看。","tool_calls":[],"subagent_calls":[],"final_response":true}
JSON
    else
      cat > "$output" <<'JSON'
{"assistant_message":"我先读取 README。","tool_calls":[{"name":"read_file","arguments":{"path":"README.md"}}],"subagent_calls":[],"final_response":false}
JSON
    fi
    ;;
  * )
    cat > "$output" <<'JSON'
{"assistant_message":"默认回复。","tool_calls":[],"subagent_calls":[],"final_response":true}
JSON
    ;;
esac
"#,
    )
    .expect("write fake codex");
    chmod_755(&script_path);
}

fn write_fake_docker(bin_dir: &Path) {
    let script_path = bin_dir.join("docker");
    fs::write(
        &script_path,
        r#"#!/bin/sh
set -eu

state_dir="${CODEX_FAKE_DOCKER_STATE:-/tmp/codex-fake-docker}"
mkdir -p "$state_dir"

cmd="${1:-}"
shift || true

case "$cmd" in
  --version)
    echo "Docker version 27.0.0, build fake"
    ;;
  run)
    name=""
    repo_mount=""
    run_mount=""
    args_log=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        -d|--rm|--privileged) args_log="$args_log $1"; shift ;;
        --name) name="$2"; shift 2 ;;
        --user) args_log="$args_log --user $2"; shift 2 ;;
        -v)
          case "$2" in
            *:/workspace/repo) repo_mount="${2%%:/workspace/repo}" ;;
            *:/workspace/run) run_mount="${2%%:/workspace/run}" ;;
          esac
          args_log="$args_log -v $2"
          shift 2
          ;;
        -w) shift 2 ;;
        *)
          image="$1"
          shift
          break
          ;;
      esac
    done
    printf "%s" "$repo_mount" > "$state_dir/$name.repo"
    printf "%s" "$run_mount" > "$state_dir/$name.run"
    printf "%s\n" "$args_log" > "$state_dir/$name.run-args"
    echo "$name"
    ;;
  exec)
    name="$1"
    shift
    repo_mount="$(cat "$state_dir/$name.repo")"
    while [ "$#" -gt 0 ]; do
      case "$1" in
        sh) shift ;;
        -lc)
          shell_cmd="$2"
          shift 2
          ;;
        *)
          shift
          ;;
      esac
    done
    case "$shell_cmd" in
      chown\ -R\ *)
        exit 0
        ;;
    esac
    cd "$repo_mount"
    /bin/sh -lc "$shell_cmd"
    ;;
  rm)
    if [ "${1:-}" = "-f" ]; then shift; fi
    name="$1"
    rm -f "$state_dir/$name.repo" "$state_dir/$name.run"
    ;;
  *)
    echo "unsupported docker subcommand: $cmd" >&2
    exit 1
    ;;
esac
"#,
    )
    .expect("write fake docker");
    chmod_755(&script_path);
}

fn run(dir: &Path, args: &[&str]) {
    let status = Command::new(args[0])
        .args(&args[1..])
        .current_dir(dir)
        .status()
        .expect("run command");
    assert!(status.success(), "command failed: {:?}", args);
}

fn chmod_755(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("chmod");
    }
}
