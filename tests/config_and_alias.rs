mod support;

use std::fs;

use support::{command, make_repo};

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
    assert!(
        chat_stdout.contains("status: waiting_for_input"),
        "{chat_stdout}"
    );
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
    assert!(
        approve_stdout.contains("status: completed"),
        "{approve_stdout}"
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
