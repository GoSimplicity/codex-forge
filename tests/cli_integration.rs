use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn make_repo(case_name: &str) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    run(dir.path(), &["git", "init", "-b", "main"]);
    run(
        dir.path(),
        &["git", "config", "user.email", "codex@example.com"],
    );
    run(dir.path(), &["git", "config", "user.name", "Codex"]);
    fs::write(dir.path().join("README.md"), "seed\n").expect("write readme");
    fs::write(dir.path().join("file-a.txt"), "alpha\n").expect("write file-a");
    fs::write(
        dir.path().join("codex-forge.toml"),
        r#"
[defaults]
workers = 3
apply_mode = "auto-safe"
max_retries = 2
verification_commands = ["git status --short >/dev/null"]
"#,
    )
    .expect("write config");
    install_fake_codex(dir.path(), case_name);
    run(dir.path(), &["git", "add", "."]);
    run(dir.path(), &["git", "commit", "-m", "init"]);
    dir
}

fn make_unborn_repo(case_name: &str) -> (TempDir, TempDir) {
    let repo = TempDir::new().expect("temp repo");
    let bin = TempDir::new().expect("temp bin");
    run(repo.path(), &["git", "init", "-b", "main"]);
    install_fake_codex_to(bin.path(), case_name);
    (repo, bin)
}

fn install_fake_codex(repo: &Path, case_name: &str) {
    let bin_dir = repo.join(".fake-bin");
    install_fake_codex_to(&bin_dir, case_name);
}

fn install_fake_codex_to(bin_dir: &Path, case_name: &str) {
    fs::create_dir_all(bin_dir).expect("create bin dir");
    let script_path = bin_dir.join("codex");
    fs::write(
        &script_path,
        r#"#!/bin/sh
set -eu

mode=""
cwd="$PWD"
output=""
schema=""
json_mode=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    exec) shift ;;
    -C) cwd="$2"; shift 2 ;;
    -o) output="$2"; shift 2 ;;
    --output-schema) schema="$2"; shift 2 ;;
    --json) json_mode=1; shift ;;
    --skip-git-repo-check|--ephemeral|--color|--full-auto|-m)
      if [ "$1" = "--color" ] || [ "$1" = "-m" ]; then shift 2; else shift; fi ;;
    *)
      prompt="$1"
      shift ;;
  esac
done

cd "$cwd"
case_name="${CODEX_FIXTURE_CASE:-success}"
state_dir="${CODEX_FIXTURE_STATE:-$cwd/.codex-state}"
mkdir -p "$state_dir"
agent_id="$(basename "$cwd")"

if [ -n "${schema}" ] || printf "%s" "${prompt:-}" | grep -q "最终输出必须是一个合法 JSON 对象"; then
  if printf "%s" "${prompt:-}" | grep -q "最终总结"; then
    cat > "$output" <<'JSON'
{"overview":"fake summary","highlights":["h1"],"risks":[],"conflicts":[],"next_steps":["n1"]}
JSON
    exit 0
  fi

  if printf "%s" "${prompt:-}" | grep -q "面向用户可读"; then
    cat > "$output" <<'JSON'
{"summary":"todo summary","approach":"todo approach","risks":["r1"],"todos":[
  {"title":"明确范围","goal":"先明确边界","details":["梳理目标"],"dependencies":[],"completion_criteria":["完成范围确认"]},
  {"title":"实现主干","goal":"完成主路径","details":["搭建主功能"],"dependencies":["明确范围"],"completion_criteria":["主链路可运行"]}
]}
JSON
    exit 0
  fi

  if [ "$case_name" = "conflict" ]; then
    cat > "$output" <<'JSON'
{"summary":"plan","strategy":"conflict","nodes":[
  {"title":"实现A","role":"implementer","objective":"改 file-a","deliverables":["patch"],"dependencies":[],"prompt_focus":"改同一文件","input_artifacts":[],"output_artifacts":["handoff"],"completion_criteria":["完成"]},
  {"title":"实现B","role":"implementer","objective":"改 file-a","deliverables":["patch"],"dependencies":[],"prompt_focus":"改同一文件","input_artifacts":[],"output_artifacts":["handoff"],"completion_criteria":["完成"]},
  {"title":"审阅","role":"reviewer","objective":"审阅","deliverables":["review"],"dependencies":["implementer-1","implementer-2"],"prompt_focus":"找冲突","input_artifacts":["handoff"],"output_artifacts":["review"],"completion_criteria":["完成"]}
]}
JSON
  elif [ "$case_name" = "title-dependency" ]; then
    cat > "$output" <<'JSON'
{"summary":"plan","strategy":"title-dependency","nodes":[
  {"title":"架构设计","role":"architect","objective":"拆图","deliverables":["plan"],"dependencies":[],"prompt_focus":"拆图","input_artifacts":[],"output_artifacts":["handoff"],"completion_criteria":["完成"]},
  {"title":"实现主干","role":"implementer","objective":"改 file-a","deliverables":["patch"],"dependencies":["架构设计"],"prompt_focus":"改主文件","input_artifacts":["handoff"],"output_artifacts":["handoff"],"completion_criteria":["完成"]},
  {"title":"审阅","role":"reviewer","objective":"审阅","deliverables":["review"],"dependencies":["实现主干"],"prompt_focus":"找问题","input_artifacts":["handoff"],"output_artifacts":["review"],"completion_criteria":["完成"]}
]}
JSON
  else
    cat > "$output" <<'JSON'
{"summary":"plan","strategy":"success","nodes":[
  {"title":"实现主干","role":"implementer","objective":"改 file-a","deliverables":["patch"],"dependencies":[],"prompt_focus":"改主文件","input_artifacts":[],"output_artifacts":["handoff"],"completion_criteria":["完成"]},
  {"title":"审阅","role":"reviewer","objective":"审阅","deliverables":["review"],"dependencies":["implementer-1"],"prompt_focus":"找问题","input_artifacts":["handoff"],"output_artifacts":["review"],"completion_criteria":["完成"]}
]}
JSON
  fi
  echo '{"type":"planner.done","message":"ok"}'
  echo 'planner noise' >&2
  exit 0
fi

if [ "$case_name" = "retry" ] && [ "$agent_id" = "implementer-1" ]; then
  marker="$state_dir/retry-once"
  if [ ! -f "$marker" ]; then
    touch "$marker"
    echo "temporary network timeout" >&2
    exit 1
  fi
fi

if [ "$case_name" = "needs-context" ]; then
  if [ ! -f "Cargo.toml" ] || [ ! -f "README.md" ]; then
    echo "missing materialized repo context" >&2
    exit 1
  fi
fi

if [ "$agent_id" = "implementer-1" ]; then
  if [ "$case_name" = "conflict" ]; then
    printf 'from implementer 1\n' > file-a.txt
  else
    printf 'from implementer 1\n' >> file-a.txt
  fi
  if [ "$case_name" = "failed-patch" ]; then
    cat > "$output" <<EOF
# 交付摘要
${agent_id} partial
# 变更清单
- touched file-a.txt
# 风险
- 中途失败
# 验证
- fake verify
# 交接
- downstream inspect
EOF
    echo "simulated worker failure after patch" >&2
    exit 1
  fi
elif [ "$agent_id" = "implementer-2" ]; then
  printf 'from implementer 2\n' > file-a.txt
elif [ "$agent_id" = "reviewer-1" ]; then
  decision="allow"
  if [ "$case_name" = "reviewer-block" ]; then
    decision="block"
  elif [ "$case_name" = "reviewer-needs-materialized" ]; then
    if grep -q "from implementer 1" file-a.txt; then
      decision="allow"
    else
      decision="block"
    fi
  fi
  cat > "$output" <<EOF
# 交付摘要
${agent_id} ${decision}
# 变更清单
- reviewed worker outputs
# 风险
- 无
# 验证
- fake verify
# 交接
- APPLY_DECISION: ${decision}
- reviewer ${decision}
EOF
  if [ "$json_mode" -eq 1 ]; then
    echo '{"type":"turn.started","message":"running"}'
    echo 'mixed stderr noise' >&2
  fi
  exit 0
fi

cat > "$output" <<EOF
# 交付摘要
${agent_id} done
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
  echo 'mixed stderr noise' >&2
fi
"#,
    )
    .expect("write fake codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
    }
    fs::write(bin_dir.join(".case"), case_name).expect("write case");
}

#[test]
fn config_validate_and_doctor_work() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let validate = command(bin, repo.path())
        .args([
            "config",
            "validate",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run config validate");
    assert!(validate.status.success(), "{:?}", validate);
    assert!(String::from_utf8_lossy(&validate.stdout).contains("配置有效"));

    let doctor = command(bin, repo.path())
        .args(["doctor", "--target-dir", repo.path().to_str().unwrap()])
        .output()
        .expect("run doctor");
    assert!(doctor.status.success(), "{:?}", doctor);
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("doctor 通过"));
}

#[test]
fn doctor_demo_outputs_readiness_and_recommendation() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let doctor = command(bin, repo.path())
        .args([
            "doctor",
            "--demo",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run doctor demo");
    assert!(doctor.status.success(), "{:?}", doctor);
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(stdout.contains("doctor 结论"));
    assert!(stdout.contains("推荐 role_set"));
}

#[test]
fn run_auto_safe_applies_and_replay() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let run_output = command(bin, repo.path())
        .args([
            "run",
            "实现一个测试任务",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run forge");
    assert!(run_output.status.success(), "{:?}", run_output);

    let file_a = fs::read_to_string(repo.path().join("file-a.txt")).expect("read file-a");
    assert!(file_a.contains("from implementer 1"));

    let manifest = load_manifest(repo.path());
    assert_eq!(manifest["apply_result"]["trust_level"], "high");
    assert_eq!(manifest["change_trust_report"]["trust_level"], "high");
    assert_eq!(
        manifest["final_summary"]["accepted_files"]
            .as_array()
            .expect("accepted files"),
        &vec![Value::String("file-a.txt".to_string())]
    );
    assert!(manifest["execution_contract"]["task_fingerprint"].is_string());

    let session_id = latest_session_id(repo.path());
    let replay_output = command(bin, repo.path())
        .args([
            "replay",
            &session_id,
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("replay");
    assert!(replay_output.status.success(), "{:?}", replay_output);
    assert!(String::from_utf8_lossy(&replay_output.stdout).contains("回放完成"));

    let timeline_output = command(bin, repo.path())
        .args([
            "replay",
            &session_id,
            "--timeline",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("replay timeline");
    assert!(timeline_output.status.success(), "{:?}", timeline_output);
    let timeline_text = String::from_utf8_lossy(&timeline_output.stdout);
    assert!(
        timeline_text.contains("方案完成") || timeline_text.contains("子任务开始"),
        "{}",
        timeline_text
    );
}

#[test]
fn run_apply_mode_none_outputs_review_package() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "run",
            "只做审阅决策",
            "--apply-mode",
            "none",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run forge");
    assert_undelivered_run(&output);

    let file_a = fs::read_to_string(repo.path().join("file-a.txt")).expect("read file-a");
    assert_eq!(file_a, "alpha\n");

    let manifest = load_manifest(repo.path());
    assert_eq!(manifest["apply_result"]["status"], "skipped");
    assert_eq!(manifest["final_summary"]["result_status"], "failed");
    assert_eq!(
        manifest["apply_result"]["accepted_files"]
            .as_array()
            .expect("accepted files"),
        &vec![Value::String("file-a.txt".to_string())]
    );
    assert!(
        manifest["change_trust_report"]["safe_to_auto_apply"]
            .as_bool()
            .is_some()
    );
}

#[test]
fn run_retries_then_succeeds() {
    let repo = make_repo("retry");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "run",
            "带重试的任务",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run forge");
    assert!(output.status.success(), "{:?}", output);

    let manifest = load_manifest(repo.path());
    let attempts = manifest["worker_results"]
        .as_array()
        .expect("worker results")
        .iter()
        .find(|item| item["agent_id"] == "implementer-1")
        .and_then(|item| item["attempts"].as_u64())
        .expect("attempts");
    assert_eq!(attempts, 2);
}

#[test]
fn run_with_feature_demo_preset_persists_preset() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "run",
            "黑客松主路径",
            "--preset",
            "feature-demo",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run preset");
    assert!(output.status.success(), "{:?}", output);

    let manifest = load_manifest(repo.path());
    assert_eq!(manifest["preset"], "feature-demo");
    assert!(manifest["demo_summary"].as_array().is_some());
}

#[test]
fn conflict_marks_auto_apply_failed() {
    let repo = make_repo("conflict");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "run",
            "冲突任务",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run forge");
    assert_undelivered_run(&output);

    let manifest = load_manifest(repo.path());
    assert_eq!(manifest["apply_result"]["status"], "sync_failed");
}

#[test]
fn reviewer_block_stops_auto_apply() {
    let repo = make_repo("reviewer-block");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "run",
            "reviewer gate 阻止应用",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run forge");
    assert!(output.status.success(), "{:?}", output);

    let manifest = load_manifest(repo.path());
    assert_eq!(manifest["apply_result"]["status"], "applied");
    let conflicts = manifest["apply_result"]["conflicts"]
        .as_array()
        .expect("conflicts")
        .iter()
        .filter_map(|item| item.as_str())
        .collect::<Vec<_>>();
    assert!(
        conflicts
            .iter()
            .any(|item| item.contains("明确阻止自动应用")),
        "{conflicts:?}"
    );

    let file_a = fs::read_to_string(repo.path().join("file-a.txt")).expect("read file-a");
    assert!(file_a.contains("from implementer 1"));
}

#[test]
fn reviewer_receives_materialized_dependency_changes() {
    let repo = make_repo("reviewer-needs-materialized");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "run",
            "reviewer 应看到上游候选改动",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run forge");
    assert!(output.status.success(), "{:?}", output);

    let manifest = load_manifest(repo.path());
    assert_eq!(manifest["apply_result"]["status"], "applied");
    assert_eq!(manifest["apply_result"]["review_gate"], "allow_full");
    assert_eq!(
        manifest["final_summary"]["open_risks"]
            .as_array()
            .expect("open risks"),
        &vec![Value::String("未发现必须立即阻断的开放风险。".to_string())]
    );

    let file_a = fs::read_to_string(repo.path().join("file-a.txt")).expect("read file-a");
    assert!(file_a.contains("from implementer 1"));
}

#[test]
fn failed_worker_patch_is_excluded_from_apply_plan() {
    let repo = make_repo("failed-patch");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "run",
            "失败 patch 不应进入 apply",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run forge");
    assert_undelivered_run(&output);

    let session_id = latest_session_id(repo.path());
    let apply_plan_path = repo
        .path()
        .join(".codex-forge")
        .join("sessions")
        .join(session_id)
        .join("integration")
        .join("apply-plan.json");
    let apply_plan: Value =
        serde_json::from_str(&fs::read_to_string(apply_plan_path).expect("read apply plan"))
            .expect("parse apply plan");
    assert_eq!(
        apply_plan["operations"]
            .as_array()
            .expect("operations")
            .len(),
        0
    );

    let manifest = load_manifest(repo.path());
    assert_eq!(manifest["apply_result"]["status"], "sync_failed");
    let file_a = fs::read_to_string(repo.path().join("file-a.txt")).expect("read file-a");
    assert_eq!(file_a, "alpha\n");
}

#[test]
fn planner_title_dependencies_are_normalized() {
    let repo = make_repo("title-dependency");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "run",
            "标题依赖应被归一化",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run forge");
    assert!(output.status.success(), "{:?}", output);

    let manifest = load_manifest(repo.path());
    let graph_nodes = manifest["execution_graph"]["nodes"]
        .as_array()
        .expect("graph nodes");
    let implementer = graph_nodes
        .iter()
        .find(|item| item["role"] == "implementer")
        .expect("implementer");
    let reviewer = graph_nodes
        .iter()
        .find(|item| item["role"] == "reviewer")
        .expect("reviewer");

    assert_eq!(
        implementer["dependencies"].as_array().expect("deps"),
        &vec![Value::String("architect-1".to_string())]
    );
    assert_eq!(
        reviewer["dependencies"].as_array().expect("deps"),
        &vec![Value::String("implementer-1".to_string())]
    );
}

#[test]
fn config_validate_stays_as_non_mutating_check() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "config",
            "validate",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run config validate");
    assert!(output.status.success(), "{:?}", output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("配置有效"));
    assert!(!repo.path().join(".codex-forge").exists());
}

#[test]
fn run_writes_user_facing_todo_artifacts() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "run",
            "创建一个简单博客",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run session");
    assert!(output.status.success(), "{:?}", output);

    let session_id = latest_session_id(repo.path());
    let todo_path = repo
        .path()
        .join(".codex-forge")
        .join("sessions")
        .join(session_id)
        .join("commander")
        .join("plan-todo.json");
    let todo: Value =
        serde_json::from_str(&fs::read_to_string(todo_path).expect("read todo")).expect("todo");
    assert_eq!(todo["summary"], "todo summary");
    assert_eq!(todo["todos"].as_array().expect("todos").len(), 2);
}

#[test]
fn run_generates_plan_artifacts_inside_run_session() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let run_output = command(bin, repo.path())
        .args([
            "run",
            "创建一个简单博客",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run with reused plan");
    assert!(run_output.status.success(), "{:?}", run_output);

    let manifest = load_manifest(repo.path());
    assert_eq!(manifest["source_plan_session_id"].as_str(), None);
    assert_eq!(manifest["reused_plan_session_id"].as_str(), None);
    assert_eq!(manifest["session_kind"], "run");
    assert_eq!(manifest["plan_todo"]["summary"], "todo summary");
}

#[test]
fn plan_subcommand_is_rejected() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "plan",
            "创建一个简单博客",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run removed plan");
    assert!(!output.status.success(), "{:?}", output);
}

#[test]
fn run_rejects_from_plan_flag() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "run",
            "创建一个简单博客",
            "--from-plan",
            "plan-123",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run removed from-plan");
    assert!(!output.status.success(), "{:?}", output);
}

#[test]
fn run_can_resume_previous_session() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let first = command(bin, repo.path())
        .args([
            "run",
            "创建一个简单博客",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("first run");
    assert!(first.status.success(), "{:?}", first);
    let first_session_id = latest_session_id(repo.path());

    let resumed = command(bin, repo.path())
        .args([
            "run",
            "创建一个简单博客",
            "--resume",
            &first_session_id,
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("resume run");
    assert_undelivered_run(&resumed);

    let manifest = load_manifest(repo.path());
    assert_eq!(
        manifest["resumed_from_session_id"].as_str(),
        Some(first_session_id.as_str())
    );
}

#[test]
fn continue_from_run_creates_iteration_artifacts() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let first = command(bin, repo.path())
        .args([
            "run",
            "创建一个简单博客",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("first run");
    assert!(first.status.success(), "{:?}", first);
    let parent_session_id = latest_session_id(repo.path());

    let continued = command(bin, repo.path())
        .args([
            "continue",
            "--session",
            &parent_session_id,
            "--feedback",
            "把第一版计划再收敛一下，突出下一步优化项",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("continue run");
    assert!(continued.status.success(), "{:?}", continued);

    let manifest = load_manifest(repo.path());
    assert_eq!(
        manifest["parent_session_id"].as_str(),
        Some(parent_session_id.as_str())
    );
    assert_eq!(
        manifest["root_session_id"].as_str(),
        Some(parent_session_id.as_str())
    );
    assert_eq!(manifest["continuation_kind"], "run_refine");
    assert_eq!(manifest["iteration_index"], 2);
    assert_eq!(manifest["plan_todo"]["iteration_index"], 2);
    assert_eq!(
        manifest["feedback_history"]
            .as_array()
            .expect("feedback")
            .len(),
        1
    );

    let session_id = latest_session_id(repo.path());
    let session_dir = repo
        .path()
        .join(".codex-forge")
        .join("sessions")
        .join(&session_id);
    assert!(session_dir.join("commander").join("feedback.json").exists());
    assert!(session_dir.join("commander").join("feedback.md").exists());
    assert!(
        session_dir
            .join("commander")
            .join("session-lineage.json")
            .exists()
    );
    assert!(
        repo.path()
            .join(".codex-forge")
            .join("sessions")
            .join(parent_session_id)
            .join("latest.md")
            .exists()
    );
}

#[test]
fn shared_memory_is_persisted_and_injected_into_worker_prompt() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let first = command(bin, repo.path())
        .args([
            "run",
            "创建一个简单博客",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("first run");
    assert!(first.status.success(), "{:?}", first);
    let parent_session_id = latest_session_id(repo.path());

    let second = command(bin, repo.path())
        .args([
            "continue",
            "--session",
            &parent_session_id,
            "--mode",
            "run",
            "--feedback",
            "补充验证说明，并继承上一轮的稳定结论",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("continue run");
    assert!(second.status.success(), "{:?}", second);

    let session_id = latest_session_id(repo.path());
    let memory_root = repo.path().join(".codex-forge").join("memory");
    let shared_index_path = memory_root.join("shared").join("index.json");
    let session_memory_dir = memory_root.join("session").join(&session_id);
    let worker_prompt_path = repo
        .path()
        .join(".codex-forge")
        .join("sessions")
        .join(&session_id)
        .join("workers")
        .join("implementer-1")
        .join("prompt.md");

    assert!(shared_index_path.exists());
    assert!(session_memory_dir.join("manifest.json").exists());
    assert!(session_memory_dir.join("entries.json").exists());
    assert!(session_memory_dir.join("task-brief.md").exists());
    assert!(
        session_memory_dir
            .join("views")
            .join("implementer-1.md")
            .exists()
    );

    let shared_index: Value =
        serde_json::from_str(&fs::read_to_string(&shared_index_path).expect("read shared index"))
            .expect("parse shared index");
    assert!(
        shared_index
            .as_array()
            .expect("shared entries")
            .iter()
            .any(|item| item["kind"] == "summary")
    );

    let prompt = fs::read_to_string(worker_prompt_path).expect("read prompt");
    assert!(prompt.contains("共享记忆视图"));
    assert!(prompt.contains("补充验证说明"));

    let manifest = load_manifest(repo.path());
    assert_eq!(manifest["shared_context_version"], 1);
    assert!(manifest["memory_manifest"].is_object());
    assert!(
        manifest["artifact_index"]
            .as_array()
            .expect("artifact index")
            .iter()
            .any(|item| item["key"] == "shared_memory_index")
    );
}

#[test]
fn auto_safe_supports_unborn_repo() {
    let (repo, fake_bin) = make_unborn_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");
    let config_path = fake_bin.path().join("codex-forge.toml");
    fs::write(
        &config_path,
        r#"
[defaults]
verification_commands = ["git status --short >/dev/null"]
"#,
    )
    .expect("write unborn config");

    let output = command_with_bin(bin, repo.path(), fake_bin.path())
        .args([
            "run",
            "初始化新仓库",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
            "--config",
            config_path.to_str().unwrap(),
        ])
        .output()
        .expect("run forge");
    assert!(output.status.success(), "{:?}", output);

    let file_a = fs::read_to_string(repo.path().join("file-a.txt")).expect("read file-a");
    assert!(file_a.contains("from implementer 1"));
}

#[test]
fn unborn_repo_materializes_source_context_for_workers() {
    let (repo, fake_bin) = make_unborn_repo("needs-context");
    let bin = env!("CARGO_BIN_EXE_codex-forge");
    let config_path = fake_bin.path().join("codex-forge.toml");
    fs::write(
        repo.path().join("README.md"),
        "# unborn context\nreal files should be visible to workers\n",
    )
    .expect("write readme");
    fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"smoke\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write cargo");
    fs::write(
        &config_path,
        r#"
[defaults]
verification_commands = ["git status --short >/dev/null"]
"#,
    )
    .expect("write unborn config");

    let output = command_with_bin(bin, repo.path(), fake_bin.path())
        .args([
            "run",
            "验证 unborn worktree 物化",
            "--apply-mode",
            "none",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
            "--config",
            config_path.to_str().unwrap(),
        ])
        .output()
        .expect("run forge");
    assert_undelivered_run(&output);

    let manifest = load_manifest(repo.path());
    assert_eq!(manifest["final_summary"]["apply_status"], "skipped");
    assert_eq!(manifest["final_summary"]["result_status"], "failed");
}

#[test]
fn clean_session_removes_selected_history_and_rewrites_latest_pointer() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let first = command(bin, repo.path())
        .args([
            "run",
            "创建一个简单博客",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("first run");
    assert!(first.status.success(), "{:?}", first);
    let root_session_id = latest_session_id(repo.path());

    let second = command(bin, repo.path())
        .args([
            "continue",
            "--session",
            &root_session_id,
            "--feedback",
            "继续细化这版实现",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("continue run");
    assert!(second.status.success(), "{:?}", second);
    let child_session_id = latest_session_id(repo.path());

    let clean = command(bin, repo.path())
        .args([
            "clean",
            "--session",
            &child_session_id,
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("clean session");
    assert!(clean.status.success(), "{:?}", clean);

    assert!(!session_dir(repo.path(), &child_session_id).exists());
    assert!(session_dir(repo.path(), &root_session_id).exists());

    let latest = fs::read_to_string(session_dir(repo.path(), &root_session_id).join("latest.md"))
        .expect("read latest pointer");
    assert!(latest.contains(&format!("latest_session: `{}`", root_session_id)));
}

#[test]
fn clean_all_removes_entire_codex_forge_dir() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "run",
            "创建一个简单博客",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run forge");
    assert!(output.status.success(), "{:?}", output);
    assert!(repo.path().join(".codex-forge").exists());

    let clean = command(bin, repo.path())
        .args([
            "clean",
            "--all",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("clean all");
    assert!(clean.status.success(), "{:?}", clean);
    assert!(!repo.path().join(".codex-forge").exists());
}

#[test]
fn reset_session_rolls_back_commits_and_removes_history() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "run",
            "创建一个简单博客",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run forge");
    assert!(output.status.success(), "{:?}", output);

    let session_id = latest_session_id(repo.path());
    assert_ne!(
        git_stdout(repo.path(), &["rev-list", "--count", "HEAD"]),
        "1".to_string()
    );

    let reset = command(bin, repo.path())
        .args([
            "reset",
            "--session",
            &session_id,
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("reset session");
    assert!(reset.status.success(), "{:?}", reset);

    let file_a = fs::read_to_string(repo.path().join("file-a.txt")).expect("read file-a");
    assert_eq!(file_a, "alpha\n");
    assert_eq!(
        git_stdout(repo.path(), &["rev-list", "--count", "HEAD"]),
        "1"
    );
    assert_eq!(
        git_stdout(repo.path(), &["rev-parse", "--short", "HEAD"]),
        git_stdout(
            repo.path(),
            &["rev-list", "--max-count", "1", "--abbrev-commit", "HEAD"]
        )
    );
    assert!(!session_dir(repo.path(), &session_id).exists());
}

fn command(bin: &str, repo: &Path) -> Command {
    command_with_bin(bin, repo, &repo.join(".fake-bin"))
}

fn command_with_bin(bin: &str, repo: &Path, bin_dir: &Path) -> Command {
    let mut cmd = Command::new(bin);
    let state_dir = std::env::temp_dir().join(format!(
        "codex-forge-state-{}",
        repo.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repo")
    ));
    let home_dir = std::env::temp_dir().join(format!(
        "codex-forge-home-{}",
        repo.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repo")
    ));
    let _ = fs::create_dir_all(&state_dir);
    let _ = fs::create_dir_all(&home_dir);
    let path = std::env::var("PATH").expect("PATH");
    cmd.env("PATH", format!("{}:{}", bin_dir.display(), path));
    cmd.env("CODEX_FIXTURE_STATE", state_dir);
    cmd.env("CODEX_FORGE_HOME", home_dir);
    if let Ok(case_name) = fs::read_to_string(bin_dir.join(".case")) {
        cmd.env("CODEX_FIXTURE_CASE", case_name.trim());
    }
    cmd.current_dir(repo);
    cmd
}

fn latest_session_id(repo: &Path) -> String {
    fs::read_dir(repo.join(".codex-forge").join("sessions"))
        .expect("read sessions")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .max()
        .expect("latest session")
}

fn session_dir(repo: &Path, session_id: &str) -> PathBuf {
    repo.join(".codex-forge").join("sessions").join(session_id)
}

fn assert_undelivered_run(output: &std::process::Output) {
    assert!(!output.status.success(), "{:?}", output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("代码未交付到目标目录"), "{stderr}");
}

fn load_manifest(repo: &Path) -> Value {
    let session_id = latest_session_id(repo);
    let manifest_path = repo
        .join(".codex-forge")
        .join("sessions")
        .join(session_id)
        .join("manifest.json");
    serde_json::from_str(&fs::read_to_string(manifest_path).expect("read manifest"))
        .expect("parse manifest")
}

fn run(dir: &Path, args: &[&str]) {
    let status = Command::new(args[0])
        .args(&args[1..])
        .current_dir(dir)
        .status()
        .expect("run command");
    assert!(status.success(), "command failed: {:?}", args);
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(output.status.success(), "git failed: {:?}", args);
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn current_dir_is_used_without_repassing_flag() {
    let repo = make_repo("success");
    let other = TempDir::new().expect("other cwd");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    fs::write(
        other.path().join("codex-forge.toml"),
        r#"
[defaults]
workers = 2
apply_mode = "bundle"
max_retries = 1
verification_commands = ["pwd >/dev/null"]
"#,
    )
    .expect("write other config");

    let mut second = command(bin, repo.path());
    second.current_dir(other.path());
    let second = second
        .args(["config", "validate"])
        .output()
        .expect("config validate without target");
    assert!(second.status.success(), "{:?}", second);
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(stdout.contains("默认 workers：2"), "{stdout}");
    assert!(stdout.contains("默认 apply_mode：bundle"), "{stdout}");
}

#[test]
fn repo_root_is_used_when_invoked_from_subdir() {
    let repo = make_repo("success");
    let subdir = repo.path().join("nested").join("deeper");
    fs::create_dir_all(&subdir).expect("create subdir");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let mut cmd = command(bin, repo.path());
    cmd.current_dir(&subdir);
    let output = cmd
        .args(["config", "validate"])
        .output()
        .expect("config validate from subdir");
    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("默认 workers：3"), "{stdout}");
    assert!(
        stdout.contains(&repo.path().join("codex-forge.toml").display().to_string()),
        "{stdout}"
    );
}

#[test]
fn run_creates_local_commits_for_each_todo() {
    let repo = make_repo("success");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let output = command(bin, repo.path())
        .args([
            "run",
            "创建一个简单博客",
            "--ui",
            "minimal",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run forge");
    assert!(output.status.success(), "{:?}", output);

    let count = Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .expect("count commits");
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "3");

    let subjects = Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["log", "--pretty=%s", "-2"])
        .output()
        .expect("log subjects");
    let text = String::from_utf8_lossy(&subjects.stdout);
    assert!(text.contains("todo-1"));
    assert!(text.contains("todo-2"));
}
