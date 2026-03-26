use anyhow::Result;

use crate::cli::ReplayArgs;
use crate::commands::format::describe_event;
use crate::config::load_app_config;
use crate::harness::HarnessStore;
use crate::workspace::resolve_target_dir;

pub fn run(args: ReplayArgs) -> Result<()> {
    let repo_root = resolve_target_dir(args.target_dir.as_deref())?.path;
    let config = load_app_config(&repo_root)?;
    let store = HarnessStore::new(&repo_root, config.backend.provider);
    let events = store.list_run_events(&args.thread, &args.run_id)?;
    if events.is_empty() {
        println!("当前 run 没有事件");
        return Ok(());
    }

    for event in events {
        println!(
            "{} {}",
            event.at.format("%Y-%m-%d %H:%M:%S"),
            describe_event(&event.payload)
        );
    }
    Ok(())
}
