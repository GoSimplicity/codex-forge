mod support;

use support::{command, make_repo};

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
    assert!(
        replay_stdout.contains("Docker 沙箱已启动"),
        "{replay_stdout}"
    );
}
