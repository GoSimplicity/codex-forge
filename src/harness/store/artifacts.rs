use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;

use crate::harness::types::{ArtifactKind, ArtifactRecord};

use super::HarnessStore;
use super::ids::make_id;
use super::jsonl::{append_jsonl, read_jsonl};

impl HarnessStore {
    #[allow(clippy::too_many_arguments)]
    pub fn append_artifact(
        &self,
        thread_id: &str,
        run_id: &str,
        task_node_id: Option<String>,
        subagent_id: Option<String>,
        label: String,
        kind: ArtifactKind,
        path: PathBuf,
    ) -> Result<ArtifactRecord> {
        let run = self.load_run(thread_id, run_id)?;
        let thread = self.load_thread(thread_id)?;
        let artifact = ArtifactRecord {
            id: make_id("artifact"),
            thread_id: thread_id.to_string(),
            run_id: run_id.to_string(),
            task_node_id,
            subagent_id,
            label,
            kind,
            path,
            created_at: Utc::now(),
        };
        append_jsonl(&run.artifacts_path, &artifact)?;
        append_jsonl(&thread.artifacts_dir.join("index.jsonl"), &artifact)?;
        Ok(artifact)
    }

    pub fn list_artifacts(
        &self,
        thread_id: Option<&str>,
        run_id: Option<&str>,
    ) -> Result<Vec<ArtifactRecord>> {
        if let (Some(thread_id), Some(run_id)) = (thread_id, run_id) {
            let run = self.load_run(thread_id, run_id)?;
            let mut artifacts: Vec<ArtifactRecord> = read_jsonl(&run.artifacts_path)?;
            artifacts.sort_by(|left, right| right.created_at.cmp(&left.created_at));
            return Ok(artifacts);
        }
        if let Some(thread_id) = thread_id {
            let thread = self.load_thread(thread_id)?;
            let mut artifacts: Vec<ArtifactRecord> =
                read_jsonl(&thread.artifacts_dir.join("index.jsonl"))?;
            artifacts.sort_by(|left, right| right.created_at.cmp(&left.created_at));
            return Ok(artifacts);
        }

        let mut artifacts = Vec::new();
        for thread in self.list_threads()? {
            artifacts.extend(self.list_artifacts(Some(&thread.id), None)?);
        }
        artifacts.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(artifacts)
    }
}
