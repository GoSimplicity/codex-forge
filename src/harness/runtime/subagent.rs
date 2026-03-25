use anyhow::Result;

use crate::harness::store::HarnessStore;
use crate::harness::types::{
    HarnessEvent, HarnessMessageRole, HarnessRunManifest, HarnessRunStatus, SubagentKind,
};

pub(super) fn execute_subagent(
    store: &HarnessStore,
    run: &HarnessRunManifest,
    kind: &SubagentKind,
    task: &str,
) -> Result<()> {
    let mut subagent = store.append_subagent(run, *kind, task.to_string())?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::SubagentStarted {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            subagent_id: subagent.id.clone(),
            kind: *kind,
        },
    )?;
    subagent.status = HarnessRunStatus::Completed;
    subagent.summary = Some(format!("{kind:?} 已分析任务：{task}"));
    store.update_subagent(run, &subagent)?;
    store.append_message(
        &run.thread_id,
        HarnessMessageRole::Summary,
        subagent.summary.clone().unwrap_or_default(),
        Some(run.id.clone()),
    )?;
    store.append_run_event(
        &run.thread_id,
        &run.id,
        HarnessEvent::SubagentCompleted {
            thread_id: run.thread_id.clone(),
            run_id: run.id.clone(),
            subagent_id: subagent.id,
            status: HarnessRunStatus::Completed,
        },
    )?;
    Ok(())
}
