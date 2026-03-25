mod support;

use support::{command, make_repo};

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
    assert!(
        artifact_stdout.contains("write-file:file-a.txt"),
        "{artifact_stdout}"
    );
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
