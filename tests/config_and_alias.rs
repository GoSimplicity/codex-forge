mod support;

use std::fs;

use support::{command, docker_state_dir, make_repo, write_global_config};

#[test]
fn cmd_alias_from_backend_is_supported() {
    let repo = make_repo();
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let chat = command(bin, repo.path())
        .args([
            "chat",
            "--title",
            "cmd alias",
            "请在沙箱里执行 pwd（使用cmd键）",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run chat");
    assert!(chat.status.success(), "{:?}", chat);
    let chat_stdout = String::from_utf8_lossy(&chat.stdout);
    assert!(chat_stdout.contains("status: completed"), "{chat_stdout}");
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
    assert!(
        approval_stdout.contains("当前没有待处理审批"),
        "{approval_stdout}"
    );
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

#[test]
fn global_config_commands_work() {
    let repo = make_repo();
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let init = command(bin, repo.path())
        .args(["config", "init", "--global"])
        .output()
        .expect("global config init");
    assert!(init.status.success(), "{:?}", init);

    let show = command(bin, repo.path())
        .args(["config", "show", "--global"])
        .output()
        .expect("global config show");
    assert!(show.status.success(), "{:?}", show);
    let show_stdout = String::from_utf8_lossy(&show.stdout);
    assert!(
        show_stdout.contains("provider = \"codex\""),
        "{show_stdout}"
    );
    assert!(!show_stdout.contains("sk-demo"), "{show_stdout}");

    let validate = command(bin, repo.path())
        .args(["config", "validate", "--global"])
        .output()
        .expect("global config validate");
    assert!(validate.status.success(), "{:?}", validate);
}

#[test]
fn global_config_set_updates_provider_to_codex() {
    let repo = make_repo();
    let bin = env!("CARGO_BIN_EXE_codex-forge");
    write_global_config(
        repo.path(),
        "[backend]\nprovider = \"openai_compatible\"\nkey = \"sk-demo\"\nbase_url = \"https://example.com/v1\"\nmodel = \"demo-model\"\nturn_timeout_secs = 3\n",
    );

    let set = command(bin, repo.path())
        .args(["config", "set", "--global", "backend.provider", "codex"])
        .output()
        .expect("global config set");
    assert!(set.status.success(), "{:?}", set);
    let set_stdout = String::from_utf8_lossy(&set.stdout);
    assert!(
        set_stdout.contains("backend.provider = \"codex\""),
        "{set_stdout}"
    );

    let show = command(bin, repo.path())
        .args(["config", "show", "--global"])
        .output()
        .expect("global config show");
    assert!(show.status.success(), "{:?}", show);
    let show_stdout = String::from_utf8_lossy(&show.stdout);
    assert!(
        show_stdout.contains("provider = \"codex\""),
        "{show_stdout}"
    );
}

#[test]
fn global_config_set_rejects_incomplete_openai_backend() {
    let repo = make_repo();
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let set = command(bin, repo.path())
        .args([
            "config",
            "set",
            "--global",
            "backend.provider",
            "openai_compatible",
        ])
        .output()
        .expect("global config set");
    assert!(!set.status.success(), "{:?}", set);
    let stderr = String::from_utf8_lossy(&set.stderr);
    assert!(stderr.contains("backend.key 不能为空"), "{stderr}");
}

#[test]
fn global_config_set_switches_to_openai_when_fields_are_complete() {
    let repo = make_repo();
    let bin = env!("CARGO_BIN_EXE_codex-forge");
    write_global_config(
        repo.path(),
        "[backend]\nprovider = \"codex\"\nkey = \"sk-demo\"\nbase_url = \"https://example.com/v1\"\nmodel = \"demo-model\"\nturn_timeout_secs = 3\n",
    );

    let set = command(bin, repo.path())
        .args([
            "config",
            "set",
            "--global",
            "backend.provider",
            "openai_compatible",
        ])
        .output()
        .expect("global config set");
    assert!(set.status.success(), "{:?}", set);

    let show = command(bin, repo.path())
        .args(["config", "show", "--global"])
        .output()
        .expect("global config show");
    assert!(show.status.success(), "{:?}", show);
    let show_stdout = String::from_utf8_lossy(&show.stdout);
    assert!(
        show_stdout.contains("provider = \"openai_compatible\""),
        "{show_stdout}"
    );
    assert!(!show_stdout.contains("sk-demo"), "{show_stdout}");
    assert!(show_stdout.contains("key = \"***\""), "{show_stdout}");
}

#[test]
fn global_validate_rejects_incomplete_openai_backend() {
    let repo = make_repo();
    let bin = env!("CARGO_BIN_EXE_codex-forge");
    write_global_config(
        repo.path(),
        "[backend]\nprovider = \"openai_compatible\"\nbase_url = \"https://example.com/v1\"\nmodel = \"demo-model\"\nturn_timeout_secs = 3\n",
    );

    let validate = command(bin, repo.path())
        .args(["config", "validate", "--global"])
        .output()
        .expect("global config validate");
    assert!(!validate.status.success(), "{:?}", validate);
    let stderr = String::from_utf8_lossy(&validate.stderr);
    assert!(stderr.contains("backend.key 不能为空"), "{stderr}");
}

#[test]
fn sandbox_uses_direct_mount_with_privileged_flags_and_run_shell_outputs_land_in_target_dir() {
    let repo = make_repo();
    fs::write(
        repo.path().join("codex-forge.toml"),
        "[runtime]\nrequire_tool_approval = true\nauto_approve_readonly = true\n",
    )
    .expect("write config");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let chat = command(bin, repo.path())
        .args([
            "chat",
            "--title",
            "生成产物",
            "请在沙箱里生成 out/result.txt",
            "--target-dir",
            repo.path().to_str().unwrap(),
        ])
        .output()
        .expect("run chat");
    assert!(chat.status.success(), "{:?}", chat);
    let chat_stdout = String::from_utf8_lossy(&chat.stdout);
    assert!(chat_stdout.contains("status: completed"), "{chat_stdout}");

    let docker_state = docker_state_dir(repo.path());
    let repo_root = repo.path().canonicalize().expect("canonical repo");
    let run_args = fs::read_dir(&docker_state)
        .expect("read docker state")
        .filter_map(|entry| entry.ok())
        .find(|entry| entry.file_name().to_string_lossy().ends_with(".run-args"))
        .map(|entry| fs::read_to_string(entry.path()).expect("read run args"))
        .expect("run args file");
    assert!(run_args.contains("--privileged"), "{run_args}");
    assert!(run_args.contains("--user 0:0"), "{run_args}");
    assert!(
        run_args.contains(&format!("-v {}:/workspace/repo", repo_root.display())),
        "{run_args}"
    );
    assert_eq!(
        fs::read_to_string(repo.path().join("out").join("result.txt")).expect("read artifact"),
        "artifact"
    );
}

#[test]
fn target_dir_is_resolved_as_the_exact_directory() {
    let repo = make_repo();
    let nested = repo.path().join("apps").join("demo");
    fs::create_dir_all(&nested).expect("mkdir nested");
    fs::write(nested.join("docs.md"), "nested\n").expect("write nested file");
    let bin = env!("CARGO_BIN_EXE_codex-forge");

    let created = command(bin, repo.path())
        .args([
            "thread",
            "new",
            "--title",
            "Nested Demo",
            "--target-dir",
            nested.to_str().unwrap(),
        ])
        .output()
        .expect("run thread new");
    assert!(created.status.success(), "{:?}", created);
    let stdout = String::from_utf8_lossy(&created.stdout);
    assert!(
        stdout.contains(&format!(
            "repo: {}",
            nested.canonicalize().expect("canonical").display()
        )),
        "{stdout}"
    );
}
