use crate::memories::raw_memories_file;
use crate::memories::rollout_summaries_dir;
use crate::memories::storage::rollout_summary_file_stem;
use codex_state::Stage1Output;
use std::collections::BTreeSet;
use std::path::Path;

const MIN_MEMORY_MD_CHARS: usize = 120;
const MIN_MEMORY_SUMMARY_CHARS: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ArtifactValidationIssue {
    pub(super) code: &'static str,
    pub(super) message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ArtifactValidationReport {
    pub(super) issues: Vec<ArtifactValidationIssue>,
}

impl ArtifactValidationReport {
    pub(super) fn is_ok(&self) -> bool {
        self.issues.is_empty()
    }

    fn push(&mut self, code: &'static str, message: impl Into<String>) {
        self.issues.push(ArtifactValidationIssue {
            code,
            message: message.into(),
        });
    }
}

pub(super) async fn validate_post_consolidation_artifacts(
    root: &Path,
    artifact_memories: &[Stage1Output],
) -> std::io::Result<ArtifactValidationReport> {
    let mut report = ArtifactValidationReport::default();
    if artifact_memories.is_empty() {
        return Ok(report);
    }

    let expected_rollout_files = artifact_memories
        .iter()
        .map(|memory| format!("{}.md", rollout_summary_file_stem(memory)))
        .collect::<BTreeSet<_>>();

    let memory_path = root.join("MEMORY.md");
    let memory_summary_path = root.join("memory_summary.md");
    let raw_memories_path = raw_memories_file(root);

    let memory_contents =
        read_required_text_artifact(&mut report, "missing_memory", &memory_path, "MEMORY.md")
            .await?;
    let memory_summary_contents = read_required_text_artifact(
        &mut report,
        "missing_memory_summary",
        &memory_summary_path,
        "memory_summary.md",
    )
    .await?;
    let raw_memories_contents = read_required_text_artifact(
        &mut report,
        "missing_raw_memories",
        &raw_memories_path,
        "raw_memories.md",
    )
    .await?;

    if let Some(raw_memories) = raw_memories_contents.as_deref() {
        validate_raw_memories_contents(&mut report, raw_memories);
    }

    let actual_rollout_files = read_rollout_summary_directory(&mut report, root).await?;

    for expected in &expected_rollout_files {
        if !actual_rollout_files.contains(expected) {
            report.push(
                "missing_rollout_summary_file",
                format!(
                    "expected rollout summary file is missing: {}",
                    rollout_summaries_dir(root).join(expected).display()
                ),
            );
        }
    }

    for unexpected in actual_rollout_files.difference(&expected_rollout_files) {
        report.push(
            "unexpected_rollout_summary_file",
            format!(
                "rollout_summaries contains a file outside the current Phase 2 artifact set: {}",
                rollout_summaries_dir(root).join(unexpected).display()
            ),
        );
    }

    if let Some(memory) = memory_contents.as_deref() {
        validate_memory_contents(
            &mut report,
            memory,
            root,
            &actual_rollout_files,
            &expected_rollout_files,
        );
    }

    if let Some(memory_summary) = memory_summary_contents.as_deref() {
        validate_memory_summary_contents(&mut report, memory_summary);
    }

    report.issues.sort_by(|left, right| {
        left.code
            .cmp(right.code)
            .then_with(|| left.message.cmp(&right.message))
    });
    Ok(report)
}

async fn read_required_text_artifact(
    report: &mut ArtifactValidationReport,
    missing_code: &'static str,
    path: &Path,
    display_name: &'static str,
) -> std::io::Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            report.push(
                missing_code,
                format!("required artifact is missing: {}", path.display()),
            );
            Ok(None)
        }
        Err(err) => Err(std::io::Error::new(
            err.kind(),
            format!("failed reading {display_name} at {}: {err}", path.display()),
        )),
    }
}

async fn read_rollout_summary_directory(
    report: &mut ArtifactValidationReport,
    root: &Path,
) -> std::io::Result<BTreeSet<String>> {
    let dir = rollout_summaries_dir(root);
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            report.push(
                "missing_rollout_summaries_dir",
                format!("rollout_summaries directory is missing: {}", dir.display()),
            );
            return Ok(BTreeSet::new());
        }
        Err(err) => {
            return Err(std::io::Error::new(
                err.kind(),
                format!(
                    "failed reading rollout_summaries directory {}: {err}",
                    dir.display()
                ),
            ));
        }
    };

    let mut files = BTreeSet::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let file_type = entry.file_type().await?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        if !file_type.is_file() {
            report.push(
                "malformed_rollout_summary_entry",
                format!(
                    "rollout_summaries contains a non-file entry: {}",
                    path.display()
                ),
            );
            continue;
        }
        if !file_name.ends_with(".md") {
            report.push(
                "malformed_rollout_summary_entry",
                format!(
                    "rollout_summaries contains a non-markdown entry: {}",
                    path.display()
                ),
            );
            continue;
        }
        files.insert(file_name);
    }

    Ok(files)
}

fn validate_raw_memories_contents(report: &mut ArtifactValidationReport, contents: &str) {
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        report.push(
            "degenerate_raw_memories",
            "raw_memories.md is empty after consolidation",
        );
        return;
    }
    if trimmed.contains("No raw memories yet.") {
        report.push(
            "degenerate_raw_memories",
            "raw_memories.md still contains the empty placeholder despite Phase 2 inputs",
        );
    }
    if !trimmed.contains("## Thread `") || !trimmed.contains("rollout_summary_file: ") {
        report.push(
            "degenerate_raw_memories",
            "raw_memories.md is missing thread entries or rollout_summary_file routing data",
        );
    }
}

fn validate_memory_contents(
    report: &mut ArtifactValidationReport,
    contents: &str,
    root: &Path,
    actual_rollout_files: &BTreeSet<String>,
    expected_rollout_files: &BTreeSet<String>,
) {
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        report.push(
            "degenerate_memory",
            "MEMORY.md is empty after consolidation",
        );
        return;
    }
    if trimmed.len() < MIN_MEMORY_MD_CHARS {
        report.push(
            "degenerate_memory",
            format!(
                "MEMORY.md is unexpectedly short for a non-empty consolidation output: {} chars",
                trimmed.len()
            ),
        );
    }
    if !trimmed.contains("# Task Group:") {
        report.push(
            "degenerate_memory",
            "MEMORY.md is missing the required `# Task Group:` structure",
        );
    }
    if !trimmed.contains("### rollout_summary_files") {
        report.push(
            "degenerate_memory",
            "MEMORY.md is missing `### rollout_summary_files` sections",
        );
    }
    if references_raw_memories(contents) {
        report.push(
            "durable_references_episodic_raw_memories",
            "MEMORY.md references raw_memories.md directly; keep raw_memories.md as episodic evidence only",
        );
    }

    let referenced_rollout_files = extract_rollout_summary_references(contents);
    if referenced_rollout_files.is_empty() {
        report.push(
            "missing_rollout_summary_reference",
            "MEMORY.md does not reference any rollout summary files",
        );
        return;
    }

    for referenced in &referenced_rollout_files {
        if !actual_rollout_files.contains(referenced) {
            report.push(
                "missing_rollout_summary_reference_target",
                format!(
                    "MEMORY.md references a rollout summary file that is not present on disk: {}",
                    rollout_summaries_dir(root).join(referenced).display()
                ),
            );
        }
    }

    for missing_reference in expected_rollout_files.difference(&referenced_rollout_files) {
        report.push(
            "missing_rollout_summary_reference",
            format!(
                "current Phase 2 rollout summary is not referenced from MEMORY.md: {}",
                rollout_summaries_dir(root)
                    .join(missing_reference)
                    .display()
            ),
        );
    }

    for unexpected_reference in referenced_rollout_files.difference(expected_rollout_files) {
        report.push(
            "unexpected_rollout_summary_reference",
            format!(
                "MEMORY.md references a rollout summary file outside the current Phase 2 artifact set: {}",
                rollout_summaries_dir(root).join(unexpected_reference).display()
            ),
        );
    }
}

fn validate_memory_summary_contents(report: &mut ArtifactValidationReport, contents: &str) {
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        report.push(
            "degenerate_memory_summary",
            "memory_summary.md is empty after consolidation",
        );
        return;
    }
    if trimmed.len() < MIN_MEMORY_SUMMARY_CHARS {
        report.push(
            "degenerate_memory_summary",
            format!(
                "memory_summary.md is unexpectedly short for a non-empty consolidation output: {} chars",
                trimmed.len()
            ),
        );
    }
    if !trimmed.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("# ") || line.starts_with("## ") || line.starts_with("- ")
    }) {
        report.push(
            "degenerate_memory_summary",
            "memory_summary.md is missing headings or bullets and looks structurally weak",
        );
    }
    if references_raw_memories(contents) {
        report.push(
            "durable_references_episodic_raw_memories",
            "memory_summary.md references raw_memories.md directly; keep raw_memories.md as episodic evidence only",
        );
    }
}

fn extract_rollout_summary_references(contents: &str) -> BTreeSet<String> {
    let mut references = BTreeSet::new();
    let mut in_rollout_summary_section = false;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed == "### rollout_summary_files" {
            in_rollout_summary_section = true;
            continue;
        }

        if trimmed.starts_with('#') && trimmed != "### rollout_summary_files" {
            in_rollout_summary_section = false;
        }

        if !in_rollout_summary_section || trimmed.is_empty() {
            continue;
        }

        let mut rest = trimmed;
        while let Some(idx) = rest.find("rollout_summaries/") {
            let suffix = &rest[idx + "rollout_summaries/".len()..];
            let file_name = suffix
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
                .collect::<String>();
            if file_name.ends_with(".md") && !file_name.is_empty() {
                references.insert(file_name);
            }
            if suffix.is_empty() {
                break;
            }
            rest = &suffix[1..];
        }
    }

    references
}

fn references_raw_memories(contents: &str) -> bool {
    contents.to_ascii_lowercase().contains("raw_memories.md")
}

#[cfg(test)]
mod tests {
    use super::validate_post_consolidation_artifacts;
    use crate::memories::ensure_layout;
    use crate::memories::raw_memories_file;
    use crate::memories::rollout_summaries_dir;
    use crate::memories::storage::rebuild_raw_memories_file_from_memories;
    use crate::memories::storage::rollout_summary_file_stem;
    use crate::memories::storage::sync_rollout_summaries_from_memories;
    use chrono::TimeZone;
    use chrono::Utc;
    use codex_protocol::ThreadId;
    use codex_state::Stage1Output;
    use std::path::PathBuf;

    fn sample_memory(
        thread_id: ThreadId,
        source_updated_at: i64,
        rollout_slug: &str,
    ) -> Stage1Output {
        Stage1Output {
            thread_id,
            source_updated_at: Utc
                .timestamp_opt(source_updated_at, 0)
                .single()
                .expect("timestamp"),
            raw_memory: "structured raw memory".to_string(),
            rollout_summary: "structured rollout summary".to_string(),
            rollout_slug: Some(rollout_slug.to_string()),
            rollout_path: PathBuf::from("/tmp/rollout.jsonl"),
            cwd: PathBuf::from("/tmp/workspace"),
            git_branch: Some("feature/memory".to_string()),
            generated_at: Utc
                .timestamp_opt(source_updated_at + 1, 0)
                .single()
                .expect("generated timestamp"),
        }
    }

    #[tokio::test]
    async fn validator_accepts_consistent_phase2_artifacts() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = tempdir.path().join("memories");
        ensure_layout(&root).await.expect("ensure layout");

        let memories = vec![sample_memory(
            ThreadId::new(),
            1_710_300_000,
            "validator-pass",
        )];
        sync_rollout_summaries_from_memories(&root, &memories, memories.len())
            .await
            .expect("sync rollout summaries");
        rebuild_raw_memories_file_from_memories(&root, &memories, memories.len())
            .await
            .expect("rebuild raw memories");

        let rollout_file = format!("{}.md", rollout_summary_file_stem(&memories[0]));
        tokio::fs::write(
            root.join("MEMORY.md"),
            format!(
                "# Task Group: memory validator coverage\n\nscope: validate post-consolidation artifacts for Codex memory\napplies_to: cwd=/tmp/workspace; reuse_rule=checkout-local until reconfirmed elsewhere\n\n## Task 1: Validate consistent rollout references\n\n### rollout_summary_files\n\n- <rollout_summaries/{rollout_file}> (cwd=/tmp/workspace, rollout_path=/tmp/rollout.jsonl, updated_at=2024-03-11T00:00:00Z, thread_id={}, retained for validator coverage)\n\n### keywords\n\n- validator, memory, rollout_summary_files, codex\n\n## Reusable knowledge\n\n- deterministic validation should accept a complete and non-degenerate artifact set backed by the current phase-2 rollout summaries [Task 1]\n",
                memories[0].thread_id
            ),
        )
        .await
        .expect("write MEMORY.md");
        tokio::fs::write(
            root.join("memory_summary.md"),
            "# Memory Summary\n\n## Durable guidance\n\n- Keep post-consolidation artifact validation deterministic and fail the job when durable artifacts are missing or inconsistent.\n",
        )
        .await
        .expect("write memory_summary.md");

        let report = validate_post_consolidation_artifacts(&root, &memories)
            .await
            .expect("validate artifacts");

        assert!(
            report.is_ok(),
            "expected validator to accept consistent artifacts, got {report:?}"
        );
    }

    #[tokio::test]
    async fn validator_reports_missing_and_inconsistent_artifacts() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = tempdir.path().join("memories");
        ensure_layout(&root).await.expect("ensure layout");

        let memories = vec![sample_memory(
            ThreadId::new(),
            1_710_400_000,
            "validator-fail",
        )];
        sync_rollout_summaries_from_memories(&root, &memories, memories.len())
            .await
            .expect("sync rollout summaries");

        tokio::fs::write(
            root.join("MEMORY.md"),
            "# Task Group: broken memory validator coverage\n\nscope: exercise inconsistent rollout references\napplies_to: cwd=/tmp/workspace; reuse_rule=checkout-local\n\n## Task 1: Reference the wrong rollout summary\n\n### rollout_summary_files\n\n- <rollout_summaries/stale-reference.md> (cwd=/tmp/workspace, rollout_path=/tmp/rollout.jsonl, updated_at=2024-03-12T00:00:00Z, thread_id=broken)\n\n### keywords\n\n- validator, stale-reference\n",
        )
        .await
        .expect("write broken MEMORY.md");
        tokio::fs::write(root.join("memory_summary.md"), "summary\n")
            .await
            .expect("write degenerate memory_summary.md");

        let report = validate_post_consolidation_artifacts(&root, &memories)
            .await
            .expect("validate artifacts");

        let issue_codes = report
            .issues
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>();
        assert!(
            issue_codes.contains(&"missing_raw_memories"),
            "expected missing raw_memories issue, got {report:?}"
        );
        assert!(
            issue_codes.contains(&"degenerate_memory_summary"),
            "expected degenerate memory_summary issue, got {report:?}"
        );
        assert!(
            issue_codes.contains(&"missing_rollout_summary_reference"),
            "expected missing expected rollout reference issue, got {report:?}"
        );
        assert!(
            issue_codes.contains(&"missing_rollout_summary_reference_target"),
            "expected missing referenced rollout target issue, got {report:?}"
        );
        assert!(
            issue_codes.contains(&"unexpected_rollout_summary_reference"),
            "expected unexpected rollout reference issue, got {report:?}"
        );

        let actual_rollout_file = format!("{}.md", rollout_summary_file_stem(&memories[0]));
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.message.contains(actual_rollout_file.as_str())),
            "expected report to mention the current rollout summary file, got {report:?}"
        );
        assert!(
            !report.is_ok(),
            "expected validator to reject inconsistent artifacts"
        );

        assert!(
            tokio::fs::try_exists(&raw_memories_file(&root))
                .await
                .expect("check raw_memories.md existence")
                == false
        );
        assert!(
            tokio::fs::read_dir(rollout_summaries_dir(&root))
                .await
                .expect("read rollout summaries dir")
                .next_entry()
                .await
                .expect("read rollout summaries entry")
                .is_some(),
            "expected rollout summary materialization to exist for mismatch coverage"
        );
    }

    #[tokio::test]
    async fn validator_rejects_durable_artifacts_that_reference_raw_memories() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = tempdir.path().join("memories");
        ensure_layout(&root).await.expect("ensure layout");

        let memories = vec![sample_memory(
            ThreadId::new(),
            1_710_500_000,
            "validator-raw-memories-reference",
        )];
        sync_rollout_summaries_from_memories(&root, &memories, memories.len())
            .await
            .expect("sync rollout summaries");
        rebuild_raw_memories_file_from_memories(&root, &memories, memories.len())
            .await
            .expect("rebuild raw memories");

        let rollout_file = format!("{}.md", rollout_summary_file_stem(&memories[0]));
        tokio::fs::write(
            root.join("MEMORY.md"),
            format!(
                "# Task Group: memory validator raw references\n\nscope: reject durable artifacts that point at episodic raw memory\napplies_to: cwd=/tmp/workspace; reuse_rule=checkout-local\n\n## Task 1: Reference episodic raw memory directly\n\n### rollout_summary_files\n\n- <rollout_summaries/{rollout_file}> (cwd=/tmp/workspace, rollout_path=/tmp/rollout.jsonl, updated_at=2024-03-13T00:00:00Z, thread_id={})\n\n### keywords\n\n- validator, raw_memories, episodic\n\n## Reusable knowledge\n\n- durable memory should route through MEMORY.md and rollout summaries instead of pointing future agents at raw_memories.md [Task 1]\n",
                memories[0].thread_id
            ),
        )
        .await
        .expect("write MEMORY.md");
        tokio::fs::write(
            root.join("memory_summary.md"),
            "# Memory Summary\n\n## Durable guidance\n\n- Route through MEMORY.md and rollout summaries; do not send future agents to raw_memories.md.\n",
        )
        .await
        .expect("write memory_summary.md");

        let report = validate_post_consolidation_artifacts(&root, &memories)
            .await
            .expect("validate artifacts");

        let issue_codes = report
            .issues
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>();
        assert!(
            issue_codes.contains(&"durable_references_episodic_raw_memories"),
            "expected raw_memories durable-reference issue, got {report:?}"
        );
        assert!(
            !report.is_ok(),
            "expected validator to reject durable artifacts that reference raw_memories.md"
        );
    }
}
