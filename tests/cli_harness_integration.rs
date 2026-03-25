use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn make_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    run(dir.path(), &["git", "init", "-b", "main"]);
    run(
        dir.path(),
        &["git", "config", "user.email", "codex@example.com"],
    );
    run(dir.path(), &["git", "config", "user.name", "Codex"]);
    fs::write(dir.path().join("README.md"), "seed\n").expect("write readme");
    fs::write(dir.path().join("file-a.txt"), "alpha\n").expect("write file-a");
    install_fake_binaries(dir.path());
    run(dir.path(), &["git", "add", "."]);
    run(dir.path(), &["git", "commit", "-m", "init"]);
    dir
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
  *"请修改 file-a.txt 为 beta"* )
    if printf "%s" "$prompt" | grep -q 'write_file `file-a.txt` 成功'; then
      cat > "$output" <<'JSON'
{"assistant_message":"文件已经改成 beta，并已完成本轮处理。","tool_calls":[],"subagent_calls":[],"final_response":true}
JSON
    else
      cat > "$output" <<'JSON'
{"assistant_message":"我准备修改 file-a.txt。","tool_calls":[{"name":"write_file","arguments":{"path":"file-a.txt","content":"beta\n"}}],"subagent_calls":[],"final_response":false}
JSON
    fi
    ;;
  *"请总结这个仓库目前的用途"* )
    cat > "$output" <<'JSON'
{"assistant_message":"这是一个本地 Codex harness，用 thread/run 管理对话与执行。","tool_calls":[],"subagent_calls":[],"final_response":true}
JSON
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
    volume=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        -d|--rm) shift ;;
        --name) name="$2"; shift 2 ;;
        -v) volume="$2"; shift 2 ;;
        -w) shift 2 ;;
        *)
          image="$1"
          shift
          break
          ;;
      esac
    done
    workspace="${volume%%:*}"
    printf "%s" "$workspace" > "$state_dir/$name.workspace"
    echo "$name"
    ;;
  exec)
    name="$1"
    shift
    workspace="$(cat "$state_dir/$name.workspace")"
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
    cd "$workspace/repo"
    /bin/sh -lc "$shell_cmd"
    ;;
  rm)
    if [ "${1:-}" = "-f" ]; then shift; fi
    name="$1"
    rm -f "$state_dir/$name.workspace"
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

#[test]
fn thread_new_list_chat_run_and_replay_work() {
    let repo = make_repo();
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let created = command(bin, repo.path())
        .args([
            "thread",
            "new",
            "--title",
            "Harness Demo",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run thread new");
    assert!(created.status.success(), "{:?}", created);
    let stdout = String::from_utf8_lossy(&created.stdout);
    let thread_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("id: "))
        .expect("thread id")
        .to_string();

    let listed = command(bin, repo.path())
        .args([
            "thread",
            "list",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run thread list");
    assert!(listed.status.success(), "{:?}", listed);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains(&thread_id),
        "{:?}",
        listed
    );

    let chat = command(bin, repo.path())
        .args([
            "chat",
            "--thread",
            &thread_id,
            "请总结这个仓库目前的用途",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run chat");
    assert!(chat.status.success(), "{:?}", chat);
    let chat_stdout = String::from_utf8_lossy(&chat.stdout);
    assert!(chat_stdout.contains("status: completed"), "{chat_stdout}");
    let run_id = chat_stdout
        .lines()
        .find_map(|line| line.strip_prefix("run: "))
        .expect("run id")
        .to_string();

    let run_show = command(bin, repo.path())
        .args([
            "run",
            "show",
            "--thread",
            &thread_id,
            &run_id,
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run show");
    assert!(run_show.status.success(), "{:?}", run_show);
    assert!(
        String::from_utf8_lossy(&run_show.stdout).contains("backend: codex"),
        "{:?}",
        run_show
    );

    let replay = command(bin, repo.path())
        .args([
            "replay",
            "--thread",
            &thread_id,
            &run_id,
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("replay");
    assert!(replay.status.success(), "{:?}", replay);
    let replay_stdout = String::from_utf8_lossy(&replay.stdout);
    assert!(replay_stdout.contains("Docker 沙箱已启动"), "{replay_stdout}");
}

#[test]
fn approval_flow_and_artifact_commands_work() {
    let repo = make_repo();
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let chat = command(bin, repo.path())
        .args([
            "chat",
            "--title",
            "修改文件",
            "请修改 file-a.txt 为 beta",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run chat");
    assert!(chat.status.success(), "{:?}", chat);
    let chat_stdout = String::from_utf8_lossy(&chat.stdout);
    assert!(chat_stdout.contains("status: waiting_for_input"), "{chat_stdout}");
    let thread_id = chat_stdout
        .lines()
        .find_map(|line| line.strip_prefix("thread: "))
        .expect("thread id")
        .to_string();

    let approval_list = command(bin, repo.path())
        .args([
            "approval",
            "list",
            "--thread",
            &thread_id,
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("approval list");
    assert!(approval_list.status.success(), "{:?}", approval_list);
    let approval_stdout = String::from_utf8_lossy(&approval_list.stdout);
    let approval_id = approval_stdout
        .lines()
        .next()
        .and_then(|line| line.split('\t').next())
        .expect("approval id")
        .to_string();

    let approve = command(bin, repo.path())
        .args([
            "approval",
            "approve",
            "--thread",
            &thread_id,
            &approval_id,
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("approval approve");
    assert!(approve.status.success(), "{:?}", approve);
    let approve_stdout = String::from_utf8_lossy(&approve.stdout);
    assert!(approve_stdout.contains("status: completed"), "{approve_stdout}");

    let thread_show = command(bin, repo.path())
        .args([
            "thread",
            "show",
            &thread_id,
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("thread show");
    assert!(thread_show.status.success(), "{:?}", thread_show);
    let thread_stdout = String::from_utf8_lossy(&thread_show.stdout);
    assert!(thread_stdout.contains("artifacts: "), "{thread_stdout}");

    let artifact_list = command(bin, repo.path())
        .args([
            "artifact",
            "list",
            "--thread",
            &thread_id,
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("artifact list");
    assert!(artifact_list.status.success(), "{:?}", artifact_list);
    let artifact_stdout = String::from_utf8_lossy(&artifact_list.stdout);
    assert!(artifact_stdout.contains("write-file:file-a.txt"), "{artifact_stdout}");
    let artifact_id = artifact_stdout
        .lines()
        .next()
        .and_then(|line| line.split('\t').next())
        .expect("artifact id")
        .to_string();

    let artifact_show = command(bin, repo.path())
        .args([
            "artifact",
            "show",
            "--thread",
            &thread_id,
            &artifact_id,
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("artifact show");
    assert!(artifact_show.status.success(), "{:?}", artifact_show);
}

#[test]
fn config_commands_work() {
    let repo = make_repo();
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let _ = fs::remove_file(repo.path().join("codex-forge.toml"));
    let init = command(bin, repo.path())
        .args([
            "config",
            "init",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("config init");
    assert!(init.status.success(), "{:?}", init);

    let show = command(bin, repo.path())
        .args([
            "config",
            "show",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("config show");
    assert!(show.status.success(), "{:?}", show);
    let show_stdout = String::from_utf8_lossy(&show.stdout);
    assert!(show_stdout.contains("docker_image"), "{show_stdout}");

    let validate = command(bin, repo.path())
        .args([
            "config",
            "validate",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("config validate");
    assert!(validate.status.success(), "{:?}", validate);
}

fn command(bin: &str, repo: &Path) -> Command {
    let mut cmd = Command::new(bin);
    let path = std::env::var("PATH").expect("PATH");
    let docker_state = std::env::temp_dir().join(format!(
        "codex-forge-fake-docker-{}",
        repo.file_name().and_then(|name| name.to_str()).unwrap_or("repo")
    ));
    let _ = fs::create_dir_all(&docker_state);
    cmd.env(
        "PATH",
        format!("{}:{}", repo.join(".fake-bin").display(), path),
    );
    cmd.env("CODEX_FAKE_DOCKER_STATE", &docker_state);
    cmd.current_dir(repo);
    cmd
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
