use super::threads::ThreadFilterOptions;
use super::threads::push_thread_filters;
use super::threads::push_thread_order_and_limit;
use super::*;
use crate::MemoryDirectoryHealth;
use crate::MemoryHealthArtifact;
use crate::MemoryHealthIssue;
use crate::MemoryHealthReport;
use crate::MemoryHealthRolloutSummary;
use crate::MemoryHealthRolloutSummarySet;
use crate::MemoryHealthSeverity;
use crate::MemoryNegativeFeedbackReason;
use crate::MemoryNegativeFeedbackRecord;
use crate::MemoryNegativeFeedbackTarget;
use crate::MemoryPeekEntry;
use crate::MemoryRetrievalRecord;
use crate::SortDirection;
use crate::model::Phase2InputSelection;
use crate::model::Phase2JobClaimOutcome;
use crate::model::SessionValueScore;
use crate::model::Stage1JobClaim;
use crate::model::Stage1JobClaimOutcome;
use crate::model::Stage1Output;
use crate::model::Stage1OutputRef;
use crate::model::Stage1OutputRow;
use crate::model::Stage1StartupClaimParams;
use crate::model::ThreadRow;
use crate::model::memory_negative_feedback_reason_from_str;
use crate::model::rollout_summary_file_name_from_parts;
use crate::model::stage1_output_ref_from_parts;
use chrono::DateTime;
use chrono::Duration;
use sqlx::Executor;
use sqlx::QueryBuilder;
use sqlx::Sqlite;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use uuid::Uuid;

const JOB_KIND_MEMORY_STAGE1: &str = "memory_stage1";
const JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL: &str = "memory_consolidate_global";
const MEMORY_CONSOLIDATION_JOB_KEY: &str = "global";
const DEFAULT_MEMORY_PEEK_LIMIT: usize = 10;

const DEFAULT_RETRY_REMAINING: i64 = 3;
const SESSION_VALUE_DUPLICATE_PENALTY: i64 = 18;

#[derive(Debug, Clone)]
struct SessionValueCandidate {
    output: Stage1Output,
    usage_count: i64,
    last_usage: Option<DateTime<Utc>>,
    negative_feedback_reason: Option<MemoryNegativeFeedbackReason>,
    score: SessionValueScore,
    fingerprint: String,
}

impl SessionValueCandidate {
    fn recency_timestamp(&self) -> i64 {
        self.last_usage
            .unwrap_or(self.output.source_updated_at)
            .timestamp()
    }

    fn from_row(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Self> {
        let output = Stage1Output::try_from(Stage1OutputRow::try_from_row(row)?)?;
        let last_usage = row
            .try_get::<Option<i64>, _>("last_usage")?
            .map(|secs| {
                DateTime::<Utc>::from_timestamp(secs, 0)
                    .ok_or_else(|| anyhow::anyhow!("invalid last_usage timestamp: {secs}"))
            })
            .transpose()?;
        let negative_feedback_reason = row
            .try_get::<Option<String>, _>("negative_feedback_reason")?
            .map(|reason| memory_negative_feedback_reason_from_str(reason.as_str()))
            .transpose()?;
        let usage_count = row.try_get::<Option<i64>, _>("usage_count")?.unwrap_or(0);
        let mut candidate = Self {
            output,
            usage_count,
            last_usage,
            negative_feedback_reason,
            score: SessionValueScore::default(),
            fingerprint: String::new(),
        };
        candidate.score = compute_session_value_score(
            &candidate.output,
            candidate.usage_count,
            candidate.last_usage,
            candidate.negative_feedback_reason,
        );
        candidate.fingerprint = session_value_fingerprint(&candidate.output);
        Ok(candidate)
    }
}

#[derive(Debug, Default)]
struct ParsedRawMemorySignals {
    preference_bullets: Vec<String>,
    decision_bullets: Vec<String>,
    reusable_bullets: Vec<String>,
    failure_bullets: Vec<String>,
    command_path_bullets: Vec<String>,
    successful_task_count: usize,
}

fn compute_session_value_score(
    output: &Stage1Output,
    usage_count: i64,
    last_usage: Option<DateTime<Utc>>,
    negative_feedback_reason: Option<MemoryNegativeFeedbackReason>,
) -> SessionValueScore {
    let signals = parse_raw_memory_signals(output.raw_memory.as_str());
    let unique_signal_bullets = collect_unique_signal_bullets(&signals);
    let command_path_count = count_command_or_path_signals(
        signals
            .command_path_bullets
            .iter()
            .chain(signals.reusable_bullets.iter())
            .map(String::as_str),
    );
    let recovery_term_hits = count_recovery_term_hits(&signals.failure_bullets);
    let summary_word_count = output.rollout_summary.split_whitespace().count();
    let total_signal_bullets = signals.preference_bullets.len()
        + signals.decision_bullets.len()
        + signals.reusable_bullets.len()
        + signals.failure_bullets.len()
        + signals.command_path_bullets.len();

    let novelty = (unique_signal_bullets.len().min(6) as i64) * 3;
    let preference_decision_density =
        ((signals.preference_bullets.len() + signals.decision_bullets.len()).min(6) as i64) * 3;
    let reusable_command_path_signal = (command_path_count.min(5) as i64) * 3;
    let failure_recovery_signal = ((signals.failure_bullets.len().min(3) as i64) * 3)
        + ((recovery_term_hits.min(3) as i64) * 2);

    let usage_signal = usage_count.max(0).min(3) * 4;
    let recent_usage_signal = match last_usage.map(|value| (Utc::now() - value).num_days()) {
        Some(days) if days <= 7 => 4,
        Some(days) if days <= 30 => 2,
        _ => 0,
    };
    let completion_citation_signal =
        ((signals.successful_task_count.min(3) as i64) * 3) + usage_signal + recent_usage_signal;

    let negative_feedback_penalty = negative_feedback_reason.map_or(0, |reason| match reason {
        MemoryNegativeFeedbackReason::Transient => 18,
        MemoryNegativeFeedbackReason::Duplicate => 20,
        MemoryNegativeFeedbackReason::WrongScope => 24,
        MemoryNegativeFeedbackReason::IncorrectInference => 28,
        MemoryNegativeFeedbackReason::PrivacySensitive => 40,
    });

    let low_information_penalty = match (summary_word_count, total_signal_bullets) {
        (_, 0) => 20,
        (0..=20, _) => 12,
        (_, 1..=2) => 8,
        _ => 0,
    };

    let total = novelty
        + preference_decision_density
        + reusable_command_path_signal
        + failure_recovery_signal
        + completion_citation_signal
        - negative_feedback_penalty
        - low_information_penalty;

    SessionValueScore {
        total,
        novelty,
        preference_decision_density,
        reusable_command_path_signal,
        failure_recovery_signal,
        completion_citation_signal,
        negative_feedback_penalty,
        duplication_penalty: 0,
        low_information_penalty,
        suppressed: negative_feedback_reason.is_some(),
    }
}

fn parse_raw_memory_signals(raw_memory: &str) -> ParsedRawMemorySignals {
    const PREFERENCE: &str = "Preference signals:";
    const DECISION: &str = "Decision signals:";
    const REUSABLE: &str = "Reusable knowledge:";
    const FAILURE: &str = "Failures and how to do differently:";
    const COMMAND_PATH: &str = "High-value commands or paths:";

    let mut parsed = ParsedRawMemorySignals::default();
    let mut current_section: Option<&str> = None;
    for line in raw_memory.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("task_outcome: success") {
            parsed.successful_task_count += 1;
        }

        match trimmed {
            PREFERENCE | DECISION | REUSABLE | FAILURE | COMMAND_PATH => {
                current_section = Some(trimmed);
                continue;
            }
            _ => {}
        }

        if trimmed.starts_with("### Task ")
            || trimmed.starts_with("task:")
            || trimmed.starts_with("task_group:")
            || trimmed.starts_with("task_outcome:")
            || (trimmed.ends_with(':') && !trimmed.starts_with("- "))
        {
            current_section = None;
        }

        if !trimmed.starts_with("- ") {
            continue;
        }
        let bullet = trimmed.trim_start_matches("- ").trim();
        if bullet.is_empty() {
            continue;
        }

        match current_section {
            Some(PREFERENCE) => parsed.preference_bullets.push(bullet.to_string()),
            Some(DECISION) => parsed.decision_bullets.push(bullet.to_string()),
            Some(REUSABLE) => parsed.reusable_bullets.push(bullet.to_string()),
            Some(FAILURE) => parsed.failure_bullets.push(bullet.to_string()),
            Some(COMMAND_PATH) => parsed.command_path_bullets.push(bullet.to_string()),
            _ => {}
        }
    }

    parsed
}

fn collect_unique_signal_bullets(signals: &ParsedRawMemorySignals) -> HashSet<String> {
    signals
        .preference_bullets
        .iter()
        .chain(signals.decision_bullets.iter())
        .chain(signals.reusable_bullets.iter())
        .chain(signals.failure_bullets.iter())
        .chain(signals.command_path_bullets.iter())
        .map(|bullet| normalize_signal_text(bullet.as_str()))
        .filter(|bullet| !bullet.is_empty())
        .collect()
}

fn count_command_or_path_signals<'a>(bullets: impl Iterator<Item = &'a str>) -> usize {
    bullets
        .filter(|bullet| {
            bullet.contains('`')
                || bullet.contains('\\')
                || bullet.contains('/')
                || [
                    "cargo ",
                    "git ",
                    "just ",
                    "rg ",
                    "Set-Location ",
                    "codex",
                    ".md",
                ]
                .iter()
                .any(|needle| bullet.contains(needle))
        })
        .count()
}

fn count_recovery_term_hits(bullets: &[String]) -> usize {
    bullets
        .iter()
        .filter(|bullet| {
            let normalized = normalize_signal_text(bullet.as_str());
            [
                "avoid",
                "instead",
                "retry",
                "recover",
                "fallback",
                "fix",
                "correct",
                "do differently",
            ]
            .iter()
            .any(|needle| normalized.contains(needle))
        })
        .count()
}

fn normalize_signal_text(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn session_value_fingerprint(output: &Stage1Output) -> String {
    let signals = parse_raw_memory_signals(output.raw_memory.as_str());
    let mut unique_signal_bullets = collect_unique_signal_bullets(&signals)
        .into_iter()
        .collect::<Vec<_>>();
    unique_signal_bullets.sort();

    let normalized_summary = normalize_signal_text(output.rollout_summary.as_str());
    let summary_fragment = normalized_summary
        .split_whitespace()
        .take(24)
        .collect::<Vec<_>>()
        .join(" ");
    let bullet_fragment = unique_signal_bullets
        .into_iter()
        .take(8)
        .collect::<Vec<_>>()
        .join("|");
    format!("{summary_fragment}||{bullet_fragment}")
}

fn apply_duplicate_penalties(candidates: &mut [SessionValueCandidate]) {
    let mut order = (0..candidates.len()).collect::<Vec<_>>();
    order.sort_by(|lhs, rhs| compare_session_candidates_desc(&candidates[*lhs], &candidates[*rhs]));

    let mut seen_fingerprints = HashMap::<String, usize>::new();
    for idx in order {
        let fingerprint = candidates[idx].fingerprint.trim();
        if fingerprint.is_empty() {
            continue;
        }
        let duplicate_count = seen_fingerprints
            .entry(fingerprint.to_string())
            .or_insert(0);
        if *duplicate_count > 0 {
            let penalty =
                SESSION_VALUE_DUPLICATE_PENALTY * i64::try_from(*duplicate_count).unwrap_or(1);
            candidates[idx].score.duplication_penalty = penalty;
            candidates[idx].score.total -= penalty;
        }
        *duplicate_count += 1;
    }
}

fn compare_session_candidates_desc(
    lhs: &SessionValueCandidate,
    rhs: &SessionValueCandidate,
) -> Ordering {
    lhs.score
        .suppressed
        .cmp(&rhs.score.suppressed)
        .then_with(|| rhs.score.total.cmp(&lhs.score.total))
        .then_with(|| rhs.usage_count.cmp(&lhs.usage_count))
        .then_with(|| rhs.recency_timestamp().cmp(&lhs.recency_timestamp()))
        .then_with(|| {
            rhs.output
                .source_updated_at
                .cmp(&lhs.output.source_updated_at)
        })
        .then_with(|| {
            rhs.output
                .thread_id
                .to_string()
                .cmp(&lhs.output.thread_id.to_string())
        })
}

fn compare_session_candidates_asc(
    lhs: &SessionValueCandidate,
    rhs: &SessionValueCandidate,
) -> Ordering {
    rhs.score
        .suppressed
        .cmp(&lhs.score.suppressed)
        .then_with(|| lhs.score.total.cmp(&rhs.score.total))
        .then_with(|| lhs.recency_timestamp().cmp(&rhs.recency_timestamp()))
        .then_with(|| {
            lhs.output
                .source_updated_at
                .cmp(&rhs.output.source_updated_at)
        })
        .then_with(|| {
            lhs.output
                .thread_id
                .to_string()
                .cmp(&rhs.output.thread_id.to_string())
        })
}

impl StateRuntime {
    /// Deletes all persisted memory state in one transaction.
    ///
    /// This removes every `stage1_outputs` row and all `jobs` rows for the
    /// stage-1 (`memory_stage1`) and phase-2 (`memory_consolidate_global`)
    /// memory pipelines.
    pub async fn clear_memory_data(&self) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            r#"
DELETE FROM stage1_outputs
            "#,
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
DELETE FROM memory_negative_feedback
            "#,
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
DELETE FROM jobs
WHERE kind = ? OR kind = ?
            "#,
        )
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Lists recent stage-1 outputs in an operator-friendly shape for `memory/peek`.
    pub async fn list_memory_peek_entries(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryPeekEntry>> {
        let limit = if limit == 0 {
            DEFAULT_MEMORY_PEEK_LIMIT
        } else {
            limit
        };

        let rows = sqlx::query(
            r#"
SELECT
    so.thread_id,
    so.source_updated_at,
    so.raw_memory,
    so.rollout_summary,
    so.rollout_slug,
    so.generated_at,
    COALESCE(t.cwd, '') AS cwd,
    t.updated_at_ms AS thread_updated_at
FROM stage1_outputs AS so
LEFT JOIN threads AS t
    ON t.id = so.thread_id
WHERE t.memory_mode = 'enabled'
  AND (length(trim(so.raw_memory)) > 0 OR length(trim(so.rollout_summary)) > 0)
ORDER BY so.source_updated_at DESC, t.updated_at_ms DESC, so.thread_id DESC
LIMIT ?
            "#,
        )
        .bind(limit as i64)
        .fetch_all(self.pool.as_ref())
        .await?;

        rows.into_iter()
            .map(|row| {
                let thread_id = ThreadId::try_from(row.try_get::<String, _>("thread_id")?)?;
                let source_updated_at =
                    DateTime::<Utc>::from_timestamp(row.try_get::<i64, _>("source_updated_at")?, 0)
                        .ok_or_else(|| anyhow::anyhow!("invalid source_updated_at timestamp"))?;
                let generated_at =
                    DateTime::<Utc>::from_timestamp(row.try_get::<i64, _>("generated_at")?, 0)
                        .ok_or_else(|| anyhow::anyhow!("invalid generated_at timestamp"))?;
                let thread_updated_at =
                    epoch_millis_to_datetime(row.try_get::<i64, _>("thread_updated_at")?)?;
                let rollout_slug = row.try_get::<Option<String>, _>("rollout_slug")?;
                let rollout_summary = row.try_get::<String, _>("rollout_summary")?;
                let raw_memory = row.try_get::<String, _>("raw_memory")?;

                Ok(MemoryPeekEntry {
                    rollout_summary_file: format!(
                        "rollout_summaries/{}",
                        rollout_summary_file_name_from_parts(
                            thread_id.clone(),
                            source_updated_at,
                            rollout_slug.as_deref(),
                        )
                    ),
                    summary_excerpt: summarize_memory_peek_excerpt(
                        rollout_summary.as_str(),
                        raw_memory.as_str(),
                    ),
                    thread_id,
                    thread_updated_at,
                    source_updated_at,
                    generated_at,
                    cwd: PathBuf::from(row.try_get::<String, _>("cwd")?),
                })
            })
            .collect()
    }

    /// Lists stage-1 retrieval metadata for pre-turn ranking without changing
    /// the persisted memory schema.
    pub async fn list_memory_retrieval_records(
        &self,
    ) -> anyhow::Result<Vec<MemoryRetrievalRecord>> {
        let rows = sqlx::query(
            r#"
SELECT
    so.thread_id,
    so.source_updated_at,
    so.rollout_slug,
    COALESCE(t.cwd, '') AS cwd,
    COALESCE(t.rollout_path, '') AS rollout_path,
    COALESCE(so.usage_count, 0) AS usage_count,
    so.last_usage,
    mnf.reason AS negative_feedback_reason
FROM stage1_outputs AS so
LEFT JOIN threads AS t
    ON t.id = so.thread_id
LEFT JOIN memory_negative_feedback AS mnf
    ON mnf.thread_id = so.thread_id
   AND mnf.source_updated_at = so.source_updated_at
WHERE t.memory_mode = 'enabled'
  AND (length(trim(so.raw_memory)) > 0 OR length(trim(so.rollout_summary)) > 0)
ORDER BY so.source_updated_at DESC, so.thread_id DESC
            "#,
        )
        .fetch_all(self.pool.as_ref())
        .await?;

        rows.into_iter()
            .map(|row| {
                let thread_id = ThreadId::try_from(row.try_get::<String, _>("thread_id")?)?;
                let source_updated_at =
                    DateTime::<Utc>::from_timestamp(row.try_get::<i64, _>("source_updated_at")?, 0)
                        .ok_or_else(|| anyhow::anyhow!("invalid source_updated_at timestamp"))?;
                let last_usage = row
                    .try_get::<Option<i64>, _>("last_usage")?
                    .map(|timestamp| {
                        DateTime::<Utc>::from_timestamp(timestamp, 0)
                            .ok_or_else(|| anyhow::anyhow!("invalid last_usage timestamp"))
                    })
                    .transpose()?;
                let negative_feedback_reason = row
                    .try_get::<Option<String>, _>("negative_feedback_reason")?
                    .map(|value| memory_negative_feedback_reason_from_str(&value))
                    .transpose()?;

                Ok(MemoryRetrievalRecord {
                    thread_id,
                    source_updated_at,
                    rollout_slug: row.try_get::<Option<String>, _>("rollout_slug")?,
                    cwd: PathBuf::from(row.try_get::<String, _>("cwd")?),
                    rollout_path: PathBuf::from(row.try_get::<String, _>("rollout_path")?),
                    usage_count: row.try_get::<i64, _>("usage_count")?,
                    last_usage,
                    negative_feedback_reason,
                })
            })
            .collect()
    }

    /// Validates the current memory artifact layout for `memory/health`.
    pub async fn inspect_memory_health(
        &self,
        max_raw_memories_for_consolidation: usize,
        max_unused_days: i64,
    ) -> anyhow::Result<MemoryHealthReport> {
        let memory_root_path = self.codex_home().join("memories");
        let rollout_summaries_dir = memory_root_path.join("rollout_summaries");
        let selection = self
            .get_phase2_input_selection(max_raw_memories_for_consolidation, max_unused_days)
            .await?;
        let has_selected_inputs = !selection.selected.is_empty();

        let memory_root = collect_directory_health(memory_root_path.as_path()).await;
        let rollout_directory = collect_directory_health(rollout_summaries_dir.as_path()).await;
        let memory_file =
            collect_artifact_health("memory", memory_root_path.join("MEMORY.md")).await;
        let memory_summary_file =
            collect_artifact_health("memory_summary", memory_root_path.join("memory_summary.md"))
                .await;
        let raw_memories_file =
            collect_artifact_health("raw_memories", memory_root_path.join("raw_memories.md")).await;

        let expected_rollout_files = selection
            .selected
            .iter()
            .map(|item| {
                rollout_summary_file_name_from_parts(
                    item.thread_id,
                    item.source_updated_at,
                    item.rollout_slug.as_deref(),
                )
            })
            .collect::<Vec<_>>();

        let mut expected_rollout_summaries = Vec::with_capacity(expected_rollout_files.len());
        for file_name in &expected_rollout_files {
            expected_rollout_summaries.push(
                collect_rollout_summary_health(file_name, rollout_summaries_dir.join(file_name))
                    .await,
            );
        }

        let mut stale_rollout_summaries = Vec::new();
        let mut malformed_retrieval_sources = Vec::new();
        if rollout_directory.exists && rollout_directory.is_directory {
            match tokio::fs::read_dir(&rollout_summaries_dir).await {
                Ok(mut entries) => {
                    let expected_names = expected_rollout_files
                        .iter()
                        .cloned()
                        .collect::<BTreeSet<_>>();
                    while let Some(entry) = entries.next_entry().await? {
                        let path = entry.path();
                        let file_name = entry.file_name().to_string_lossy().to_string();
                        let file_type = entry.file_type().await?;
                        if !file_type.is_file() {
                            malformed_retrieval_sources.push(MemoryHealthIssue {
                                severity: MemoryHealthSeverity::Warning,
                                code: "malformed_retrieval_source".to_string(),
                                message: format!(
                                    "rollout summaries entry is not a file: {}",
                                    path.display()
                                ),
                            });
                            continue;
                        }

                        if !file_name.ends_with(".md") {
                            malformed_retrieval_sources.push(MemoryHealthIssue {
                                severity: MemoryHealthSeverity::Warning,
                                code: "malformed_retrieval_source".to_string(),
                                message: format!(
                                    "rollout summaries entry does not use the expected .md suffix: {}",
                                    path.display()
                                ),
                            });
                            continue;
                        }

                        if expected_names.contains(&file_name) {
                            continue;
                        }

                        stale_rollout_summaries
                            .push(collect_rollout_summary_health(file_name.as_str(), path).await);
                    }
                }
                Err(err) => {
                    malformed_retrieval_sources.push(MemoryHealthIssue {
                        severity: MemoryHealthSeverity::Error,
                        code: "rollout_summaries_unreadable".to_string(),
                        message: format!(
                            "failed to read rollout summaries directory {}: {err}",
                            rollout_summaries_dir.display()
                        ),
                    });
                }
            }
        }

        stale_rollout_summaries.sort_by(|left, right| left.file_name.cmp(&right.file_name));

        let artifacts = vec![memory_file, memory_summary_file, raw_memories_file];
        let mut issues = Vec::new();
        if !memory_root.exists {
            issues.push(MemoryHealthIssue {
                severity: if has_selected_inputs {
                    MemoryHealthSeverity::Error
                } else {
                    MemoryHealthSeverity::Warning
                },
                code: "missing_memory_root".to_string(),
                message: format!("memory root is missing: {}", memory_root.path.display()),
            });
        } else if !memory_root.is_directory {
            issues.push(MemoryHealthIssue {
                severity: MemoryHealthSeverity::Error,
                code: "invalid_memory_root".to_string(),
                message: format!(
                    "memory root exists but is not a directory: {}",
                    memory_root.path.display()
                ),
            });
        }

        evaluate_memory_artifact_health(
            &mut issues,
            artifact_by_kind(&artifacts, "memory").expect("memory artifact"),
            has_selected_inputs,
        );
        evaluate_memory_artifact_health(
            &mut issues,
            artifact_by_kind(&artifacts, "memory_summary").expect("memory_summary artifact"),
            has_selected_inputs,
        );
        evaluate_memory_artifact_health(
            &mut issues,
            artifact_by_kind(&artifacts, "raw_memories").expect("raw_memories artifact"),
            has_selected_inputs
                || artifact_by_kind(&artifacts, "memory")
                    .expect("memory artifact")
                    .exists
                || artifact_by_kind(&artifacts, "memory_summary")
                    .expect("memory_summary artifact")
                    .exists
                || rollout_directory.exists,
        );

        if !rollout_directory.exists && has_selected_inputs {
            issues.push(MemoryHealthIssue {
                severity: MemoryHealthSeverity::Error,
                code: "missing_rollout_summaries_dir".to_string(),
                message: format!(
                    "rollout summaries directory is missing: {}",
                    rollout_directory.path.display()
                ),
            });
        } else if rollout_directory.exists && !rollout_directory.is_directory {
            issues.push(MemoryHealthIssue {
                severity: MemoryHealthSeverity::Error,
                code: "invalid_rollout_summaries_dir".to_string(),
                message: format!(
                    "rollout summaries path exists but is not a directory: {}",
                    rollout_directory.path.display()
                ),
            });
        }

        for summary in &expected_rollout_summaries {
            if !summary.exists {
                issues.push(MemoryHealthIssue {
                    severity: MemoryHealthSeverity::Error,
                    code: "missing_rollout_summary".to_string(),
                    message: format!(
                        "expected rollout summary is missing: {}",
                        summary.path.display()
                    ),
                });
                continue;
            }
            if !summary.is_file || !summary.readable || summary.size_bytes == Some(0) {
                issues.push(MemoryHealthIssue {
                    severity: MemoryHealthSeverity::Error,
                    code: "invalid_rollout_summary".to_string(),
                    message: format!(
                        "expected rollout summary is malformed or unreadable: {}",
                        summary.path.display()
                    ),
                });
            }
        }

        for summary in &stale_rollout_summaries {
            issues.push(MemoryHealthIssue {
                severity: MemoryHealthSeverity::Warning,
                code: "stale_rollout_summary".to_string(),
                message: format!(
                    "rollout summary exists on disk but is not in the current phase-2 selection: {}",
                    summary.path.display()
                ),
            });
        }

        issues.extend(malformed_retrieval_sources);
        issues.sort_by(|left, right| {
            left.code
                .cmp(&right.code)
                .then_with(|| left.message.cmp(&right.message))
        });

        Ok(MemoryHealthReport {
            ok: !issues
                .iter()
                .any(|issue| issue.severity == MemoryHealthSeverity::Error),
            memory_root,
            artifacts,
            rollout_summaries: MemoryHealthRolloutSummarySet {
                directory: rollout_directory,
                expected: expected_rollout_summaries,
                stale: stale_rollout_summaries,
            },
            issues,
        })
    }

    /// Record usage for cited stage-1 outputs.
    ///
    /// Each thread id increments `usage_count` by one and sets `last_usage` to
    /// the current Unix timestamp. Missing rows are ignored.
    pub async fn record_stage1_output_usage(
        &self,
        thread_ids: &[ThreadId],
    ) -> anyhow::Result<usize> {
        if thread_ids.is_empty() {
            return Ok(0);
        }

        let now = Utc::now().timestamp();
        let mut tx = self.pool.begin().await?;
        let mut updated_rows = 0;

        for thread_id in thread_ids {
            updated_rows += sqlx::query(
                r#"
UPDATE stage1_outputs
SET
    usage_count = COALESCE(usage_count, 0) + 1,
    last_usage = ?
WHERE thread_id = ?
                "#,
            )
            .bind(now)
            .bind(thread_id.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected() as usize;
        }

        tx.commit().await?;
        Ok(updated_rows)
    }

    /// Resolves a target to the current persisted stage-1 snapshot, stores
    /// negative feedback for that snapshot, and enqueues phase-2 forgetting.
    pub async fn record_memory_negative_feedback(
        &self,
        target: &MemoryNegativeFeedbackTarget,
        reason: MemoryNegativeFeedbackReason,
    ) -> anyhow::Result<MemoryNegativeFeedbackRecord> {
        let mut tx = self.pool.begin().await?;
        let resolved_target = resolve_memory_negative_feedback_target(&mut *tx, target).await?;
        let now = Utc::now().timestamp();

        sqlx::query(
            r#"
INSERT INTO memory_negative_feedback (
    thread_id,
    source_updated_at,
    rollout_slug,
    reason,
    created_at,
    updated_at
) VALUES (?, ?, ?, ?, ?, ?)
ON CONFLICT(thread_id, source_updated_at) DO UPDATE SET
    rollout_slug = excluded.rollout_slug,
    reason = excluded.reason,
    updated_at = excluded.updated_at
            "#,
        )
        .bind(resolved_target.thread_id.to_string())
        .bind(resolved_target.source_updated_at.timestamp())
        .bind(resolved_target.rollout_slug.as_deref())
        .bind(reason.as_str())
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        enqueue_global_consolidation_with_executor(
            &mut *tx,
            resolved_target.source_updated_at.timestamp(),
        )
        .await?;

        let row = sqlx::query(
            r#"
SELECT thread_id, source_updated_at, rollout_slug, reason, created_at, updated_at
FROM memory_negative_feedback
WHERE thread_id = ? AND source_updated_at = ?
            "#,
        )
        .bind(resolved_target.thread_id.to_string())
        .bind(resolved_target.source_updated_at.timestamp())
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(MemoryNegativeFeedbackRecord {
            thread_id: ThreadId::try_from(row.try_get::<String, _>("thread_id")?)?,
            source_updated_at: DateTime::<Utc>::from_timestamp(
                row.try_get::<i64, _>("source_updated_at")?,
                0,
            )
            .ok_or_else(|| anyhow::anyhow!("invalid memory negative feedback source_updated_at"))?,
            rollout_slug: row.try_get("rollout_slug")?,
            reason: memory_negative_feedback_reason_from_str(
                row.try_get::<String, _>("reason")?.as_str(),
            )?,
            created_at: DateTime::<Utc>::from_timestamp(row.try_get::<i64, _>("created_at")?, 0)
                .ok_or_else(|| anyhow::anyhow!("invalid memory negative feedback created_at"))?,
            updated_at: DateTime::<Utc>::from_timestamp(row.try_get::<i64, _>("updated_at")?, 0)
                .ok_or_else(|| anyhow::anyhow!("invalid memory negative feedback updated_at"))?,
        })
    }

    /// Selects and claims stage-1 startup jobs for stale threads.
    ///
    /// Query behavior:
    /// - starts from `threads` filtered to active threads and allowed sources
    ///   (`push_thread_filters`)
    /// - excludes threads with `memory_mode != 'enabled'`
    /// - excludes the current thread id
    /// - keeps only threads whose millisecond `updated_at` is in the age window
    /// - keeps only threads whose memory is stale compared to millisecond `updated_at`
    /// - orders by `updated_at_ms DESC` and applies `scan_limit`
    ///
    /// For each selected thread, this function calls [`Self::try_claim_stage1_job`]
    /// with `source_updated_at = thread.updated_at.timestamp()` and returns up to
    /// `max_claimed` successful claims.
    pub async fn claim_stage1_jobs_for_startup(
        &self,
        current_thread_id: ThreadId,
        params: Stage1StartupClaimParams<'_>,
    ) -> anyhow::Result<Vec<Stage1JobClaim>> {
        let Stage1StartupClaimParams {
            scan_limit,
            max_claimed,
            max_age_days,
            min_rollout_idle_hours,
            allowed_sources,
            lease_seconds,
        } = params;
        if scan_limit == 0 || max_claimed == 0 {
            return Ok(Vec::new());
        }

        let worker_id = current_thread_id;
        let current_thread_id = worker_id.to_string();
        let max_age_cutoff = (Utc::now() - Duration::days(max_age_days.max(0))).timestamp_millis();
        let idle_cutoff =
            (Utc::now() - Duration::hours(min_rollout_idle_hours.max(0))).timestamp_millis();

        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT
    threads.id,
    threads.rollout_path,
    threads.created_at_ms AS created_at,
    threads.updated_at_ms AS updated_at,
    threads.source,
    threads.agent_path,
    threads.agent_nickname,
    threads.agent_role,
    threads.model_provider,
    threads.model,
    threads.reasoning_effort,
    threads.cwd,
    threads.cli_version,
    threads.title,
    threads.sandbox_policy,
    threads.approval_mode,
    threads.tokens_used,
    threads.first_user_message,
    threads.archived_at,
    threads.git_sha,
    threads.git_branch,
    threads.git_origin_url
FROM threads
LEFT JOIN stage1_outputs
    ON stage1_outputs.thread_id = threads.id
LEFT JOIN jobs
    ON jobs.kind = 
            "#,
        );
        builder.push_bind(JOB_KIND_MEMORY_STAGE1);
        builder.push(
            r#"
   AND jobs.job_key = threads.id
            "#,
        );
        push_thread_filters(
            &mut builder,
            ThreadFilterOptions {
                archived_only: false,
                allowed_sources,
                model_providers: None,
                cwd_filters: None,
                anchor: None,
                sort_key: SortKey::UpdatedAt,
                sort_direction: SortDirection::Desc,
                search_term: None,
            },
        );
        builder.push(" AND threads.memory_mode = 'enabled'");
        builder
            .push(" AND threads.id != ")
            .push_bind(current_thread_id.as_str());
        builder
            .push(" AND ")
            .push("threads.updated_at_ms")
            .push(" >= ")
            .push_bind(max_age_cutoff);
        builder
            .push(" AND ")
            .push("threads.updated_at_ms")
            .push(" <= ")
            .push_bind(idle_cutoff);
        let updated_at = "threads.updated_at_ms";
        builder.push(" AND ((COALESCE(stage1_outputs.source_updated_at, -1) + 1) * 1000) <= ");
        builder.push(updated_at);
        builder.push(" AND ((COALESCE(jobs.last_success_watermark, -1) + 1) * 1000) <= ");
        builder.push(updated_at);
        push_thread_order_and_limit(
            &mut builder,
            SortKey::UpdatedAt,
            SortDirection::Desc,
            scan_limit,
        );

        let items = builder
            .build()
            .fetch_all(self.pool.as_ref())
            .await?
            .into_iter()
            .map(|row| ThreadRow::try_from_row(&row).and_then(ThreadMetadata::try_from))
            .collect::<Result<Vec<_>, _>>()?;

        let mut claimed = Vec::new();

        for item in items {
            if claimed.len() >= max_claimed {
                break;
            }

            if let Stage1JobClaimOutcome::Claimed { ownership_token } = self
                .try_claim_stage1_job(
                    item.id,
                    worker_id,
                    item.updated_at.timestamp(),
                    lease_seconds,
                    max_claimed,
                )
                .await?
            {
                claimed.push(Stage1JobClaim {
                    thread: item,
                    ownership_token,
                });
            }
        }

        Ok(claimed)
    }

    /// Lists the most recent non-empty stage-1 outputs for global consolidation.
    ///
    /// Query behavior:
    /// - filters out rows where both `raw_memory` and `rollout_summary` are blank
    /// - joins `threads` to include thread `cwd`, `rollout_path`, and `git_branch`
    /// - orders by `source_updated_at DESC, thread_id DESC`
    /// - applies `LIMIT n`
    pub async fn list_stage1_outputs_for_global(
        &self,
        n: usize,
    ) -> anyhow::Result<Vec<Stage1Output>> {
        if n == 0 {
            return Ok(Vec::new());
        }

        let rows = sqlx::query(
            r#"
SELECT
    so.thread_id,
    COALESCE(t.rollout_path, '') AS rollout_path,
    so.source_updated_at,
    so.raw_memory,
    so.rollout_summary,
    so.rollout_slug,
    so.generated_at,
    COALESCE(t.cwd, '') AS cwd,
    t.git_branch AS git_branch
FROM stage1_outputs AS so
LEFT JOIN threads AS t
    ON t.id = so.thread_id
WHERE t.memory_mode = 'enabled'
  AND (length(trim(so.raw_memory)) > 0 OR length(trim(so.rollout_summary)) > 0)
ORDER BY so.source_updated_at DESC, so.thread_id DESC
LIMIT ?
            "#,
        )
        .bind(n as i64)
        .fetch_all(self.pool.as_ref())
        .await?;

        rows.into_iter()
            .map(|row| Stage1OutputRow::try_from_row(&row).and_then(Stage1Output::try_from))
            .collect::<Result<Vec<_>, _>>()
    }

    /// Prunes stale stage-1 outputs while preserving the latest phase-2
    /// baseline and stage-1 job watermarks.
    ///
    /// Query behavior:
    /// - considers only rows with `selected_for_phase2 = 0`
    /// - keeps recency as `COALESCE(last_usage, source_updated_at)`
    /// - removes rows older than `max_unused_days`
    /// - computes a deterministic session-value score for each stale row and
    ///   prunes at most `limit` rows ordered from lowest-value to highest-value
    ///   with stale recency as a tie-breaker
    pub async fn prune_stage1_outputs_for_retention(
        &self,
        max_unused_days: i64,
        limit: usize,
    ) -> anyhow::Result<usize> {
        if limit == 0 {
            return Ok(0);
        }

        let cutoff = (Utc::now() - Duration::days(max_unused_days.max(0))).timestamp();
        let rows = sqlx::query(
            r#"
SELECT
    so.thread_id,
    COALESCE(t.rollout_path, '') AS rollout_path,
    so.source_updated_at,
    so.raw_memory,
    so.rollout_summary,
    so.rollout_slug,
    so.generated_at,
    COALESCE(t.cwd, '') AS cwd,
    t.git_branch AS git_branch,
    COALESCE(so.usage_count, 0) AS usage_count,
    so.last_usage,
    mnf.reason AS negative_feedback_reason
FROM stage1_outputs AS so
LEFT JOIN threads AS t
    ON t.id = so.thread_id
LEFT JOIN memory_negative_feedback AS mnf
    ON mnf.thread_id = so.thread_id
   AND mnf.source_updated_at = so.source_updated_at
WHERE so.selected_for_phase2 = 0
  AND COALESCE(so.last_usage, so.source_updated_at) < ?
            "#,
        )
        .bind(cutoff)
        .fetch_all(self.pool.as_ref())
        .await?
        .into_iter()
        .map(|row| SessionValueCandidate::from_row(&row))
        .collect::<Result<Vec<_>, _>>()?;

        let mut candidates = rows;
        apply_duplicate_penalties(&mut candidates);
        candidates.sort_by(compare_session_candidates_asc);
        let thread_ids = candidates
            .into_iter()
            .take(limit)
            .map(|candidate| candidate.output.thread_id.to_string())
            .collect::<Vec<_>>();
        if thread_ids.is_empty() {
            return Ok(0);
        }

        let mut builder =
            QueryBuilder::<Sqlite>::new("DELETE FROM stage1_outputs WHERE thread_id IN (");
        let mut separated = builder.separated(", ");
        for thread_id in &thread_ids {
            separated.push_bind(thread_id);
        }
        separated.push_unseparated(")");
        let rows_affected = builder
            .build()
            .execute(self.pool.as_ref())
            .await?
            .rows_affected();

        Ok(rows_affected as usize)
    }

    /// Returns the current phase-2 input set along with its diff against the
    /// last successful phase-2 selection.
    ///
    /// Query behavior:
    /// - current selection keeps only non-empty stage-1 outputs whose
    ///   `last_usage` is within `max_unused_days`, or whose
    ///   `source_updated_at` is within that window when the memory has never
    ///   been used
    /// - eligible rows are scored deterministically using structured Phase-1
    ///   content plus usage / suppression metadata
    /// - current selection is ordered by session-value score DESC, then by
    ///   usage / recency tie-breakers
    /// - previously selected rows are identified by `selected_for_phase2 = 1`
    /// - `previous_selected` contains the current persisted rows that belonged
    ///   to the last successful phase-2 baseline, even if those threads are no
    ///   longer memory-eligible
    /// - `retained_thread_ids` records which current rows still match the exact
    ///   snapshot selected in the last successful phase-2 run
    /// - removed rows are previously selected rows that are still present in
    ///   `stage1_outputs` but are no longer in the current selection, including
    ///   threads that are no longer memory-eligible
    pub async fn get_phase2_input_selection(
        &self,
        n: usize,
        max_unused_days: i64,
    ) -> anyhow::Result<Phase2InputSelection> {
        if n == 0 {
            return Ok(Phase2InputSelection::default());
        }
        let cutoff = (Utc::now() - Duration::days(max_unused_days.max(0))).timestamp();

        let current_rows = sqlx::query(
            r#"
SELECT
    so.thread_id,
    COALESCE(t.rollout_path, '') AS rollout_path,
    so.source_updated_at,
    so.raw_memory,
    so.rollout_summary,
    so.rollout_slug,
    so.generated_at,
    COALESCE(t.cwd, '') AS cwd,
    t.git_branch AS git_branch,
    COALESCE(so.usage_count, 0) AS usage_count,
    so.last_usage,
    mnf.reason AS negative_feedback_reason,
    so.selected_for_phase2,
    so.selected_for_phase2_source_updated_at
FROM stage1_outputs AS so
LEFT JOIN threads AS t
    ON t.id = so.thread_id
LEFT JOIN memory_negative_feedback AS mnf
    ON mnf.thread_id = so.thread_id
   AND mnf.source_updated_at = so.source_updated_at
WHERE t.memory_mode = 'enabled'
  AND (length(trim(so.raw_memory)) > 0 OR length(trim(so.rollout_summary)) > 0)
  AND (
        (so.last_usage IS NOT NULL AND so.last_usage >= ?)
        OR (so.last_usage IS NULL AND so.source_updated_at >= ?)
  )
            "#,
        )
        .bind(cutoff)
        .bind(cutoff)
        .fetch_all(self.pool.as_ref())
        .await?;

        let mut current_candidates = current_rows
            .into_iter()
            .map(|row| {
                let selected_for_phase2 = row.try_get::<i64, _>("selected_for_phase2")? != 0;
                let selected_for_phase2_source_updated_at =
                    row.try_get::<Option<i64>, _>("selected_for_phase2_source_updated_at")?;
                let candidate = SessionValueCandidate::from_row(&row)?;
                Ok((
                    candidate,
                    selected_for_phase2,
                    selected_for_phase2_source_updated_at,
                ))
            })
            .collect::<Result<Vec<_>, anyhow::Error>>()?;
        let mut scored_candidates = current_candidates
            .iter()
            .map(|(candidate, _, _)| candidate.clone())
            .collect::<Vec<_>>();
        apply_duplicate_penalties(&mut scored_candidates);
        for ((candidate, _, _), scored_candidate) in current_candidates
            .iter_mut()
            .zip(scored_candidates.into_iter())
        {
            *candidate = scored_candidate;
        }
        current_candidates.sort_by(|lhs, rhs| compare_session_candidates_desc(&lhs.0, &rhs.0));

        let mut current_thread_ids = HashSet::with_capacity(current_candidates.len());
        let mut selected = Vec::with_capacity(current_candidates.len().min(n));
        let mut retained_thread_ids = Vec::new();
        for (candidate, selected_for_phase2, selected_for_phase2_source_updated_at) in
            current_candidates
                .into_iter()
                .filter(|(candidate, _, _)| !candidate.score.suppressed)
                .take(n)
        {
            current_thread_ids.insert(candidate.output.thread_id.to_string());
            let source_updated_at = candidate.output.source_updated_at.timestamp();
            if selected_for_phase2
                && selected_for_phase2_source_updated_at == Some(source_updated_at)
            {
                retained_thread_ids.push(candidate.output.thread_id.clone());
            }
            selected.push(candidate.output);
        }

        let previous_rows = sqlx::query(
            r#"
SELECT
    so.thread_id,
    COALESCE(t.rollout_path, '') AS rollout_path,
    so.source_updated_at,
    so.raw_memory,
    so.rollout_summary,
    so.rollout_slug,
    so.generated_at,
    COALESCE(t.cwd, '') AS cwd,
    t.git_branch AS git_branch
FROM stage1_outputs AS so
LEFT JOIN threads AS t
    ON t.id = so.thread_id
WHERE so.selected_for_phase2 = 1
ORDER BY so.source_updated_at DESC, so.thread_id DESC
            "#,
        )
        .fetch_all(self.pool.as_ref())
        .await?;

        let previous_selected = previous_rows
            .iter()
            .map(Stage1OutputRow::try_from_row)
            .map(|row| row.and_then(Stage1Output::try_from))
            .collect::<Result<Vec<_>, _>>()?;
        let mut removed = Vec::new();
        for row in previous_rows {
            let thread_id = row.try_get::<String, _>("thread_id")?;
            if current_thread_ids.contains(thread_id.as_str()) {
                continue;
            }
            removed.push(stage1_output_ref_from_parts(
                thread_id,
                row.try_get("source_updated_at")?,
                row.try_get("rollout_slug")?,
            )?);
        }

        Ok(Phase2InputSelection {
            selected,
            previous_selected,
            retained_thread_ids,
            removed,
        })
    }

    /// Marks a thread as polluted and enqueues phase-2 forgetting when the
    /// thread participated in the last successful phase-2 baseline.
    pub async fn mark_thread_memory_mode_polluted(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let thread_id = thread_id.to_string();
        let mut tx = self.pool.begin().await?;
        let rows_affected = sqlx::query(
            r#"
UPDATE threads
SET memory_mode = 'polluted'
WHERE id = ? AND memory_mode != 'polluted'
            "#,
        )
        .bind(thread_id.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if rows_affected == 0 {
            tx.commit().await?;
            return Ok(false);
        }

        let selected_for_phase2 = sqlx::query_scalar::<_, i64>(
            r#"
SELECT selected_for_phase2
FROM stage1_outputs
WHERE thread_id = ?
            "#,
        )
        .bind(thread_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(0);
        if selected_for_phase2 != 0 {
            enqueue_global_consolidation_with_executor(&mut *tx, now).await?;
        }

        tx.commit().await?;
        Ok(true)
    }

    /// Attempts to claim a stage-1 job for a thread at `source_updated_at`.
    ///
    /// Claim semantics:
    /// - skips as up-to-date when either:
    ///   - `stage1_outputs.source_updated_at >= source_updated_at`, or
    ///   - `jobs.last_success_watermark >= source_updated_at`
    /// - inserts or updates a `jobs` row to `running` only when:
    ///   - global running job count for `memory_stage1` is below `max_running_jobs`
    ///   - existing row is not actively running with a valid lease
    ///   - retry backoff (if present) has elapsed, or `source_updated_at` advanced
    ///   - retries remain, or `source_updated_at` advanced (which resets retries)
    ///
    /// The update path refreshes ownership token, lease, and `input_watermark`.
    /// If claiming fails, a follow-up read maps current row state to a precise
    /// skip outcome (`SkippedRunning`, `SkippedRetryBackoff`, or
    /// `SkippedRetryExhausted`).
    pub async fn try_claim_stage1_job(
        &self,
        thread_id: ThreadId,
        worker_id: ThreadId,
        source_updated_at: i64,
        lease_seconds: i64,
        max_running_jobs: usize,
    ) -> anyhow::Result<Stage1JobClaimOutcome> {
        let now = Utc::now().timestamp();
        let lease_until = now.saturating_add(lease_seconds.max(0));
        let max_running_jobs = max_running_jobs as i64;
        let ownership_token = Uuid::new_v4().to_string();
        let thread_id = thread_id.to_string();
        let worker_id = worker_id.to_string();

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let existing_output = sqlx::query(
            r#"
SELECT source_updated_at
FROM stage1_outputs
WHERE thread_id = ?
            "#,
        )
        .bind(thread_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(existing_output) = existing_output {
            let existing_source_updated_at: i64 = existing_output.try_get("source_updated_at")?;
            if existing_source_updated_at >= source_updated_at {
                tx.commit().await?;
                return Ok(Stage1JobClaimOutcome::SkippedUpToDate);
            }
        }
        let existing_job = sqlx::query(
            r#"
SELECT last_success_watermark
FROM jobs
WHERE kind = ? AND job_key = ?
            "#,
        )
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(thread_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(existing_job) = existing_job {
            let last_success_watermark =
                existing_job.try_get::<Option<i64>, _>("last_success_watermark")?;
            if last_success_watermark.is_some_and(|watermark| watermark >= source_updated_at) {
                tx.commit().await?;
                return Ok(Stage1JobClaimOutcome::SkippedUpToDate);
            }
        }

        let rows_affected = sqlx::query(
            r#"
INSERT INTO jobs (
    kind,
    job_key,
    status,
    worker_id,
    ownership_token,
    started_at,
    finished_at,
    lease_until,
    retry_at,
    retry_remaining,
    last_error,
    input_watermark,
    last_success_watermark
)
SELECT ?, ?, 'running', ?, ?, ?, NULL, ?, NULL, ?, NULL, ?, NULL
WHERE (
    SELECT COUNT(*)
    FROM jobs
    WHERE kind = ?
      AND status = 'running'
      AND lease_until IS NOT NULL
      AND lease_until > ?
) < ?
ON CONFLICT(kind, job_key) DO UPDATE SET
    status = 'running',
    worker_id = excluded.worker_id,
    ownership_token = excluded.ownership_token,
    started_at = excluded.started_at,
    finished_at = NULL,
    lease_until = excluded.lease_until,
    retry_at = NULL,
    retry_remaining = CASE
        WHEN excluded.input_watermark > COALESCE(jobs.input_watermark, -1) THEN ?
        ELSE jobs.retry_remaining
    END,
    last_error = NULL,
    input_watermark = excluded.input_watermark
WHERE
    (jobs.status != 'running' OR jobs.lease_until IS NULL OR jobs.lease_until <= excluded.started_at)
    AND (
        jobs.retry_at IS NULL
        OR jobs.retry_at <= excluded.started_at
        OR excluded.input_watermark > COALESCE(jobs.input_watermark, -1)
    )
    AND (
        jobs.retry_remaining > 0
        OR excluded.input_watermark > COALESCE(jobs.input_watermark, -1)
    )
    AND (
        SELECT COUNT(*)
        FROM jobs AS running_jobs
        WHERE running_jobs.kind = excluded.kind
          AND running_jobs.status = 'running'
          AND running_jobs.lease_until IS NOT NULL
          AND running_jobs.lease_until > excluded.started_at
          AND running_jobs.job_key != excluded.job_key
    ) < ?
            "#,
        )
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(thread_id.as_str())
        .bind(worker_id.as_str())
        .bind(ownership_token.as_str())
        .bind(now)
        .bind(lease_until)
        .bind(DEFAULT_RETRY_REMAINING)
        .bind(source_updated_at)
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(now)
        .bind(max_running_jobs)
        .bind(DEFAULT_RETRY_REMAINING)
        .bind(max_running_jobs)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if rows_affected > 0 {
            tx.commit().await?;
            return Ok(Stage1JobClaimOutcome::Claimed { ownership_token });
        }

        let existing_job = sqlx::query(
            r#"
SELECT status, lease_until, retry_at, retry_remaining
FROM jobs
WHERE kind = ? AND job_key = ?
            "#,
        )
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(thread_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;

        tx.commit().await?;

        if let Some(existing_job) = existing_job {
            let status: String = existing_job.try_get("status")?;
            let existing_lease_until: Option<i64> = existing_job.try_get("lease_until")?;
            let retry_at: Option<i64> = existing_job.try_get("retry_at")?;
            let retry_remaining: i64 = existing_job.try_get("retry_remaining")?;

            if retry_remaining <= 0 {
                return Ok(Stage1JobClaimOutcome::SkippedRetryExhausted);
            }
            if retry_at.is_some_and(|retry_at| retry_at > now) {
                return Ok(Stage1JobClaimOutcome::SkippedRetryBackoff);
            }
            if status == "running"
                && existing_lease_until.is_some_and(|lease_until| lease_until > now)
            {
                return Ok(Stage1JobClaimOutcome::SkippedRunning);
            }
        }

        Ok(Stage1JobClaimOutcome::SkippedRunning)
    }

    /// Marks a claimed stage-1 job successful and upserts generated output.
    ///
    /// Transaction behavior:
    /// - updates `jobs` only for the currently owned running row
    /// - sets `status='done'` and `last_success_watermark = input_watermark`
    /// - upserts `stage1_outputs` for the thread, replacing existing output only
    ///   when `source_updated_at` is newer or equal
    /// - preserves any existing `selected_for_phase2` baseline until the next
    ///   successful phase-2 run rewrites the baseline selection, including the
    ///   snapshot timestamp chosen during that run
    /// - persists optional `rollout_slug` for rollout summary artifact naming
    /// - enqueues/advances the global phase-2 job watermark using
    ///   `source_updated_at`
    pub async fn mark_stage1_job_succeeded(
        &self,
        thread_id: ThreadId,
        ownership_token: &str,
        source_updated_at: i64,
        raw_memory: &str,
        rollout_summary: &str,
        rollout_slug: Option<&str>,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let thread_id = thread_id.to_string();

        let mut tx = self.pool.begin().await?;
        let rows_affected = sqlx::query(
            r#"
UPDATE jobs
SET
    status = 'done',
    finished_at = ?,
    lease_until = NULL,
    last_error = NULL,
    last_success_watermark = input_watermark
WHERE kind = ? AND job_key = ?
  AND status = 'running' AND ownership_token = ?
            "#,
        )
        .bind(now)
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(thread_id.as_str())
        .bind(ownership_token)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if rows_affected == 0 {
            tx.commit().await?;
            return Ok(false);
        }

        sqlx::query(
            r#"
INSERT INTO stage1_outputs (
    thread_id,
    source_updated_at,
    raw_memory,
    rollout_summary,
    rollout_slug,
    generated_at
) VALUES (?, ?, ?, ?, ?, ?)
ON CONFLICT(thread_id) DO UPDATE SET
    source_updated_at = excluded.source_updated_at,
    raw_memory = excluded.raw_memory,
    rollout_summary = excluded.rollout_summary,
    rollout_slug = excluded.rollout_slug,
    generated_at = excluded.generated_at
WHERE excluded.source_updated_at >= stage1_outputs.source_updated_at
            "#,
        )
        .bind(thread_id.as_str())
        .bind(source_updated_at)
        .bind(raw_memory)
        .bind(rollout_summary)
        .bind(rollout_slug)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        enqueue_global_consolidation_with_executor(&mut *tx, source_updated_at).await?;

        tx.commit().await?;
        Ok(true)
    }

    /// Marks a claimed stage-1 job successful when extraction produced no output.
    ///
    /// Transaction behavior:
    /// - updates `jobs` only for the currently owned running row
    /// - sets `status='done'` and `last_success_watermark = input_watermark`
    /// - deletes any existing `stage1_outputs` row for the thread
    /// - enqueues/advances the global phase-2 job watermark using the claimed
    ///   `input_watermark` only when deleting an existing `stage1_outputs` row
    pub async fn mark_stage1_job_succeeded_no_output(
        &self,
        thread_id: ThreadId,
        ownership_token: &str,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let thread_id = thread_id.to_string();

        let mut tx = self.pool.begin().await?;
        let rows_affected = sqlx::query(
            r#"
UPDATE jobs
SET
    status = 'done',
    finished_at = ?,
    lease_until = NULL,
    last_error = NULL,
    last_success_watermark = input_watermark
WHERE kind = ? AND job_key = ?
  AND status = 'running' AND ownership_token = ?
            "#,
        )
        .bind(now)
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(thread_id.as_str())
        .bind(ownership_token)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if rows_affected == 0 {
            tx.commit().await?;
            return Ok(false);
        }

        let source_updated_at = sqlx::query(
            r#"
SELECT input_watermark
FROM jobs
WHERE kind = ? AND job_key = ? AND ownership_token = ?
            "#,
        )
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(thread_id.as_str())
        .bind(ownership_token)
        .fetch_one(&mut *tx)
        .await?
        .try_get::<i64, _>("input_watermark")?;

        let deleted_rows = sqlx::query(
            r#"
DELETE FROM stage1_outputs
WHERE thread_id = ?
            "#,
        )
        .bind(thread_id.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if deleted_rows > 0 {
            enqueue_global_consolidation_with_executor(&mut *tx, source_updated_at).await?;
        }

        tx.commit().await?;
        Ok(true)
    }

    /// Marks a claimed stage-1 job as failed and schedules retry backoff.
    ///
    /// Query behavior:
    /// - updates only the owned running row for `(kind='memory_stage1', job_key)`
    /// - sets `status='error'`, clears lease, writes `last_error`
    /// - decrements `retry_remaining`
    /// - sets `retry_at = now + retry_delay_seconds`
    pub async fn mark_stage1_job_failed(
        &self,
        thread_id: ThreadId,
        ownership_token: &str,
        failure_reason: &str,
        retry_delay_seconds: i64,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let retry_at = now.saturating_add(retry_delay_seconds.max(0));
        let thread_id = thread_id.to_string();

        let rows_affected = sqlx::query(
            r#"
UPDATE jobs
SET
    status = 'error',
    finished_at = ?,
    lease_until = NULL,
    retry_at = ?,
    retry_remaining = retry_remaining - 1,
    last_error = ?
WHERE kind = ? AND job_key = ?
  AND status = 'running' AND ownership_token = ?
            "#,
        )
        .bind(now)
        .bind(retry_at)
        .bind(failure_reason)
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(thread_id.as_str())
        .bind(ownership_token)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();

        Ok(rows_affected > 0)
    }

    /// Enqueues or advances the global phase-2 consolidation job watermark.
    ///
    /// The underlying upsert keeps the job `running` when already running, resets
    /// `pending/error` jobs to `pending`, and advances `input_watermark` so each
    /// enqueue marks new consolidation work even when `source_updated_at` is
    /// older than prior maxima.
    pub async fn enqueue_global_consolidation(&self, input_watermark: i64) -> anyhow::Result<()> {
        enqueue_global_consolidation_with_executor(self.pool.as_ref(), input_watermark).await
    }

    /// Attempts to claim the global phase-2 consolidation job.
    ///
    /// Claim semantics:
    /// - reads the singleton global job row (`kind='memory_consolidate_global'`)
    /// - returns `SkippedNotDirty` when `input_watermark <= last_success_watermark`
    /// - returns `SkippedNotDirty` when retries are exhausted or retry backoff is active
    /// - returns `SkippedRunning` when an active running lease exists
    /// - otherwise updates the row to `running`, sets ownership + lease, and
    ///   returns `Claimed`
    pub async fn try_claim_global_phase2_job(
        &self,
        worker_id: ThreadId,
        lease_seconds: i64,
    ) -> anyhow::Result<Phase2JobClaimOutcome> {
        let now = Utc::now().timestamp();
        let lease_until = now.saturating_add(lease_seconds.max(0));
        let ownership_token = Uuid::new_v4().to_string();
        let worker_id = worker_id.to_string();

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let existing_job = sqlx::query(
            r#"
SELECT status, lease_until, retry_at, retry_remaining, input_watermark, last_success_watermark
FROM jobs
WHERE kind = ? AND job_key = ?
            "#,
        )
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(existing_job) = existing_job else {
            tx.commit().await?;
            return Ok(Phase2JobClaimOutcome::SkippedNotDirty);
        };

        let input_watermark: Option<i64> = existing_job.try_get("input_watermark")?;
        let input_watermark_value = input_watermark.unwrap_or(0);
        let last_success_watermark: Option<i64> = existing_job.try_get("last_success_watermark")?;
        if input_watermark_value <= last_success_watermark.unwrap_or(0) {
            tx.commit().await?;
            return Ok(Phase2JobClaimOutcome::SkippedNotDirty);
        }

        let status: String = existing_job.try_get("status")?;
        let existing_lease_until: Option<i64> = existing_job.try_get("lease_until")?;
        let retry_at: Option<i64> = existing_job.try_get("retry_at")?;
        let retry_remaining: i64 = existing_job.try_get("retry_remaining")?;

        if retry_remaining <= 0 {
            tx.commit().await?;
            return Ok(Phase2JobClaimOutcome::SkippedNotDirty);
        }
        if retry_at.is_some_and(|retry_at| retry_at > now) {
            tx.commit().await?;
            return Ok(Phase2JobClaimOutcome::SkippedNotDirty);
        }
        if status == "running" && existing_lease_until.is_some_and(|lease_until| lease_until > now)
        {
            tx.commit().await?;
            return Ok(Phase2JobClaimOutcome::SkippedRunning);
        }

        let rows_affected = sqlx::query(
            r#"
UPDATE jobs
SET
    status = 'running',
    worker_id = ?,
    ownership_token = ?,
    started_at = ?,
    finished_at = NULL,
    lease_until = ?,
    retry_at = NULL,
    last_error = NULL
WHERE kind = ? AND job_key = ?
  AND input_watermark > COALESCE(last_success_watermark, 0)
  AND (status != 'running' OR lease_until IS NULL OR lease_until <= ?)
  AND (retry_at IS NULL OR retry_at <= ?)
  AND retry_remaining > 0
            "#,
        )
        .bind(worker_id.as_str())
        .bind(ownership_token.as_str())
        .bind(now)
        .bind(lease_until)
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        tx.commit().await?;
        if rows_affected == 0 {
            Ok(Phase2JobClaimOutcome::SkippedRunning)
        } else {
            Ok(Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark: input_watermark_value,
            })
        }
    }

    /// Extends the lease for an owned running phase-2 global job.
    ///
    /// Query behavior:
    /// - `UPDATE jobs SET lease_until = ?` for the singleton global row
    /// - requires `status='running'` and matching `ownership_token`
    pub async fn heartbeat_global_phase2_job(
        &self,
        ownership_token: &str,
        lease_seconds: i64,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let lease_until = now.saturating_add(lease_seconds.max(0));
        let rows_affected = sqlx::query(
            r#"
UPDATE jobs
SET lease_until = ?
WHERE kind = ? AND job_key = ?
  AND status = 'running' AND ownership_token = ?
            "#,
        )
        .bind(lease_until)
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .bind(ownership_token)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();

        Ok(rows_affected > 0)
    }

    /// Marks the owned running global phase-2 job as succeeded.
    ///
    /// Query behavior:
    /// - updates only the owned running singleton global row
    /// - sets `status='done'`, clears lease/errors
    /// - advances `last_success_watermark` to
    ///   `max(existing_last_success_watermark, completed_watermark)`
    /// - rewrites `selected_for_phase2` so only the exact selected stage-1
    ///   snapshots remain marked as part of the latest successful phase-2
    ///   selection, and persists each selected snapshot's
    ///   `source_updated_at` for future retained-vs-added diffing
    pub async fn mark_global_phase2_job_succeeded(
        &self,
        ownership_token: &str,
        completed_watermark: i64,
        selected_outputs: &[Stage1Output],
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let mut tx = self.pool.begin().await?;
        let rows_affected = sqlx::query(
            r#"
UPDATE jobs
SET
    status = 'done',
    finished_at = ?,
    lease_until = NULL,
    last_error = NULL,
    last_success_watermark = max(COALESCE(last_success_watermark, 0), ?)
WHERE kind = ? AND job_key = ?
  AND status = 'running' AND ownership_token = ?
            "#,
        )
        .bind(now)
        .bind(completed_watermark)
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .bind(ownership_token)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if rows_affected == 0 {
            tx.commit().await?;
            return Ok(false);
        }

        sqlx::query(
            r#"
UPDATE stage1_outputs
SET
    selected_for_phase2 = 0,
    selected_for_phase2_source_updated_at = NULL
WHERE selected_for_phase2 != 0 OR selected_for_phase2_source_updated_at IS NOT NULL
            "#,
        )
        .execute(&mut *tx)
        .await?;

        for output in selected_outputs {
            sqlx::query(
                r#"
UPDATE stage1_outputs
SET
    selected_for_phase2 = 1,
    selected_for_phase2_source_updated_at = ?
WHERE thread_id = ? AND source_updated_at = ?
                "#,
            )
            .bind(output.source_updated_at.timestamp())
            .bind(output.thread_id.to_string())
            .bind(output.source_updated_at.timestamp())
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(true)
    }

    /// Marks the owned running global phase-2 job as failed and schedules retry.
    ///
    /// Query behavior:
    /// - updates only the owned running singleton global row
    /// - sets `status='error'`, clears lease
    /// - writes failure reason and retry time
    /// - decrements `retry_remaining`
    pub async fn mark_global_phase2_job_failed(
        &self,
        ownership_token: &str,
        failure_reason: &str,
        retry_delay_seconds: i64,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let retry_at = now.saturating_add(retry_delay_seconds.max(0));
        let rows_affected = sqlx::query(
            r#"
UPDATE jobs
SET
    status = 'error',
    finished_at = ?,
    lease_until = NULL,
    retry_at = ?,
    retry_remaining = retry_remaining - 1,
    last_error = ?
WHERE kind = ? AND job_key = ?
  AND status = 'running' AND ownership_token = ?
            "#,
        )
        .bind(now)
        .bind(retry_at)
        .bind(failure_reason)
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .bind(ownership_token)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();

        Ok(rows_affected > 0)
    }

    /// Fallback failure finalization when ownership may have been lost.
    ///
    /// Query behavior:
    /// - same state transition as [`Self::mark_global_phase2_job_failed`]
    /// - matches rows where `ownership_token = ? OR ownership_token IS NULL`
    /// - allows recovering a stuck unowned running row
    pub async fn mark_global_phase2_job_failed_if_unowned(
        &self,
        ownership_token: &str,
        failure_reason: &str,
        retry_delay_seconds: i64,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let retry_at = now.saturating_add(retry_delay_seconds.max(0));
        let rows_affected = sqlx::query(
            r#"
UPDATE jobs
SET
    status = 'error',
    finished_at = ?,
    lease_until = NULL,
    retry_at = ?,
    retry_remaining = retry_remaining - 1,
    last_error = ?
WHERE kind = ? AND job_key = ?
  AND status = 'running'
  AND (ownership_token = ? OR ownership_token IS NULL)
            "#,
        )
        .bind(now)
        .bind(retry_at)
        .bind(failure_reason)
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .bind(ownership_token)
        .execute(self.pool.as_ref())
        .await?
        .rows_affected();

        Ok(rows_affected > 0)
    }
}

fn summarize_memory_peek_excerpt(rollout_summary: &str, raw_memory: &str) -> String {
    const MAX_CHARS: usize = 220;

    let source = if rollout_summary.trim().is_empty() {
        raw_memory
    } else {
        rollout_summary
    };
    let normalized = source.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MAX_CHARS {
        return normalized;
    }

    let truncated = normalized
        .chars()
        .take(MAX_CHARS.saturating_sub(3))
        .collect::<String>();
    format!("{truncated}...")
}

async fn collect_directory_health(path: &Path) -> MemoryDirectoryHealth {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => MemoryDirectoryHealth {
            path: path.to_path_buf(),
            exists: true,
            is_directory: metadata.is_dir(),
        },
        Err(_) => MemoryDirectoryHealth {
            path: path.to_path_buf(),
            exists: false,
            is_directory: false,
        },
    }
}

async fn collect_artifact_health(kind: &str, path: PathBuf) -> MemoryHealthArtifact {
    let (exists, is_file, size_bytes) = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) => (true, metadata.is_file(), Some(metadata.len())),
        Err(_) => (false, false, None),
    };
    let readable = if exists && is_file {
        tokio::fs::read(&path).await.is_ok()
    } else {
        false
    };

    MemoryHealthArtifact {
        kind: kind.to_string(),
        path,
        exists,
        is_file,
        readable,
        size_bytes,
    }
}

async fn collect_rollout_summary_health(
    file_name: &str,
    path: PathBuf,
) -> MemoryHealthRolloutSummary {
    let artifact = collect_artifact_health("rollout_summary", path.clone()).await;
    MemoryHealthRolloutSummary {
        file_name: file_name.to_string(),
        path,
        exists: artifact.exists,
        is_file: artifact.is_file,
        readable: artifact.readable,
        size_bytes: artifact.size_bytes,
    }
}

fn artifact_by_kind<'a>(
    artifacts: &'a [MemoryHealthArtifact],
    kind: &str,
) -> Option<&'a MemoryHealthArtifact> {
    artifacts.iter().find(|artifact| artifact.kind == kind)
}

fn evaluate_memory_artifact_health(
    issues: &mut Vec<MemoryHealthIssue>,
    artifact: &MemoryHealthArtifact,
    expected: bool,
) {
    if !artifact.exists {
        if expected {
            issues.push(MemoryHealthIssue {
                severity: MemoryHealthSeverity::Error,
                code: format!("missing_{}", artifact.kind),
                message: format!(
                    "expected memory artifact is missing: {}",
                    artifact.path.display()
                ),
            });
        }
        return;
    }

    if !artifact.is_file || !artifact.readable || artifact.size_bytes == Some(0) {
        issues.push(MemoryHealthIssue {
            severity: if expected {
                MemoryHealthSeverity::Error
            } else {
                MemoryHealthSeverity::Warning
            },
            code: format!("invalid_{}", artifact.kind),
            message: format!(
                "memory artifact is malformed or unreadable: {}",
                artifact.path.display()
            ),
        });
        return;
    }

    if !expected && artifact.kind != "raw_memories" {
        issues.push(MemoryHealthIssue {
            severity: MemoryHealthSeverity::Warning,
            code: format!("stale_{}", artifact.kind),
            message: format!(
                "memory artifact exists on disk but no current phase-2 input requires it: {}",
                artifact.path.display()
            ),
        });
    }
}

async fn enqueue_global_consolidation_with_executor<'e, E>(
    executor: E,
    input_watermark: i64,
) -> anyhow::Result<()>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query(
        r#"
INSERT INTO jobs (
    kind,
    job_key,
    status,
    worker_id,
    ownership_token,
    started_at,
    finished_at,
    lease_until,
    retry_at,
    retry_remaining,
    last_error,
    input_watermark,
    last_success_watermark
) VALUES (?, ?, 'pending', NULL, NULL, NULL, NULL, NULL, NULL, ?, NULL, ?, 0)
ON CONFLICT(kind, job_key) DO UPDATE SET
    status = CASE
        WHEN jobs.status = 'running' THEN 'running'
        ELSE 'pending'
    END,
    retry_at = CASE
        WHEN jobs.status = 'running' THEN jobs.retry_at
        ELSE NULL
    END,
    retry_remaining = max(jobs.retry_remaining, excluded.retry_remaining),
    input_watermark = CASE
        WHEN excluded.input_watermark > COALESCE(jobs.input_watermark, 0)
            THEN excluded.input_watermark
        ELSE COALESCE(jobs.input_watermark, 0) + 1
    END
        "#,
    )
    .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
    .bind(MEMORY_CONSOLIDATION_JOB_KEY)
    .bind(DEFAULT_RETRY_REMAINING)
    .bind(input_watermark)
    .execute(executor)
    .await?;

    Ok(())
}

async fn resolve_memory_negative_feedback_target<'e, E>(
    executor: E,
    target: &MemoryNegativeFeedbackTarget,
) -> anyhow::Result<Stage1OutputRef>
where
    E: Executor<'e, Database = Sqlite>,
{
    let row = if let Some(source_updated_at) = target.source_updated_at {
        sqlx::query(
            r#"
SELECT thread_id, source_updated_at, rollout_slug
FROM stage1_outputs
WHERE thread_id = ? AND source_updated_at = ?
            "#,
        )
        .bind(target.thread_id.to_string())
        .bind(source_updated_at.timestamp())
        .fetch_optional(executor)
        .await?
    } else {
        sqlx::query(
            r#"
SELECT thread_id, source_updated_at, rollout_slug
FROM stage1_outputs
WHERE thread_id = ?
            "#,
        )
        .bind(target.thread_id.to_string())
        .fetch_optional(executor)
        .await?
    };

    let Some(row) = row else {
        anyhow::bail!(
            "no persisted stage-1 output found for thread {}",
            target.thread_id
        );
    };

    let resolved = stage1_output_ref_from_parts(
        row.try_get::<String, _>("thread_id")?,
        row.try_get::<i64, _>("source_updated_at")?,
        row.try_get::<Option<String>, _>("rollout_slug")?,
    )?;

    if let Some(expected_rollout_slug) = target.rollout_slug.as_deref()
        && resolved.rollout_slug.as_deref() != Some(expected_rollout_slug)
    {
        anyhow::bail!(
            "rollout slug mismatch for thread {}: expected {:?}, found {:?}",
            target.thread_id,
            expected_rollout_slug,
            resolved.rollout_slug
        );
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL;
    use super::JOB_KIND_MEMORY_STAGE1;
    use super::StateRuntime;
    use super::test_support::test_thread_metadata;
    use super::test_support::unique_temp_dir;
    use crate::MemoryHealthSeverity;
    use crate::MemoryNegativeFeedbackReason;
    use crate::MemoryNegativeFeedbackTarget;
    use crate::model::Phase2JobClaimOutcome;
    use crate::model::Stage1JobClaimOutcome;
    use crate::model::Stage1StartupClaimParams;
    use crate::model::rollout_summary_file_name_from_parts;
    use chrono::Duration;
    use chrono::TimeZone;
    use chrono::Utc;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;
    use sqlx::Row;
    use std::path::Path;
    use std::sync::Arc;
    use uuid::Uuid;

    #[tokio::test]
    async fn stage1_claim_skips_when_up_to_date() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let metadata = test_thread_metadata(&codex_home, thread_id, codex_home.join("a"));
        runtime
            .upsert_thread(&metadata)
            .await
            .expect("upsert thread");

        let owner_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let owner_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");

        let claim = runtime
            .try_claim_stage1_job(
                thread_id, owner_a, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 job");
        let ownership_token = match claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected claim outcome: {other:?}"),
        };

        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_id,
                    ownership_token.as_str(),
                    /*source_updated_at*/ 100,
                    "raw",
                    "sum",
                    /*rollout_slug*/ None,
                )
                .await
                .expect("mark stage1 succeeded"),
            "stage1 success should finalize for current token"
        );

        let up_to_date = runtime
            .try_claim_stage1_job(
                thread_id, owner_b, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 up-to-date");
        assert_eq!(up_to_date, Stage1JobClaimOutcome::SkippedUpToDate);

        let needs_rerun = runtime
            .try_claim_stage1_job(
                thread_id, owner_b, /*source_updated_at*/ 101, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 newer source");
        assert!(
            matches!(needs_rerun, Stage1JobClaimOutcome::Claimed { .. }),
            "newer source_updated_at should be claimable"
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn stage1_running_stale_can_be_stolen_but_fresh_running_is_skipped() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let owner_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let owner_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let cwd = codex_home.join("workspace");
        runtime
            .upsert_thread(&test_thread_metadata(&codex_home, thread_id, cwd))
            .await
            .expect("upsert thread");

        let claim_a = runtime
            .try_claim_stage1_job(
                thread_id, owner_a, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim a");
        assert!(matches!(claim_a, Stage1JobClaimOutcome::Claimed { .. }));

        let claim_b_fresh = runtime
            .try_claim_stage1_job(
                thread_id, owner_b, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim b fresh");
        assert_eq!(claim_b_fresh, Stage1JobClaimOutcome::SkippedRunning);

        sqlx::query("UPDATE jobs SET lease_until = 0 WHERE kind = 'memory_stage1' AND job_key = ?")
            .bind(thread_id.to_string())
            .execute(runtime.pool.as_ref())
            .await
            .expect("force stale lease");

        let claim_b_stale = runtime
            .try_claim_stage1_job(
                thread_id, owner_b, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim b stale");
        assert!(matches!(
            claim_b_stale,
            Stage1JobClaimOutcome::Claimed { .. }
        ));

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn stage1_concurrent_claim_for_same_thread_is_conflict_safe() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.join("workspace"),
            ))
            .await
            .expect("upsert thread");

        let owner_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let owner_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let thread_id_a = thread_id;
        let thread_id_b = thread_id;
        let runtime_a = Arc::clone(&runtime);
        let runtime_b = Arc::clone(&runtime);
        let claim_with_retry = |runtime: Arc<StateRuntime>,
                                thread_id: ThreadId,
                                owner: ThreadId| async move {
            for attempt in 0..5 {
                match runtime
                    .try_claim_stage1_job(
                        thread_id, owner, /*source_updated_at*/ 100,
                        /*lease_seconds*/ 3_600, /*max_running_jobs*/ 64,
                    )
                    .await
                {
                    Ok(outcome) => return outcome,
                    Err(err) if err.to_string().contains("database is locked") && attempt < 4 => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    Err(err) => panic!("claim stage1 should not fail: {err}"),
                }
            }
            panic!("claim stage1 should have returned within retry budget")
        };

        let (claim_a, claim_b) = tokio::join!(
            claim_with_retry(runtime_a, thread_id_a, owner_a),
            claim_with_retry(runtime_b, thread_id_b, owner_b),
        );

        let claim_outcomes = vec![claim_a, claim_b];
        let claimed_count = claim_outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Stage1JobClaimOutcome::Claimed { .. }))
            .count();
        assert_eq!(claimed_count, 1);
        assert!(
            claim_outcomes.iter().all(|outcome| {
                matches!(
                    outcome,
                    Stage1JobClaimOutcome::Claimed { .. } | Stage1JobClaimOutcome::SkippedRunning
                )
            }),
            "unexpected claim outcomes: {claim_outcomes:?}"
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn stage1_concurrent_claims_respect_running_cap() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let thread_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_a,
                codex_home.join("workspace-a"),
            ))
            .await
            .expect("upsert thread a");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_b,
                codex_home.join("workspace-b"),
            ))
            .await
            .expect("upsert thread b");

        let owner_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let owner_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let runtime_a = Arc::clone(&runtime);
        let runtime_b = Arc::clone(&runtime);

        let (claim_a, claim_b) = tokio::join!(
            async move {
                runtime_a
                    .try_claim_stage1_job(
                        thread_a, owner_a, /*source_updated_at*/ 100,
                        /*lease_seconds*/ 3_600, /*max_running_jobs*/ 1,
                    )
                    .await
                    .expect("claim stage1 thread a")
            },
            async move {
                runtime_b
                    .try_claim_stage1_job(
                        thread_b, owner_b, /*source_updated_at*/ 101,
                        /*lease_seconds*/ 3_600, /*max_running_jobs*/ 1,
                    )
                    .await
                    .expect("claim stage1 thread b")
            },
        );

        let claim_outcomes = vec![claim_a, claim_b];
        let claimed_count = claim_outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Stage1JobClaimOutcome::Claimed { .. }))
            .count();
        assert_eq!(claimed_count, 1);
        assert!(
            claim_outcomes
                .iter()
                .any(|outcome| { matches!(outcome, Stage1JobClaimOutcome::SkippedRunning) }),
            "one concurrent claim should be throttled by running cap: {claim_outcomes:?}"
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn claim_stage1_jobs_filters_by_age_idle_and_current_thread() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let now = Utc::now();
        let fresh_at = now - Duration::hours(1);
        let just_under_idle_at = now - Duration::hours(12) + Duration::minutes(1);
        let eligible_idle_at = now - Duration::hours(12) - Duration::minutes(1);
        let old_at = now - Duration::days(31);

        let current_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("current thread id");
        let fresh_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("fresh thread id");
        let just_under_idle_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("just under idle thread id");
        let eligible_idle_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("eligible idle thread id");
        let old_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("old thread id");

        let mut current =
            test_thread_metadata(&codex_home, current_thread_id, codex_home.join("current"));
        current.created_at = now;
        current.updated_at = now;
        runtime
            .upsert_thread(&current)
            .await
            .expect("upsert current");

        let mut fresh =
            test_thread_metadata(&codex_home, fresh_thread_id, codex_home.join("fresh"));
        fresh.created_at = fresh_at;
        fresh.updated_at = fresh_at;
        runtime.upsert_thread(&fresh).await.expect("upsert fresh");

        let mut just_under_idle = test_thread_metadata(
            &codex_home,
            just_under_idle_thread_id,
            codex_home.join("just-under-idle"),
        );
        just_under_idle.created_at = just_under_idle_at;
        just_under_idle.updated_at = just_under_idle_at;
        runtime
            .upsert_thread(&just_under_idle)
            .await
            .expect("upsert just-under-idle");

        let mut eligible_idle = test_thread_metadata(
            &codex_home,
            eligible_idle_thread_id,
            codex_home.join("eligible-idle"),
        );
        eligible_idle.created_at = eligible_idle_at;
        eligible_idle.updated_at = eligible_idle_at;
        runtime
            .upsert_thread(&eligible_idle)
            .await
            .expect("upsert eligible-idle");

        let mut old = test_thread_metadata(&codex_home, old_thread_id, codex_home.join("old"));
        old.created_at = old_at;
        old.updated_at = old_at;
        runtime.upsert_thread(&old).await.expect("upsert old");

        let allowed_sources = vec!["cli".to_string()];
        let claims = runtime
            .claim_stage1_jobs_for_startup(
                current_thread_id,
                Stage1StartupClaimParams {
                    scan_limit: 1,
                    max_claimed: 5,
                    max_age_days: 30,
                    min_rollout_idle_hours: 12,
                    allowed_sources: allowed_sources.as_slice(),
                    lease_seconds: 3600,
                },
            )
            .await
            .expect("claim stage1 jobs");

        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].thread.id, eligible_idle_thread_id);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn claim_stage1_jobs_prefilters_threads_with_up_to_date_memory() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let now = Utc::now();
        let eligible_newer_at = now - Duration::hours(13);
        let eligible_older_at = now - Duration::hours(14);

        let current_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("current thread id");
        let up_to_date_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("up-to-date thread id");
        let stale_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("stale thread id");
        let worker_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("worker id");

        let mut current =
            test_thread_metadata(&codex_home, current_thread_id, codex_home.join("current"));
        current.created_at = now;
        current.updated_at = now;
        runtime
            .upsert_thread(&current)
            .await
            .expect("upsert current thread");

        let mut up_to_date = test_thread_metadata(
            &codex_home,
            up_to_date_thread_id,
            codex_home.join("up-to-date"),
        );
        up_to_date.created_at = eligible_newer_at;
        up_to_date.updated_at = eligible_newer_at;
        runtime
            .upsert_thread(&up_to_date)
            .await
            .expect("upsert up-to-date thread");

        let up_to_date_claim = runtime
            .try_claim_stage1_job(
                up_to_date_thread_id,
                worker_id,
                up_to_date.updated_at.timestamp(),
                /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim up-to-date thread for seed");
        let up_to_date_token = match up_to_date_claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected seed claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    up_to_date_thread_id,
                    up_to_date_token.as_str(),
                    up_to_date.updated_at.timestamp(),
                    "raw",
                    "summary",
                    /*rollout_slug*/ None,
                )
                .await
                .expect("mark up-to-date thread succeeded"),
            "seed stage1 success should complete for up-to-date thread"
        );

        let mut stale =
            test_thread_metadata(&codex_home, stale_thread_id, codex_home.join("stale"));
        stale.created_at = eligible_older_at;
        stale.updated_at = eligible_older_at;
        runtime
            .upsert_thread(&stale)
            .await
            .expect("upsert stale thread");

        let allowed_sources = vec!["cli".to_string()];
        let claims = runtime
            .claim_stage1_jobs_for_startup(
                current_thread_id,
                Stage1StartupClaimParams {
                    scan_limit: 1,
                    max_claimed: 1,
                    max_age_days: 30,
                    min_rollout_idle_hours: 12,
                    allowed_sources: allowed_sources.as_slice(),
                    lease_seconds: 3600,
                },
            )
            .await
            .expect("claim stage1 startup jobs");
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].thread.id, stale_thread_id);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn claim_stage1_jobs_skips_threads_with_disabled_memory_mode() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let now = Utc::now();
        let eligible_at = now - Duration::hours(13);

        let current_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("current thread id");
        let disabled_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("disabled thread id");
        let enabled_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("enabled thread id");

        let mut current =
            test_thread_metadata(&codex_home, current_thread_id, codex_home.join("current"));
        current.created_at = now;
        current.updated_at = now;
        runtime
            .upsert_thread(&current)
            .await
            .expect("upsert current thread");

        let mut disabled =
            test_thread_metadata(&codex_home, disabled_thread_id, codex_home.join("disabled"));
        disabled.created_at = eligible_at;
        disabled.updated_at = eligible_at;
        runtime
            .upsert_thread(&disabled)
            .await
            .expect("upsert disabled thread");
        sqlx::query("UPDATE threads SET memory_mode = 'disabled' WHERE id = ?")
            .bind(disabled_thread_id.to_string())
            .execute(runtime.pool.as_ref())
            .await
            .expect("disable thread memory mode");

        let mut enabled =
            test_thread_metadata(&codex_home, enabled_thread_id, codex_home.join("enabled"));
        enabled.created_at = eligible_at;
        enabled.updated_at = eligible_at;
        runtime
            .upsert_thread(&enabled)
            .await
            .expect("upsert enabled thread");

        let allowed_sources = vec!["cli".to_string()];
        let claims = runtime
            .claim_stage1_jobs_for_startup(
                current_thread_id,
                Stage1StartupClaimParams {
                    scan_limit: 10,
                    max_claimed: 10,
                    max_age_days: 30,
                    min_rollout_idle_hours: 12,
                    allowed_sources: allowed_sources.as_slice(),
                    lease_seconds: 3600,
                },
            )
            .await
            .expect("claim stage1 startup jobs");

        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].thread.id, enabled_thread_id);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn clear_memory_data_clears_rows_and_preserves_thread_memory_modes() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let now = Utc::now() - Duration::hours(13);
        let worker_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("worker id");
        let enabled_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("enabled thread id");
        let disabled_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("disabled thread id");

        let mut enabled =
            test_thread_metadata(&codex_home, enabled_thread_id, codex_home.join("enabled"));
        enabled.created_at = now;
        enabled.updated_at = now;
        runtime
            .upsert_thread(&enabled)
            .await
            .expect("upsert enabled thread");

        let claim = runtime
            .try_claim_stage1_job(
                enabled_thread_id,
                worker_id,
                enabled.updated_at.timestamp(),
                /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim enabled thread");
        let ownership_token = match claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    enabled_thread_id,
                    ownership_token.as_str(),
                    enabled.updated_at.timestamp(),
                    "raw",
                    "summary",
                    /*rollout_slug*/ None,
                )
                .await
                .expect("mark enabled thread succeeded"),
            "stage1 success should be recorded"
        );
        runtime
            .enqueue_global_consolidation(enabled.updated_at.timestamp())
            .await
            .expect("enqueue global consolidation");
        runtime
            .record_memory_negative_feedback(
                &MemoryNegativeFeedbackTarget {
                    thread_id: enabled_thread_id,
                    source_updated_at: Some(enabled.updated_at),
                    rollout_slug: None,
                },
                MemoryNegativeFeedbackReason::Transient,
            )
            .await
            .expect("record negative feedback");

        let mut disabled =
            test_thread_metadata(&codex_home, disabled_thread_id, codex_home.join("disabled"));
        disabled.created_at = now;
        disabled.updated_at = now;
        runtime
            .upsert_thread(&disabled)
            .await
            .expect("upsert disabled thread");
        sqlx::query("UPDATE threads SET memory_mode = 'disabled' WHERE id = ?")
            .bind(disabled_thread_id.to_string())
            .execute(runtime.pool.as_ref())
            .await
            .expect("disable existing thread");

        runtime
            .clear_memory_data()
            .await
            .expect("clear memory data");

        let stage1_outputs_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stage1_outputs")
            .fetch_one(runtime.pool.as_ref())
            .await
            .expect("count stage1 outputs");
        assert_eq!(stage1_outputs_count, 0);

        let memory_jobs_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = ? OR kind = ?")
                .bind(JOB_KIND_MEMORY_STAGE1)
                .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
                .fetch_one(runtime.pool.as_ref())
                .await
                .expect("count memory jobs");
        assert_eq!(memory_jobs_count, 0);

        let negative_feedback_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM memory_negative_feedback")
                .fetch_one(runtime.pool.as_ref())
                .await
                .expect("count memory negative feedback");
        assert_eq!(negative_feedback_count, 0);

        let enabled_memory_mode: String =
            sqlx::query_scalar("SELECT memory_mode FROM threads WHERE id = ?")
                .bind(enabled_thread_id.to_string())
                .fetch_one(runtime.pool.as_ref())
                .await
                .expect("read enabled thread memory mode");
        assert_eq!(enabled_memory_mode, "enabled");

        let disabled_memory_mode: String =
            sqlx::query_scalar("SELECT memory_mode FROM threads WHERE id = ?")
                .bind(disabled_thread_id.to_string())
                .fetch_one(runtime.pool.as_ref())
                .await
                .expect("read disabled thread memory mode");
        assert_eq!(disabled_memory_mode, "disabled");

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn claim_stage1_jobs_enforces_global_running_cap() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let current_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("current thread id");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                current_thread_id,
                codex_home.join("current"),
            ))
            .await
            .expect("upsert current");

        let now = Utc::now();
        let started_at = now.timestamp();
        let lease_until = started_at + 3600;
        let eligible_at = now - Duration::hours(13);
        let existing_running = 10usize;
        let total_candidates = 80usize;

        for idx in 0..total_candidates {
            let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
            let mut metadata = test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.join(format!("thread-{idx}")),
            );
            metadata.created_at = eligible_at - Duration::seconds(idx as i64);
            metadata.updated_at = eligible_at - Duration::seconds(idx as i64);
            runtime
                .upsert_thread(&metadata)
                .await
                .expect("upsert thread");

            if idx < existing_running {
                sqlx::query(
                    r#"
INSERT INTO jobs (
    kind,
    job_key,
    status,
    worker_id,
    ownership_token,
    started_at,
    finished_at,
    lease_until,
    retry_at,
    retry_remaining,
    last_error,
    input_watermark,
    last_success_watermark
) VALUES (?, ?, 'running', ?, ?, ?, NULL, ?, NULL, ?, NULL, ?, NULL)
                    "#,
                )
                .bind("memory_stage1")
                .bind(thread_id.to_string())
                .bind(current_thread_id.to_string())
                .bind(Uuid::new_v4().to_string())
                .bind(started_at)
                .bind(lease_until)
                .bind(3)
                .bind(metadata.updated_at.timestamp())
                .execute(runtime.pool.as_ref())
                .await
                .expect("seed running stage1 job");
            }
        }

        let allowed_sources = vec!["cli".to_string()];
        let claims = runtime
            .claim_stage1_jobs_for_startup(
                current_thread_id,
                Stage1StartupClaimParams {
                    scan_limit: 200,
                    max_claimed: 64,
                    max_age_days: 30,
                    min_rollout_idle_hours: 12,
                    allowed_sources: allowed_sources.as_slice(),
                    lease_seconds: 3600,
                },
            )
            .await
            .expect("claim stage1 jobs");
        assert_eq!(claims.len(), 54);

        let running_count = sqlx::query(
            r#"
SELECT COUNT(*) AS count
FROM jobs
WHERE kind = 'memory_stage1'
  AND status = 'running'
  AND lease_until IS NOT NULL
  AND lease_until > ?
            "#,
        )
        .bind(Utc::now().timestamp())
        .fetch_one(runtime.pool.as_ref())
        .await
        .expect("count running stage1 jobs")
        .try_get::<i64, _>("count")
        .expect("running count value");
        assert_eq!(running_count, 64);

        let more_claims = runtime
            .claim_stage1_jobs_for_startup(
                current_thread_id,
                Stage1StartupClaimParams {
                    scan_limit: 200,
                    max_claimed: 64,
                    max_age_days: 30,
                    min_rollout_idle_hours: 12,
                    allowed_sources: allowed_sources.as_slice(),
                    lease_seconds: 3600,
                },
            )
            .await
            .expect("claim stage1 jobs with cap reached");
        assert_eq!(more_claims.len(), 0);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn claim_stage1_jobs_processes_two_full_batches_across_startup_passes() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let current_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("current thread id");
        let mut current =
            test_thread_metadata(&codex_home, current_thread_id, codex_home.join("current"));
        current.created_at = Utc::now();
        current.updated_at = Utc::now();
        runtime
            .upsert_thread(&current)
            .await
            .expect("upsert current");

        let eligible_at = Utc::now() - Duration::hours(13);
        for idx in 0..200 {
            let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
            let mut metadata = test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.join(format!("thread-{idx}")),
            );
            metadata.created_at = eligible_at - Duration::seconds(idx as i64);
            metadata.updated_at = eligible_at - Duration::seconds(idx as i64);
            runtime
                .upsert_thread(&metadata)
                .await
                .expect("upsert eligible thread");
        }

        let allowed_sources = vec!["cli".to_string()];
        let first_claims = runtime
            .claim_stage1_jobs_for_startup(
                current_thread_id,
                Stage1StartupClaimParams {
                    scan_limit: 5_000,
                    max_claimed: 64,
                    max_age_days: 30,
                    min_rollout_idle_hours: 12,
                    allowed_sources: allowed_sources.as_slice(),
                    lease_seconds: 3_600,
                },
            )
            .await
            .expect("first stage1 startup claim");
        assert_eq!(first_claims.len(), 64);

        for claim in first_claims {
            assert!(
                runtime
                    .mark_stage1_job_succeeded(
                        claim.thread.id,
                        claim.ownership_token.as_str(),
                        claim.thread.updated_at.timestamp(),
                        "raw",
                        "summary",
                        /*rollout_slug*/ None,
                    )
                    .await
                    .expect("mark first-batch stage1 success"),
                "first batch stage1 completion should succeed"
            );
        }

        let second_claims = runtime
            .claim_stage1_jobs_for_startup(
                current_thread_id,
                Stage1StartupClaimParams {
                    scan_limit: 5_000,
                    max_claimed: 64,
                    max_age_days: 30,
                    min_rollout_idle_hours: 12,
                    allowed_sources: allowed_sources.as_slice(),
                    lease_seconds: 3_600,
                },
            )
            .await
            .expect("second stage1 startup claim");
        assert_eq!(second_claims.len(), 64);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn stage1_output_cascades_on_thread_delete() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let cwd = codex_home.join("workspace");
        runtime
            .upsert_thread(&test_thread_metadata(&codex_home, thread_id, cwd))
            .await
            .expect("upsert thread");

        let claim = runtime
            .try_claim_stage1_job(
                thread_id, owner, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1");
        let ownership_token = match claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_id,
                    ownership_token.as_str(),
                    /*source_updated_at*/ 100,
                    "raw",
                    "sum",
                    /*rollout_slug*/ None,
                )
                .await
                .expect("mark stage1 succeeded"),
            "mark stage1 succeeded should write stage1_outputs"
        );

        let count_before =
            sqlx::query("SELECT COUNT(*) AS count FROM stage1_outputs WHERE thread_id = ?")
                .bind(thread_id.to_string())
                .fetch_one(runtime.pool.as_ref())
                .await
                .expect("count before delete")
                .try_get::<i64, _>("count")
                .expect("count value");
        assert_eq!(count_before, 1);

        sqlx::query("DELETE FROM threads WHERE id = ?")
            .bind(thread_id.to_string())
            .execute(runtime.pool.as_ref())
            .await
            .expect("delete thread");

        let count_after =
            sqlx::query("SELECT COUNT(*) AS count FROM stage1_outputs WHERE thread_id = ?")
                .bind(thread_id.to_string())
                .fetch_one(runtime.pool.as_ref())
                .await
                .expect("count after delete")
                .try_get::<i64, _>("count")
                .expect("count value");
        assert_eq!(count_after, 0);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn mark_stage1_job_succeeded_no_output_skips_phase2_when_output_was_already_absent() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let owner_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.join("workspace"),
            ))
            .await
            .expect("upsert thread");

        let claim = runtime
            .try_claim_stage1_job(
                thread_id, owner, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1");
        let ownership_token = match claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded_no_output(thread_id, ownership_token.as_str())
                .await
                .expect("mark stage1 succeeded without output"),
            "stage1 no-output success should complete the job"
        );

        let output_row_count =
            sqlx::query("SELECT COUNT(*) AS count FROM stage1_outputs WHERE thread_id = ?")
                .bind(thread_id.to_string())
                .fetch_one(runtime.pool.as_ref())
                .await
                .expect("load stage1 output count")
                .try_get::<i64, _>("count")
                .expect("stage1 output count");
        assert_eq!(
            output_row_count, 0,
            "stage1 no-output success should not persist empty stage1 outputs"
        );

        let up_to_date = runtime
            .try_claim_stage1_job(
                thread_id, owner_b, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 up-to-date");
        assert_eq!(up_to_date, Stage1JobClaimOutcome::SkippedUpToDate);

        let global_job_row_count = sqlx::query("SELECT COUNT(*) AS count FROM jobs WHERE kind = ?")
            .bind("memory_consolidate_global")
            .fetch_one(runtime.pool.as_ref())
            .await
            .expect("load phase2 job row count")
            .try_get::<i64, _>("count")
            .expect("phase2 job row count");
        assert_eq!(
            global_job_row_count, 0,
            "no-output without an existing stage1 output should not enqueue phase2"
        );

        let claim_phase2 = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim phase2");
        assert_eq!(
            claim_phase2,
            Phase2JobClaimOutcome::SkippedNotDirty,
            "phase2 should remain clean when no-output deleted nothing"
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn mark_stage1_job_succeeded_no_output_enqueues_phase2_when_deleting_output() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let owner_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.join("workspace"),
            ))
            .await
            .expect("upsert thread");

        let first_claim = runtime
            .try_claim_stage1_job(
                thread_id, owner, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim initial stage1");
        let first_token = match first_claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected initial stage1 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_id,
                    first_token.as_str(),
                    /*source_updated_at*/ 100,
                    "raw",
                    "sum",
                    /*rollout_slug*/ None
                )
                .await
                .expect("mark initial stage1 succeeded"),
            "initial stage1 success should create stage1 output"
        );

        let phase2_claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim phase2 after initial output");
        let (phase2_token, phase2_input_watermark) = match phase2_claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => (ownership_token, input_watermark),
            other => panic!("unexpected phase2 claim after initial output: {other:?}"),
        };
        assert_eq!(phase2_input_watermark, 100);
        assert!(
            runtime
                .mark_global_phase2_job_succeeded(
                    phase2_token.as_str(),
                    phase2_input_watermark,
                    &[],
                )
                .await
                .expect("mark initial phase2 succeeded"),
            "initial phase2 success should clear global dirty state"
        );

        let no_output_claim = runtime
            .try_claim_stage1_job(
                thread_id, owner_b, /*source_updated_at*/ 101, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 for no-output delete");
        let no_output_token = match no_output_claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected no-output stage1 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded_no_output(thread_id, no_output_token.as_str())
                .await
                .expect("mark stage1 no-output after existing output"),
            "no-output should succeed when deleting an existing stage1 output"
        );

        let output_row_count =
            sqlx::query("SELECT COUNT(*) AS count FROM stage1_outputs WHERE thread_id = ?")
                .bind(thread_id.to_string())
                .fetch_one(runtime.pool.as_ref())
                .await
                .expect("load stage1 output count after delete")
                .try_get::<i64, _>("count")
                .expect("stage1 output count");
        assert_eq!(output_row_count, 0);

        let claim_phase2 = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim phase2 after no-output deletion");
        let (phase2_token, phase2_input_watermark) = match claim_phase2 {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => (ownership_token, input_watermark),
            other => panic!("unexpected phase2 claim after no-output deletion: {other:?}"),
        };
        assert_eq!(phase2_input_watermark, 101);
        assert!(
            runtime
                .mark_global_phase2_job_succeeded(
                    phase2_token.as_str(),
                    phase2_input_watermark,
                    &[],
                )
                .await
                .expect("mark phase2 succeeded after no-output delete")
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn stage1_retry_exhaustion_does_not_block_newer_watermark() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.join("workspace"),
            ))
            .await
            .expect("upsert thread");

        for attempt in 0..3 {
            let claim = runtime
                .try_claim_stage1_job(
                    thread_id, owner, /*source_updated_at*/ 100, /*lease_seconds*/ 3_600,
                    /*max_running_jobs*/ 64,
                )
                .await
                .expect("claim stage1 for retry exhaustion");
            let ownership_token = match claim {
                Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
                other => panic!(
                    "attempt {} should claim stage1 before retries are exhausted: {other:?}",
                    attempt + 1
                ),
            };
            assert!(
                runtime
                    .mark_stage1_job_failed(
                        thread_id,
                        ownership_token.as_str(),
                        "boom",
                        /*retry_delay_seconds*/ 0
                    )
                    .await
                    .expect("mark stage1 failed"),
                "attempt {} should decrement retry budget",
                attempt + 1
            );
        }

        let exhausted_claim = runtime
            .try_claim_stage1_job(
                thread_id, owner, /*source_updated_at*/ 100, /*lease_seconds*/ 3_600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 after retry exhaustion");
        assert_eq!(
            exhausted_claim,
            Stage1JobClaimOutcome::SkippedRetryExhausted
        );

        let newer_source_claim = runtime
            .try_claim_stage1_job(
                thread_id, owner, /*source_updated_at*/ 101, /*lease_seconds*/ 3_600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 with newer source watermark");
        assert!(
            matches!(newer_source_claim, Stage1JobClaimOutcome::Claimed { .. }),
            "newer source watermark should reset retry budget and be claimable"
        );

        let job_row = sqlx::query(
            "SELECT retry_remaining, input_watermark FROM jobs WHERE kind = ? AND job_key = ?",
        )
        .bind("memory_stage1")
        .bind(thread_id.to_string())
        .fetch_one(runtime.pool.as_ref())
        .await
        .expect("load stage1 job row after newer-source claim");
        assert_eq!(
            job_row
                .try_get::<i64, _>("retry_remaining")
                .expect("retry_remaining"),
            3
        );
        assert_eq!(
            job_row
                .try_get::<i64, _>("input_watermark")
                .expect("input_watermark"),
            101
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn phase2_global_consolidation_reruns_when_watermark_advances() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");

        runtime
            .enqueue_global_consolidation(/*input_watermark*/ 100)
            .await
            .expect("enqueue global consolidation");

        let claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim phase2");
        let (ownership_token, input_watermark) = match claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => (ownership_token, input_watermark),
            other => panic!("unexpected phase2 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_global_phase2_job_succeeded(ownership_token.as_str(), input_watermark, &[],)
                .await
                .expect("mark phase2 succeeded"),
            "phase2 success should finalize for current token"
        );

        let claim_up_to_date = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim phase2 up-to-date");
        assert_eq!(claim_up_to_date, Phase2JobClaimOutcome::SkippedNotDirty);

        runtime
            .enqueue_global_consolidation(/*input_watermark*/ 101)
            .await
            .expect("enqueue global consolidation again");

        let claim_rerun = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim phase2 rerun");
        assert!(
            matches!(claim_rerun, Phase2JobClaimOutcome::Claimed { .. }),
            "advanced watermark should be claimable"
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn list_stage1_outputs_for_global_returns_latest_outputs() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let thread_id_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id_a,
                codex_home.join("workspace-a"),
            ))
            .await
            .expect("upsert thread a");
        let mut metadata_b =
            test_thread_metadata(&codex_home, thread_id_b, codex_home.join("workspace-b"));
        metadata_b.git_branch = Some("feature/stage1-b".to_string());
        runtime
            .upsert_thread(&metadata_b)
            .await
            .expect("upsert thread b");

        let claim = runtime
            .try_claim_stage1_job(
                thread_id_a,
                owner,
                /*source_updated_at*/ 100,
                /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 a");
        let ownership_token = match claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage1 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_id_a,
                    ownership_token.as_str(),
                    /*source_updated_at*/ 100,
                    "raw memory a",
                    "summary a",
                    /*rollout_slug*/ None,
                )
                .await
                .expect("mark stage1 succeeded a"),
            "stage1 success should persist output a"
        );

        let claim = runtime
            .try_claim_stage1_job(
                thread_id_b,
                owner,
                /*source_updated_at*/ 101,
                /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 b");
        let ownership_token = match claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage1 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_id_b,
                    ownership_token.as_str(),
                    /*source_updated_at*/ 101,
                    "raw memory b",
                    "summary b",
                    Some("rollout-b"),
                )
                .await
                .expect("mark stage1 succeeded b"),
            "stage1 success should persist output b"
        );

        let outputs = runtime
            .list_stage1_outputs_for_global(/*n*/ 10)
            .await
            .expect("list stage1 outputs for global");
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].thread_id, thread_id_b);
        assert_eq!(outputs[0].rollout_summary, "summary b");
        assert_eq!(outputs[0].rollout_slug.as_deref(), Some("rollout-b"));
        assert_eq!(outputs[0].cwd, codex_home.join("workspace-b"));
        assert_eq!(outputs[0].git_branch.as_deref(), Some("feature/stage1-b"));
        assert_eq!(outputs[1].thread_id, thread_id_a);
        assert_eq!(outputs[1].rollout_summary, "summary a");
        assert_eq!(outputs[1].rollout_slug, None);
        assert_eq!(outputs[1].cwd, codex_home.join("workspace-a"));
        assert_eq!(outputs[1].git_branch, None);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn list_stage1_outputs_for_global_skips_empty_payloads() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id_non_empty =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let thread_id_empty =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id_non_empty,
                codex_home.join("workspace-non-empty"),
            ))
            .await
            .expect("upsert non-empty thread");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id_empty,
                codex_home.join("workspace-empty"),
            ))
            .await
            .expect("upsert empty thread");

        sqlx::query(
            r#"
INSERT INTO stage1_outputs (thread_id, source_updated_at, raw_memory, rollout_summary, generated_at)
VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(thread_id_non_empty.to_string())
        .bind(100_i64)
        .bind("raw memory")
        .bind("summary")
        .bind(100_i64)
        .execute(runtime.pool.as_ref())
        .await
        .expect("insert non-empty stage1 output");
        sqlx::query(
            r#"
INSERT INTO stage1_outputs (thread_id, source_updated_at, raw_memory, rollout_summary, generated_at)
VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(thread_id_empty.to_string())
        .bind(101_i64)
        .bind("")
        .bind("")
        .bind(101_i64)
        .execute(runtime.pool.as_ref())
        .await
        .expect("insert empty stage1 output");

        let outputs = runtime
            .list_stage1_outputs_for_global(/*n*/ 1)
            .await
            .expect("list stage1 outputs for global");
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].thread_id, thread_id_non_empty);
        assert_eq!(outputs[0].rollout_summary, "summary");
        assert_eq!(outputs[0].cwd, codex_home.join("workspace-non-empty"));

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn list_stage1_outputs_for_global_skips_polluted_threads() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id_enabled =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let thread_id_polluted =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");

        for (thread_id, workspace) in [
            (thread_id_enabled, "workspace-enabled"),
            (thread_id_polluted, "workspace-polluted"),
        ] {
            runtime
                .upsert_thread(&test_thread_metadata(
                    &codex_home,
                    thread_id,
                    codex_home.join(workspace),
                ))
                .await
                .expect("upsert thread");

            let claim = runtime
                .try_claim_stage1_job(
                    thread_id, owner, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                    /*max_running_jobs*/ 64,
                )
                .await
                .expect("claim stage1");
            let ownership_token = match claim {
                Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
                other => panic!("unexpected stage1 claim outcome: {other:?}"),
            };
            assert!(
                runtime
                    .mark_stage1_job_succeeded(
                        thread_id,
                        ownership_token.as_str(),
                        /*source_updated_at*/ 100,
                        "raw memory",
                        "summary",
                        /*rollout_slug*/ None,
                    )
                    .await
                    .expect("mark stage1 succeeded"),
                "stage1 success should persist output"
            );
        }

        runtime
            .set_thread_memory_mode(thread_id_polluted, "polluted")
            .await
            .expect("mark thread polluted");

        let outputs = runtime
            .list_stage1_outputs_for_global(/*n*/ 10)
            .await
            .expect("list stage1 outputs for global");
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].thread_id, thread_id_enabled);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn get_phase2_input_selection_reports_added_retained_and_removed_rows() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let thread_id_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let thread_id_c = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");

        for (thread_id, workspace) in [
            (thread_id_a, "workspace-a"),
            (thread_id_b, "workspace-b"),
            (thread_id_c, "workspace-c"),
        ] {
            runtime
                .upsert_thread(&test_thread_metadata(
                    &codex_home,
                    thread_id,
                    codex_home.join(workspace),
                ))
                .await
                .expect("upsert thread");
        }

        for (thread_id, updated_at, slug) in [
            (thread_id_a, 100, Some("rollout-a")),
            (thread_id_b, 101, Some("rollout-b")),
            (thread_id_c, 102, Some("rollout-c")),
        ] {
            let claim = runtime
                .try_claim_stage1_job(
                    thread_id, owner, updated_at, /*lease_seconds*/ 3600,
                    /*max_running_jobs*/ 64,
                )
                .await
                .expect("claim stage1");
            let ownership_token = match claim {
                Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
                other => panic!("unexpected stage1 claim outcome: {other:?}"),
            };
            assert!(
                runtime
                    .mark_stage1_job_succeeded(
                        thread_id,
                        ownership_token.as_str(),
                        updated_at,
                        &format!("raw-{updated_at}"),
                        &format!("summary-{updated_at}"),
                        slug,
                    )
                    .await
                    .expect("mark stage1 succeeded"),
                "stage1 success should persist output"
            );
        }

        let claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim phase2");
        let (ownership_token, input_watermark) = match claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => (ownership_token, input_watermark),
            other => panic!("unexpected phase2 claim outcome: {other:?}"),
        };
        assert_eq!(input_watermark, 102);
        let selected_outputs = runtime
            .list_stage1_outputs_for_global(/*n*/ 10)
            .await
            .expect("list stage1 outputs for global")
            .into_iter()
            .filter(|output| output.thread_id == thread_id_c || output.thread_id == thread_id_a)
            .collect::<Vec<_>>();
        assert!(
            runtime
                .mark_global_phase2_job_succeeded(
                    ownership_token.as_str(),
                    input_watermark,
                    &selected_outputs,
                )
                .await
                .expect("mark phase2 success with selection"),
            "phase2 success should persist selected rows"
        );

        let selection = runtime
            .get_phase2_input_selection(/*n*/ 2, /*max_unused_days*/ 36_500)
            .await
            .expect("load phase2 input selection");

        assert_eq!(selection.selected.len(), 2);
        assert_eq!(selection.previous_selected.len(), 2);
        assert_eq!(selection.selected[0].thread_id, thread_id_c);
        assert_eq!(
            selection.selected[0].rollout_path,
            codex_home.join(format!("rollout-{thread_id_c}.jsonl"))
        );
        assert_eq!(selection.selected[1].thread_id, thread_id_b);
        assert_eq!(selection.retained_thread_ids, vec![thread_id_c]);

        assert_eq!(selection.removed.len(), 1);
        assert_eq!(selection.removed[0].thread_id, thread_id_a);
        assert_eq!(
            selection.removed[0].rollout_slug.as_deref(),
            Some("rollout-a")
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn get_phase2_input_selection_marks_polluted_previous_selection_as_removed() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id_enabled =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let thread_id_polluted =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");

        for (thread_id, updated_at) in [(thread_id_enabled, 100), (thread_id_polluted, 101)] {
            runtime
                .upsert_thread(&test_thread_metadata(
                    &codex_home,
                    thread_id,
                    codex_home.join(thread_id.to_string()),
                ))
                .await
                .expect("upsert thread");

            let claim = runtime
                .try_claim_stage1_job(
                    thread_id, owner, updated_at, /*lease_seconds*/ 3600,
                    /*max_running_jobs*/ 64,
                )
                .await
                .expect("claim stage1");
            let ownership_token = match claim {
                Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
                other => panic!("unexpected stage1 claim outcome: {other:?}"),
            };
            assert!(
                runtime
                    .mark_stage1_job_succeeded(
                        thread_id,
                        ownership_token.as_str(),
                        updated_at,
                        &format!("raw-{updated_at}"),
                        &format!("summary-{updated_at}"),
                        /*rollout_slug*/ None,
                    )
                    .await
                    .expect("mark stage1 succeeded"),
                "stage1 success should persist output"
            );
        }

        let claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim phase2");
        let (ownership_token, input_watermark) = match claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => (ownership_token, input_watermark),
            other => panic!("unexpected phase2 claim outcome: {other:?}"),
        };
        let selected_outputs = runtime
            .list_stage1_outputs_for_global(/*n*/ 10)
            .await
            .expect("list stage1 outputs for global");
        assert!(
            runtime
                .mark_global_phase2_job_succeeded(
                    ownership_token.as_str(),
                    input_watermark,
                    &selected_outputs,
                )
                .await
                .expect("mark phase2 success"),
            "phase2 success should persist selected rows"
        );

        runtime
            .set_thread_memory_mode(thread_id_polluted, "polluted")
            .await
            .expect("mark thread polluted");

        let selection = runtime
            .get_phase2_input_selection(/*n*/ 2, /*max_unused_days*/ 36_500)
            .await
            .expect("load phase2 input selection");

        assert_eq!(selection.selected.len(), 1);
        assert_eq!(selection.selected[0].thread_id, thread_id_enabled);
        assert_eq!(selection.previous_selected.len(), 2);
        assert!(
            selection
                .previous_selected
                .iter()
                .any(|item| item.thread_id == thread_id_enabled)
        );
        assert!(
            selection
                .previous_selected
                .iter()
                .any(|item| item.thread_id == thread_id_polluted)
        );
        assert_eq!(selection.retained_thread_ids, vec![thread_id_enabled]);
        assert_eq!(selection.removed.len(), 1);
        assert_eq!(selection.removed[0].thread_id, thread_id_polluted);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn mark_thread_memory_mode_polluted_enqueues_phase2_for_selected_threads() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.join("workspace"),
            ))
            .await
            .expect("upsert thread");

        let claim = runtime
            .try_claim_stage1_job(
                thread_id, owner, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1");
        let ownership_token = match claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage1 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_id,
                    ownership_token.as_str(),
                    /*source_updated_at*/ 100,
                    "raw",
                    "summary",
                    /*rollout_slug*/ None,
                )
                .await
                .expect("mark stage1 succeeded"),
            "stage1 success should persist output"
        );

        let phase2_claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim phase2");
        let (phase2_token, input_watermark) = match phase2_claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => (ownership_token, input_watermark),
            other => panic!("unexpected phase2 claim outcome: {other:?}"),
        };
        let selected_outputs = runtime
            .list_stage1_outputs_for_global(/*n*/ 10)
            .await
            .expect("list stage1 outputs");
        assert!(
            runtime
                .mark_global_phase2_job_succeeded(
                    phase2_token.as_str(),
                    input_watermark,
                    &selected_outputs,
                )
                .await
                .expect("mark phase2 success"),
            "phase2 success should persist selected rows"
        );

        assert!(
            runtime
                .mark_thread_memory_mode_polluted(thread_id)
                .await
                .expect("mark thread polluted"),
            "thread should transition to polluted"
        );

        let next_claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim phase2 after pollution");
        assert!(matches!(next_claim, Phase2JobClaimOutcome::Claimed { .. }));

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn get_phase2_input_selection_treats_regenerated_selected_rows_as_added() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.join("workspace"),
            ))
            .await
            .expect("upsert thread");

        let first_claim = runtime
            .try_claim_stage1_job(
                thread_id, owner, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim initial stage1");
        let first_token = match first_claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage1 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_id,
                    first_token.as_str(),
                    /*source_updated_at*/ 100,
                    "raw-100",
                    "summary-100",
                    Some("rollout-100"),
                )
                .await
                .expect("mark initial stage1 success"),
            "initial stage1 success should persist output"
        );

        let phase2_claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim phase2");
        let (phase2_token, input_watermark) = match phase2_claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => (ownership_token, input_watermark),
            other => panic!("unexpected phase2 claim outcome: {other:?}"),
        };
        let selected_outputs = runtime
            .list_stage1_outputs_for_global(/*n*/ 1)
            .await
            .expect("list selected outputs");
        assert!(
            runtime
                .mark_global_phase2_job_succeeded(
                    phase2_token.as_str(),
                    input_watermark,
                    &selected_outputs,
                )
                .await
                .expect("mark phase2 success"),
            "phase2 success should persist selected rows"
        );

        let refreshed_claim = runtime
            .try_claim_stage1_job(
                thread_id, owner, /*source_updated_at*/ 101, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim refreshed stage1");
        let refreshed_token = match refreshed_claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage1 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_id,
                    refreshed_token.as_str(),
                    /*source_updated_at*/ 101,
                    "raw-101",
                    "summary-101",
                    Some("rollout-101"),
                )
                .await
                .expect("mark refreshed stage1 success"),
            "refreshed stage1 success should persist output"
        );

        let selection = runtime
            .get_phase2_input_selection(/*n*/ 1, /*max_unused_days*/ 36_500)
            .await
            .expect("load phase2 input selection");
        assert_eq!(selection.selected.len(), 1);
        assert_eq!(selection.previous_selected.len(), 1);
        assert_eq!(selection.selected[0].thread_id, thread_id);
        assert_eq!(selection.selected[0].source_updated_at.timestamp(), 101);
        assert!(selection.retained_thread_ids.is_empty());
        assert!(selection.removed.is_empty());

        let (selected_for_phase2, selected_for_phase2_source_updated_at) =
            sqlx::query_as::<_, (i64, Option<i64>)>(
                "SELECT selected_for_phase2, selected_for_phase2_source_updated_at FROM stage1_outputs WHERE thread_id = ?",
            )
        .bind(thread_id.to_string())
        .fetch_one(runtime.pool.as_ref())
        .await
        .expect("load selected_for_phase2");
        assert_eq!(selected_for_phase2, 1);
        assert_eq!(selected_for_phase2_source_updated_at, Some(100));

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn get_phase2_input_selection_reports_regenerated_previous_selection_as_removed() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread a");
        let thread_id_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread b");
        let thread_id_c = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread c");
        let thread_id_d = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread d");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");

        for (thread_id, workspace) in [
            (thread_id_a, "workspace-a"),
            (thread_id_b, "workspace-b"),
            (thread_id_c, "workspace-c"),
            (thread_id_d, "workspace-d"),
        ] {
            runtime
                .upsert_thread(&test_thread_metadata(
                    &codex_home,
                    thread_id,
                    codex_home.join(workspace),
                ))
                .await
                .expect("upsert thread");
        }

        for (thread_id, updated_at, slug) in [
            (thread_id_a, 100, Some("rollout-a-100")),
            (thread_id_b, 101, Some("rollout-b-101")),
            (thread_id_c, 99, Some("rollout-c-99")),
            (thread_id_d, 98, Some("rollout-d-98")),
        ] {
            let claim = runtime
                .try_claim_stage1_job(
                    thread_id, owner, updated_at, /*lease_seconds*/ 3600,
                    /*max_running_jobs*/ 64,
                )
                .await
                .expect("claim initial stage1");
            let ownership_token = match claim {
                Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
                other => panic!("unexpected stage1 claim outcome: {other:?}"),
            };
            assert!(
                runtime
                    .mark_stage1_job_succeeded(
                        thread_id,
                        ownership_token.as_str(),
                        updated_at,
                        &format!("raw-{updated_at}"),
                        &format!("summary-{updated_at}"),
                        slug,
                    )
                    .await
                    .expect("mark stage1 succeeded"),
                "stage1 success should persist output"
            );
        }

        let phase2_claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim phase2");
        let (phase2_token, input_watermark) = match phase2_claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => (ownership_token, input_watermark),
            other => panic!("unexpected phase2 claim outcome: {other:?}"),
        };
        let selected_outputs = runtime
            .list_stage1_outputs_for_global(/*n*/ 2)
            .await
            .expect("list selected outputs");
        assert_eq!(
            selected_outputs
                .iter()
                .map(|output| output.thread_id)
                .collect::<Vec<_>>(),
            vec![thread_id_b, thread_id_a]
        );
        assert!(
            runtime
                .mark_global_phase2_job_succeeded(
                    phase2_token.as_str(),
                    input_watermark,
                    &selected_outputs,
                )
                .await
                .expect("mark phase2 success"),
            "phase2 success should persist selected rows"
        );

        for (thread_id, updated_at, slug) in [
            (thread_id_a, 102, Some("rollout-a-102")),
            (thread_id_c, 103, Some("rollout-c-103")),
            (thread_id_d, 104, Some("rollout-d-104")),
        ] {
            let claim = runtime
                .try_claim_stage1_job(
                    thread_id, owner, updated_at, /*lease_seconds*/ 3600,
                    /*max_running_jobs*/ 64,
                )
                .await
                .expect("claim refreshed stage1");
            let ownership_token = match claim {
                Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
                other => panic!("unexpected stage1 claim outcome: {other:?}"),
            };
            assert!(
                runtime
                    .mark_stage1_job_succeeded(
                        thread_id,
                        ownership_token.as_str(),
                        updated_at,
                        &format!("raw-{updated_at}"),
                        &format!("summary-{updated_at}"),
                        slug,
                    )
                    .await
                    .expect("mark refreshed stage1 success"),
                "refreshed stage1 success should persist output"
            );
        }

        let selection = runtime
            .get_phase2_input_selection(/*n*/ 2, /*max_unused_days*/ 36_500)
            .await
            .expect("load phase2 input selection");
        assert_eq!(
            selection
                .selected
                .iter()
                .map(|output| output.thread_id)
                .collect::<Vec<_>>(),
            vec![thread_id_d, thread_id_c]
        );
        assert_eq!(
            selection
                .previous_selected
                .iter()
                .map(|output| output.thread_id)
                .collect::<Vec<_>>(),
            vec![thread_id_a, thread_id_b]
        );
        assert!(selection.retained_thread_ids.is_empty());
        assert_eq!(
            selection
                .removed
                .iter()
                .map(|output| (output.thread_id, output.source_updated_at.timestamp()))
                .collect::<Vec<_>>(),
            vec![(thread_id_a, 102), (thread_id_b, 101)]
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn mark_global_phase2_job_succeeded_updates_selected_snapshot_timestamp() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.join("workspace"),
            ))
            .await
            .expect("upsert thread");

        let initial_claim = runtime
            .try_claim_stage1_job(
                thread_id, owner, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim initial stage1");
        let initial_token = match initial_claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage1 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_id,
                    initial_token.as_str(),
                    /*source_updated_at*/ 100,
                    "raw-100",
                    "summary-100",
                    Some("rollout-100"),
                )
                .await
                .expect("mark initial stage1 success"),
            "initial stage1 success should persist output"
        );

        let first_phase2_claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim first phase2");
        let (first_phase2_token, first_input_watermark) = match first_phase2_claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => (ownership_token, input_watermark),
            other => panic!("unexpected first phase2 claim outcome: {other:?}"),
        };
        let first_selected_outputs = runtime
            .list_stage1_outputs_for_global(/*n*/ 1)
            .await
            .expect("list first selected outputs");
        assert!(
            runtime
                .mark_global_phase2_job_succeeded(
                    first_phase2_token.as_str(),
                    first_input_watermark,
                    &first_selected_outputs,
                )
                .await
                .expect("mark first phase2 success"),
            "first phase2 success should persist selected rows"
        );

        let refreshed_claim = runtime
            .try_claim_stage1_job(
                thread_id, owner, /*source_updated_at*/ 101, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim refreshed stage1");
        let refreshed_token = match refreshed_claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected refreshed stage1 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_id,
                    refreshed_token.as_str(),
                    /*source_updated_at*/ 101,
                    "raw-101",
                    "summary-101",
                    Some("rollout-101"),
                )
                .await
                .expect("mark refreshed stage1 success"),
            "refreshed stage1 success should persist output"
        );

        let second_phase2_claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim second phase2");
        let (second_phase2_token, second_input_watermark) = match second_phase2_claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => (ownership_token, input_watermark),
            other => panic!("unexpected second phase2 claim outcome: {other:?}"),
        };
        let second_selected_outputs = runtime
            .list_stage1_outputs_for_global(/*n*/ 1)
            .await
            .expect("list second selected outputs");
        assert_eq!(
            second_selected_outputs[0].source_updated_at.timestamp(),
            101
        );
        assert!(
            runtime
                .mark_global_phase2_job_succeeded(
                    second_phase2_token.as_str(),
                    second_input_watermark,
                    &second_selected_outputs,
                )
                .await
                .expect("mark second phase2 success"),
            "second phase2 success should persist selected rows"
        );

        let selection = runtime
            .get_phase2_input_selection(/*n*/ 1, /*max_unused_days*/ 36_500)
            .await
            .expect("load phase2 input selection after refresh");
        assert_eq!(selection.retained_thread_ids, vec![thread_id]);

        let (selected_for_phase2, selected_for_phase2_source_updated_at) =
            sqlx::query_as::<_, (i64, Option<i64>)>(
                "SELECT selected_for_phase2, selected_for_phase2_source_updated_at FROM stage1_outputs WHERE thread_id = ?",
            )
            .bind(thread_id.to_string())
            .fetch_one(runtime.pool.as_ref())
            .await
            .expect("load selected snapshot after phase2");
        assert_eq!(selected_for_phase2, 1);
        assert_eq!(selected_for_phase2_source_updated_at, Some(101));

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn mark_global_phase2_job_succeeded_only_marks_exact_selected_snapshots() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.join("workspace"),
            ))
            .await
            .expect("upsert thread");

        let initial_claim = runtime
            .try_claim_stage1_job(
                thread_id, owner, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim initial stage1");
        let initial_token = match initial_claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage1 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_id,
                    initial_token.as_str(),
                    /*source_updated_at*/ 100,
                    "raw-100",
                    "summary-100",
                    Some("rollout-100"),
                )
                .await
                .expect("mark initial stage1 success"),
            "initial stage1 success should persist output"
        );

        let phase2_claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim phase2");
        let (phase2_token, input_watermark) = match phase2_claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => (ownership_token, input_watermark),
            other => panic!("unexpected phase2 claim outcome: {other:?}"),
        };
        let selected_outputs = runtime
            .list_stage1_outputs_for_global(/*n*/ 1)
            .await
            .expect("list selected outputs");
        assert_eq!(selected_outputs[0].source_updated_at.timestamp(), 100);

        let refreshed_claim = runtime
            .try_claim_stage1_job(
                thread_id, owner, /*source_updated_at*/ 101, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim refreshed stage1");
        let refreshed_token = match refreshed_claim {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage1 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_id,
                    refreshed_token.as_str(),
                    /*source_updated_at*/ 101,
                    "raw-101",
                    "summary-101",
                    Some("rollout-101"),
                )
                .await
                .expect("mark refreshed stage1 success"),
            "refreshed stage1 success should persist output"
        );

        assert!(
            runtime
                .mark_global_phase2_job_succeeded(
                    phase2_token.as_str(),
                    input_watermark,
                    &selected_outputs,
                )
                .await
                .expect("mark phase2 success"),
            "phase2 success should still complete"
        );

        let (selected_for_phase2, selected_for_phase2_source_updated_at) =
            sqlx::query_as::<_, (i64, Option<i64>)>(
                "SELECT selected_for_phase2, selected_for_phase2_source_updated_at FROM stage1_outputs WHERE thread_id = ?",
            )
            .bind(thread_id.to_string())
            .fetch_one(runtime.pool.as_ref())
            .await
            .expect("load selected_for_phase2");
        assert_eq!(selected_for_phase2, 0);
        assert_eq!(selected_for_phase2_source_updated_at, None);

        let selection = runtime
            .get_phase2_input_selection(/*n*/ 1, /*max_unused_days*/ 36_500)
            .await
            .expect("load phase2 input selection");
        assert_eq!(selection.selected.len(), 1);
        assert_eq!(selection.selected[0].source_updated_at.timestamp(), 101);
        assert!(selection.retained_thread_ids.is_empty());

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn record_stage1_output_usage_updates_usage_metadata() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id a");
        let thread_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id b");
        let missing = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("missing id");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");

        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_a,
                codex_home.join("workspace-a"),
            ))
            .await
            .expect("upsert thread a");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_b,
                codex_home.join("workspace-b"),
            ))
            .await
            .expect("upsert thread b");

        let claim_a = runtime
            .try_claim_stage1_job(
                thread_a, owner, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 a");
        let token_a = match claim_a {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage1 claim outcome for a: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_a,
                    token_a.as_str(),
                    /*source_updated_at*/ 100,
                    "raw a",
                    "sum a",
                    /*rollout_slug*/ None
                )
                .await
                .expect("mark stage1 succeeded a")
        );

        let claim_b = runtime
            .try_claim_stage1_job(
                thread_b, owner, /*source_updated_at*/ 101, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 b");
        let token_b = match claim_b {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage1 claim outcome for b: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_b,
                    token_b.as_str(),
                    /*source_updated_at*/ 101,
                    "raw b",
                    "sum b",
                    /*rollout_slug*/ None
                )
                .await
                .expect("mark stage1 succeeded b")
        );

        let updated_rows = runtime
            .record_stage1_output_usage(&[thread_a, thread_a, thread_b, missing])
            .await
            .expect("record stage1 output usage");
        assert_eq!(updated_rows, 3);

        let row_a =
            sqlx::query("SELECT usage_count, last_usage FROM stage1_outputs WHERE thread_id = ?")
                .bind(thread_a.to_string())
                .fetch_one(runtime.pool.as_ref())
                .await
                .expect("load stage1 usage row a");
        let row_b =
            sqlx::query("SELECT usage_count, last_usage FROM stage1_outputs WHERE thread_id = ?")
                .bind(thread_b.to_string())
                .fetch_one(runtime.pool.as_ref())
                .await
                .expect("load stage1 usage row b");

        assert_eq!(
            row_a
                .try_get::<i64, _>("usage_count")
                .expect("usage_count a"),
            2
        );
        assert_eq!(
            row_b
                .try_get::<i64, _>("usage_count")
                .expect("usage_count b"),
            1
        );

        let last_usage_a = row_a.try_get::<i64, _>("last_usage").expect("last_usage a");
        let last_usage_b = row_b.try_get::<i64, _>("last_usage").expect("last_usage b");
        assert_eq!(last_usage_a, last_usage_b);
        assert!(last_usage_a > 0);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn record_memory_negative_feedback_suppresses_current_snapshot_from_phase2_selection() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let thread_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id a");
        let thread_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id b");

        for (thread_id, workspace) in [(thread_a, "workspace-a"), (thread_b, "workspace-b")] {
            runtime
                .upsert_thread(&test_thread_metadata(
                    &codex_home,
                    thread_id,
                    codex_home.join(workspace),
                ))
                .await
                .expect("upsert thread");
        }

        for (thread_id, source_updated_at, rollout_slug, summary) in [
            (thread_a, 100_i64, Some("a"), "summary-a"),
            (thread_b, 101_i64, Some("b"), "summary-b"),
        ] {
            let claim = runtime
                .try_claim_stage1_job(
                    thread_id,
                    owner,
                    source_updated_at,
                    /*lease_seconds*/ 3600,
                    /*max_running_jobs*/ 64,
                )
                .await
                .expect("claim stage1");
            let ownership_token = match claim {
                Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
                other => panic!("unexpected stage1 claim outcome: {other:?}"),
            };
            assert!(
                runtime
                    .mark_stage1_job_succeeded(
                        thread_id,
                        ownership_token.as_str(),
                        source_updated_at,
                        &format!("raw-{summary}"),
                        summary,
                        rollout_slug,
                    )
                    .await
                    .expect("mark stage1 success"),
                "stage1 success should persist output"
            );
        }

        let initial_selection = runtime
            .get_phase2_input_selection(/*n*/ 2, /*max_unused_days*/ 36_500)
            .await
            .expect("load initial phase2 selection");
        assert_eq!(initial_selection.selected.len(), 2);

        let phase2_claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3_600)
            .await
            .expect("claim initial phase2");
        let (phase2_token, input_watermark) = match phase2_claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => (ownership_token, input_watermark),
            other => panic!("unexpected initial phase2 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_global_phase2_job_succeeded(
                    phase2_token.as_str(),
                    input_watermark,
                    &initial_selection.selected,
                )
                .await
                .expect("mark initial phase2 success"),
            "initial phase2 success should finalize"
        );

        let feedback = runtime
            .record_memory_negative_feedback(
                &MemoryNegativeFeedbackTarget {
                    thread_id: thread_a,
                    source_updated_at: None,
                    rollout_slug: Some("a".to_string()),
                },
                MemoryNegativeFeedbackReason::WrongScope,
            )
            .await
            .expect("record memory negative feedback");
        assert_eq!(feedback.thread_id, thread_a);
        assert_eq!(feedback.source_updated_at.timestamp(), 100);
        assert_eq!(feedback.reason, MemoryNegativeFeedbackReason::WrongScope);

        let feedback_reason: String = sqlx::query_scalar(
            "SELECT reason FROM memory_negative_feedback WHERE thread_id = ? AND source_updated_at = ?",
        )
        .bind(thread_a.to_string())
        .bind(100_i64)
        .fetch_one(runtime.pool.as_ref())
        .await
        .expect("load memory negative feedback reason");
        assert_eq!(feedback_reason, "wrong_scope");

        let selection = runtime
            .get_phase2_input_selection(/*n*/ 2, /*max_unused_days*/ 36_500)
            .await
            .expect("load phase2 selection after suppression");
        assert_eq!(selection.selected.len(), 1);
        assert_eq!(selection.selected[0].thread_id, thread_b);
        assert_eq!(selection.previous_selected.len(), 2);
        assert_eq!(selection.removed.len(), 1);
        assert_eq!(selection.removed[0].thread_id, thread_a);

        let claim_after_feedback = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3_600)
            .await
            .expect("claim phase2 after feedback");
        assert!(
            matches!(claim_after_feedback, Phase2JobClaimOutcome::Claimed { .. }),
            "negative feedback should enqueue a fresh phase2 run"
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn get_phase2_input_selection_prioritizes_usage_count_then_recent_usage() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let now = Utc::now();
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let thread_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id a");
        let thread_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id b");
        let thread_c = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id c");

        for (thread_id, workspace) in [
            (thread_a, "workspace-a"),
            (thread_b, "workspace-b"),
            (thread_c, "workspace-c"),
        ] {
            runtime
                .upsert_thread(&test_thread_metadata(
                    &codex_home,
                    thread_id,
                    codex_home.join(workspace),
                ))
                .await
                .expect("upsert thread");
        }

        for (thread_id, generated_at, summary) in [
            (thread_a, now - Duration::days(3), "summary-a"),
            (thread_b, now - Duration::days(2), "summary-b"),
            (thread_c, now - Duration::days(1), "summary-c"),
        ] {
            let source_updated_at = generated_at.timestamp();
            let claim = runtime
                .try_claim_stage1_job(
                    thread_id,
                    owner,
                    source_updated_at,
                    /*lease_seconds*/ 3600,
                    /*max_running_jobs*/ 64,
                )
                .await
                .expect("claim stage1");
            let ownership_token = match claim {
                Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
                other => panic!("unexpected stage1 claim outcome: {other:?}"),
            };
            assert!(
                runtime
                    .mark_stage1_job_succeeded(
                        thread_id,
                        ownership_token.as_str(),
                        source_updated_at,
                        &format!("raw-{summary}"),
                        summary,
                        /*rollout_slug*/ None,
                    )
                    .await
                    .expect("mark stage1 success"),
                "stage1 success should persist output"
            );
        }

        for (thread_id, usage_count, last_usage) in [
            (thread_a, 5_i64, now - Duration::days(10)),
            (thread_b, 5_i64, now - Duration::days(1)),
            (thread_c, 1_i64, now - Duration::hours(1)),
        ] {
            sqlx::query(
                "UPDATE stage1_outputs SET usage_count = ?, last_usage = ? WHERE thread_id = ?",
            )
            .bind(usage_count)
            .bind(last_usage.timestamp())
            .bind(thread_id.to_string())
            .execute(runtime.pool.as_ref())
            .await
            .expect("update usage metadata");
        }

        let selection = runtime
            .get_phase2_input_selection(/*n*/ 3, /*max_unused_days*/ 30)
            .await
            .expect("load phase2 input selection");

        assert_eq!(
            selection
                .selected
                .iter()
                .map(|output| output.thread_id)
                .collect::<Vec<_>>(),
            vec![thread_b, thread_a, thread_c]
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn get_phase2_input_selection_excludes_stale_used_memories_but_keeps_fresh_never_used() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let now = Utc::now();
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let thread_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id a");
        let thread_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id b");
        let thread_c = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id c");

        for (thread_id, workspace) in [
            (thread_a, "workspace-a"),
            (thread_b, "workspace-b"),
            (thread_c, "workspace-c"),
        ] {
            runtime
                .upsert_thread(&test_thread_metadata(
                    &codex_home,
                    thread_id,
                    codex_home.join(workspace),
                ))
                .await
                .expect("upsert thread");
        }

        for (thread_id, generated_at, summary) in [
            (thread_a, now - Duration::days(40), "summary-a"),
            (thread_b, now - Duration::days(2), "summary-b"),
            (thread_c, now - Duration::days(50), "summary-c"),
        ] {
            let source_updated_at = generated_at.timestamp();
            let claim = runtime
                .try_claim_stage1_job(
                    thread_id,
                    owner,
                    source_updated_at,
                    /*lease_seconds*/ 3600,
                    /*max_running_jobs*/ 64,
                )
                .await
                .expect("claim stage1");
            let ownership_token = match claim {
                Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
                other => panic!("unexpected stage1 claim outcome: {other:?}"),
            };
            assert!(
                runtime
                    .mark_stage1_job_succeeded(
                        thread_id,
                        ownership_token.as_str(),
                        source_updated_at,
                        &format!("raw-{summary}"),
                        summary,
                        /*rollout_slug*/ None,
                    )
                    .await
                    .expect("mark stage1 success"),
                "stage1 success should persist output"
            );
        }

        for (thread_id, usage_count, last_usage) in [
            (thread_a, Some(9_i64), Some(now - Duration::days(31))),
            (thread_b, None, None),
            (thread_c, Some(1_i64), Some(now - Duration::days(1))),
        ] {
            sqlx::query(
                "UPDATE stage1_outputs SET usage_count = ?, last_usage = ? WHERE thread_id = ?",
            )
            .bind(usage_count)
            .bind(last_usage.map(|value| value.timestamp()))
            .bind(thread_id.to_string())
            .execute(runtime.pool.as_ref())
            .await
            .expect("update usage metadata");
        }

        let selection = runtime
            .get_phase2_input_selection(/*n*/ 3, /*max_unused_days*/ 30)
            .await
            .expect("load phase2 input selection");

        assert_eq!(
            selection
                .selected
                .iter()
                .map(|output| output.thread_id)
                .collect::<Vec<_>>(),
            vec![thread_c, thread_b]
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn get_phase2_input_selection_prefers_recent_thread_updates_over_recent_generation() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let older_thread =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("older thread id");
        let newer_thread =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("newer thread id");

        for (thread_id, workspace) in [
            (older_thread, "workspace-older"),
            (newer_thread, "workspace-newer"),
        ] {
            runtime
                .upsert_thread(&test_thread_metadata(
                    &codex_home,
                    thread_id,
                    codex_home.join(workspace),
                ))
                .await
                .expect("upsert thread");
        }

        for (thread_id, source_updated_at, summary) in [
            (older_thread, 100_i64, "summary-older"),
            (newer_thread, 200_i64, "summary-newer"),
        ] {
            let claim = runtime
                .try_claim_stage1_job(
                    thread_id,
                    owner,
                    source_updated_at,
                    /*lease_seconds*/ 3600,
                    /*max_running_jobs*/ 64,
                )
                .await
                .expect("claim stage1");
            let ownership_token = match claim {
                Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
                other => panic!("unexpected stage1 claim outcome: {other:?}"),
            };
            assert!(
                runtime
                    .mark_stage1_job_succeeded(
                        thread_id,
                        ownership_token.as_str(),
                        source_updated_at,
                        &format!("raw-{summary}"),
                        summary,
                        /*rollout_slug*/ None,
                    )
                    .await
                    .expect("mark stage1 success"),
                "stage1 success should persist output"
            );
        }

        sqlx::query("UPDATE stage1_outputs SET generated_at = ? WHERE thread_id = ?")
            .bind(300_i64)
            .bind(older_thread.to_string())
            .execute(runtime.pool.as_ref())
            .await
            .expect("update older generated_at");
        sqlx::query("UPDATE stage1_outputs SET generated_at = ? WHERE thread_id = ?")
            .bind(150_i64)
            .bind(newer_thread.to_string())
            .execute(runtime.pool.as_ref())
            .await
            .expect("update newer generated_at");

        let selection = runtime
            .get_phase2_input_selection(/*n*/ 1, /*max_unused_days*/ 36_500)
            .await
            .expect("load phase2 input selection");

        assert_eq!(selection.selected.len(), 1);
        assert_eq!(selection.selected[0].thread_id, newer_thread);
        assert_eq!(selection.selected[0].source_updated_at.timestamp(), 200);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn get_phase2_input_selection_prefers_high_value_session_over_newer_low_information_session()
     {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let high_value_raw_memory = high_value_raw_memory(
            "cargo test -p codex-state memory_session_value_scoring",
            "codex-rs/state/src/runtime/memories.rs",
        );
        let high_value_thread = seed_stage1_output_with_raw_memory(
            &runtime,
            codex_home.as_path(),
            owner,
            "workspace-high",
            1_710_000_100,
            high_value_raw_memory.as_str(),
            "Session value scoring preserved a deterministic recovery pattern and reusable command path guidance.",
            Some("high-value"),
        )
        .await;
        let low_value_thread = seed_stage1_output_with_raw_memory(
            &runtime,
            codex_home.as_path(),
            owner,
            "workspace-low",
            1_710_000_300,
            low_information_raw_memory(),
            "Short note.",
            Some("low-info"),
        )
        .await;

        let selection = runtime
            .get_phase2_input_selection(/*n*/ 1, /*max_unused_days*/ 36_500)
            .await
            .expect("load phase2 input selection");

        assert_eq!(selection.selected.len(), 1);
        assert_eq!(selection.selected[0].thread_id, high_value_thread);
        assert_ne!(selection.selected[0].thread_id, low_value_thread);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn get_phase2_input_selection_demotes_duplicate_session_below_unique_medium_value_session()
     {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let duplicate_raw_memory = high_value_raw_memory(
            "cargo test -p codex-state memory_session_value_scoring",
            "codex-rs/state/src/model/memories.rs",
        );
        let medium_raw_memory = medium_value_raw_memory("cargo build -p codex-cli");
        let canonical = seed_stage1_output_with_raw_memory(
            &runtime,
            codex_home.as_path(),
            owner,
            "workspace-canonical",
            1_710_000_100,
            duplicate_raw_memory.as_str(),
            "Session value scoring preserved a deterministic recovery pattern and reusable command path guidance.",
            Some("canonical"),
        )
        .await;
        let duplicate = seed_stage1_output_with_raw_memory(
            &runtime,
            codex_home.as_path(),
            owner,
            "workspace-duplicate",
            1_710_000_090,
            duplicate_raw_memory.as_str(),
            "Session value scoring preserved a deterministic recovery pattern and reusable command path guidance.",
            Some("duplicate"),
        )
        .await;
        let medium = seed_stage1_output_with_raw_memory(
            &runtime,
            codex_home.as_path(),
            owner,
            "workspace-medium",
            1_710_000_080,
            medium_raw_memory.as_str(),
            "Runtime scoring stayed heuristic but still produced a reusable command note with enough context for later consolidation.",
            Some("medium"),
        )
        .await;

        sqlx::query(
            "UPDATE stage1_outputs SET usage_count = ?, last_usage = ? WHERE thread_id = ?",
        )
        .bind(2_i64)
        .bind((Utc::now() - Duration::days(1)).timestamp())
        .bind(medium.to_string())
        .execute(runtime.pool.as_ref())
        .await
        .expect("update medium usage metadata");

        let selection = runtime
            .get_phase2_input_selection(/*n*/ 2, /*max_unused_days*/ 36_500)
            .await
            .expect("load phase2 input selection");

        assert_eq!(
            selection
                .selected
                .iter()
                .map(|output| output.thread_id)
                .collect::<Vec<_>>(),
            vec![canonical, medium]
        );
        assert!(
            !selection
                .selected
                .iter()
                .any(|output| output.thread_id == duplicate),
            "duplicate session should be demoted below the unique medium-value session"
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn prune_stage1_outputs_for_retention_prefers_pruning_low_value_before_older_high_value()
    {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let now = Utc::now();
        let prune_high_value_raw_memory = high_value_raw_memory(
            "cargo test -p codex-state prune_stage1_outputs_for_retention",
            "codex-rs/state/src/runtime/memories.rs",
        );
        let high_value_thread = seed_stage1_output_with_raw_memory(
            &runtime,
            codex_home.as_path(),
            owner,
            "workspace-high-prune",
            (now - Duration::days(60)).timestamp(),
            prune_high_value_raw_memory.as_str(),
            "Older but high-value summary with reusable command and recovery guidance.",
            Some("high-prune"),
        )
        .await;
        let low_value_thread = seed_stage1_output_with_raw_memory(
            &runtime,
            codex_home.as_path(),
            owner,
            "workspace-low-prune",
            (now - Duration::days(40)).timestamp(),
            low_information_raw_memory(),
            "Short note.",
            Some("low-prune"),
        )
        .await;

        let pruned = runtime
            .prune_stage1_outputs_for_retention(/*max_unused_days*/ 30, /*limit*/ 1)
            .await
            .expect("prune stage1 outputs with value score");
        assert_eq!(pruned, 1);

        let remaining = sqlx::query_scalar::<_, String>(
            "SELECT thread_id FROM stage1_outputs ORDER BY thread_id",
        )
        .fetch_all(runtime.pool.as_ref())
        .await
        .expect("load remaining stage1 outputs after score-based prune");
        assert_eq!(remaining, vec![high_value_thread.to_string()]);
        assert!(
            !remaining
                .iter()
                .any(|thread_id| thread_id == &low_value_thread.to_string()),
            "low-information stale session should be pruned before the older high-value session"
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn prune_stage1_outputs_for_retention_prunes_stale_unselected_rows_only() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let stale_unused =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("stale unused");
        let stale_used = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("stale used");
        let stale_selected =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("stale selected");
        let fresh_used = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("fresh used");

        for (thread_id, workspace) in [
            (stale_unused, "workspace-stale-unused"),
            (stale_used, "workspace-stale-used"),
            (stale_selected, "workspace-stale-selected"),
            (fresh_used, "workspace-fresh-used"),
        ] {
            runtime
                .upsert_thread(&test_thread_metadata(
                    &codex_home,
                    thread_id,
                    codex_home.join(workspace),
                ))
                .await
                .expect("upsert thread");
        }

        let now = Utc::now().timestamp();
        for (thread_id, source_updated_at, summary) in [
            (
                stale_unused,
                now - Duration::days(60).num_seconds(),
                "stale-unused",
            ),
            (
                stale_used,
                now - Duration::days(50).num_seconds(),
                "stale-used",
            ),
            (
                stale_selected,
                now - Duration::days(45).num_seconds(),
                "stale-selected",
            ),
            (
                fresh_used,
                now - Duration::days(10).num_seconds(),
                "fresh-used",
            ),
        ] {
            let claim = runtime
                .try_claim_stage1_job(
                    thread_id,
                    owner,
                    source_updated_at,
                    /*lease_seconds*/ 3600,
                    /*max_running_jobs*/ 64,
                )
                .await
                .expect("claim stage1");
            let ownership_token = match claim {
                Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
                other => panic!("unexpected stage1 claim outcome: {other:?}"),
            };
            assert!(
                runtime
                    .mark_stage1_job_succeeded(
                        thread_id,
                        ownership_token.as_str(),
                        source_updated_at,
                        &format!("raw-{summary}"),
                        summary,
                        /*rollout_slug*/ None,
                    )
                    .await
                    .expect("mark stage1 success"),
                "stage1 success should persist output"
            );
        }

        sqlx::query(
            "UPDATE stage1_outputs SET usage_count = ?, last_usage = ? WHERE thread_id = ?",
        )
        .bind(3_i64)
        .bind(now - Duration::days(40).num_seconds())
        .bind(stale_used.to_string())
        .execute(runtime.pool.as_ref())
        .await
        .expect("set stale used metadata");
        sqlx::query(
            "UPDATE stage1_outputs SET selected_for_phase2 = 1, selected_for_phase2_source_updated_at = source_updated_at WHERE thread_id = ?",
        )
        .bind(stale_selected.to_string())
        .execute(runtime.pool.as_ref())
        .await
        .expect("mark selected for phase2");
        sqlx::query(
            "UPDATE stage1_outputs SET usage_count = ?, last_usage = ? WHERE thread_id = ?",
        )
        .bind(8_i64)
        .bind(now - Duration::days(2).num_seconds())
        .bind(fresh_used.to_string())
        .execute(runtime.pool.as_ref())
        .await
        .expect("set fresh used metadata");

        let before_jobs_count =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'memory_stage1'")
                .fetch_one(runtime.pool.as_ref())
                .await
                .expect("count stage1 jobs before prune");

        let pruned = runtime
            .prune_stage1_outputs_for_retention(/*max_unused_days*/ 30, /*limit*/ 100)
            .await
            .expect("prune stage1 outputs");
        assert_eq!(pruned, 2);

        let remaining = sqlx::query_scalar::<_, String>(
            "SELECT thread_id FROM stage1_outputs ORDER BY thread_id",
        )
        .fetch_all(runtime.pool.as_ref())
        .await
        .expect("load remaining stage1 outputs");
        let mut expected_remaining = vec![fresh_used.to_string(), stale_selected.to_string()];
        expected_remaining.sort();
        assert_eq!(remaining, expected_remaining);

        let after_jobs_count =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'memory_stage1'")
                .fetch_one(runtime.pool.as_ref())
                .await
                .expect("count stage1 jobs after prune");
        assert_eq!(after_jobs_count, before_jobs_count);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn prune_stage1_outputs_for_retention_respects_batch_limit() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let thread_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread a");
        let thread_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread b");
        let thread_c = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread c");

        for (thread_id, workspace) in [
            (thread_a, "workspace-a"),
            (thread_b, "workspace-b"),
            (thread_c, "workspace-c"),
        ] {
            runtime
                .upsert_thread(&test_thread_metadata(
                    &codex_home,
                    thread_id,
                    codex_home.join(workspace),
                ))
                .await
                .expect("upsert thread");
        }

        let now = Utc::now().timestamp();
        for (thread_id, source_updated_at, summary) in [
            (thread_a, now - Duration::days(60).num_seconds(), "stale-a"),
            (thread_b, now - Duration::days(50).num_seconds(), "stale-b"),
            (thread_c, now - Duration::days(40).num_seconds(), "stale-c"),
        ] {
            let claim = runtime
                .try_claim_stage1_job(
                    thread_id,
                    owner,
                    source_updated_at,
                    /*lease_seconds*/ 3600,
                    /*max_running_jobs*/ 64,
                )
                .await
                .expect("claim stage1");
            let ownership_token = match claim {
                Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
                other => panic!("unexpected stage1 claim outcome: {other:?}"),
            };
            assert!(
                runtime
                    .mark_stage1_job_succeeded(
                        thread_id,
                        ownership_token.as_str(),
                        source_updated_at,
                        &format!("raw-{summary}"),
                        summary,
                        /*rollout_slug*/ None,
                    )
                    .await
                    .expect("mark stage1 success"),
                "stage1 success should persist output"
            );
        }

        let pruned = runtime
            .prune_stage1_outputs_for_retention(/*max_unused_days*/ 30, /*limit*/ 2)
            .await
            .expect("prune stage1 outputs with limit");
        assert_eq!(pruned, 2);

        let remaining_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stage1_outputs")
            .fetch_one(runtime.pool.as_ref())
            .await
            .expect("count remaining stage1 outputs");
        assert_eq!(remaining_count, 1);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn mark_stage1_job_succeeded_enqueues_global_consolidation() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let thread_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id a");
        let thread_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id b");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");

        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_a,
                codex_home.join("workspace-a"),
            ))
            .await
            .expect("upsert thread a");
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_b,
                codex_home.join("workspace-b"),
            ))
            .await
            .expect("upsert thread b");

        let claim_a = runtime
            .try_claim_stage1_job(
                thread_a, owner, /*source_updated_at*/ 100, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 a");
        let token_a = match claim_a {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage1 claim outcome for thread a: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_a,
                    token_a.as_str(),
                    /*source_updated_at*/ 100,
                    "raw-a",
                    "summary-a",
                    /*rollout_slug*/ None,
                )
                .await
                .expect("mark stage1 succeeded a"),
            "stage1 success should persist output for thread a"
        );

        let claim_b = runtime
            .try_claim_stage1_job(
                thread_b, owner, /*source_updated_at*/ 101, /*lease_seconds*/ 3600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 b");
        let token_b = match claim_b {
            Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage1 claim outcome for thread b: {other:?}"),
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_b,
                    token_b.as_str(),
                    /*source_updated_at*/ 101,
                    "raw-b",
                    "summary-b",
                    /*rollout_slug*/ None,
                )
                .await
                .expect("mark stage1 succeeded b"),
            "stage1 success should persist output for thread b"
        );

        let claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3600)
            .await
            .expect("claim global consolidation");
        let input_watermark = match claim {
            Phase2JobClaimOutcome::Claimed {
                input_watermark, ..
            } => input_watermark,
            other => panic!("unexpected global consolidation claim outcome: {other:?}"),
        };
        assert_eq!(input_watermark, 101);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn phase2_global_lock_allows_only_one_fresh_runner() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        runtime
            .enqueue_global_consolidation(/*input_watermark*/ 200)
            .await
            .expect("enqueue global consolidation");

        let owner_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner a");
        let owner_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner b");

        let running_claim = runtime
            .try_claim_global_phase2_job(owner_a, /*lease_seconds*/ 3600)
            .await
            .expect("claim global lock");
        assert!(
            matches!(running_claim, Phase2JobClaimOutcome::Claimed { .. }),
            "first owner should claim global lock"
        );

        let second_claim = runtime
            .try_claim_global_phase2_job(owner_b, /*lease_seconds*/ 3600)
            .await
            .expect("claim global lock from second owner");
        assert_eq!(second_claim, Phase2JobClaimOutcome::SkippedRunning);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn phase2_global_lock_stale_lease_allows_takeover() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        runtime
            .enqueue_global_consolidation(/*input_watermark*/ 300)
            .await
            .expect("enqueue global consolidation");

        let owner_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner a");
        let owner_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner b");

        let initial_claim = runtime
            .try_claim_global_phase2_job(owner_a, /*lease_seconds*/ 3600)
            .await
            .expect("claim initial global lock");
        let token_a = match initial_claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token, ..
            } => ownership_token,
            other => panic!("unexpected initial claim outcome: {other:?}"),
        };

        sqlx::query("UPDATE jobs SET lease_until = ? WHERE kind = ? AND job_key = ?")
            .bind(Utc::now().timestamp() - 1)
            .bind("memory_consolidate_global")
            .bind("global")
            .execute(runtime.pool.as_ref())
            .await
            .expect("expire global consolidation lease");

        let takeover_claim = runtime
            .try_claim_global_phase2_job(owner_b, /*lease_seconds*/ 3600)
            .await
            .expect("claim stale global lock");
        let (token_b, input_watermark) = match takeover_claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => (ownership_token, input_watermark),
            other => panic!("unexpected takeover claim outcome: {other:?}"),
        };
        assert_ne!(token_a, token_b);
        assert_eq!(input_watermark, 300);

        assert_eq!(
            runtime
                .mark_global_phase2_job_succeeded(
                    token_a.as_str(),
                    /*completed_watermark*/ 300,
                    &[]
                )
                .await
                .expect("mark stale owner success result"),
            false,
            "stale owner should lose finalization ownership after takeover"
        );
        assert!(
            runtime
                .mark_global_phase2_job_succeeded(
                    token_b.as_str(),
                    /*completed_watermark*/ 300,
                    &[]
                )
                .await
                .expect("mark takeover owner success"),
            "takeover owner should finalize consolidation"
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn phase2_backfilled_inputs_below_last_success_still_become_dirty() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        runtime
            .enqueue_global_consolidation(/*input_watermark*/ 500)
            .await
            .expect("enqueue initial consolidation");
        let owner_a = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner a");
        let claim_a = runtime
            .try_claim_global_phase2_job(owner_a, /*lease_seconds*/ 3_600)
            .await
            .expect("claim initial consolidation");
        let token_a = match claim_a {
            Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => {
                assert_eq!(input_watermark, 500);
                ownership_token
            }
            other => panic!("unexpected initial phase2 claim outcome: {other:?}"),
        };
        assert!(
            runtime
                .mark_global_phase2_job_succeeded(
                    token_a.as_str(),
                    /*completed_watermark*/ 500,
                    &[]
                )
                .await
                .expect("mark initial phase2 success"),
            "initial phase2 success should finalize"
        );

        runtime
            .enqueue_global_consolidation(/*input_watermark*/ 400)
            .await
            .expect("enqueue backfilled consolidation");

        let owner_b = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner b");
        let claim_b = runtime
            .try_claim_global_phase2_job(owner_b, /*lease_seconds*/ 3_600)
            .await
            .expect("claim backfilled consolidation");
        match claim_b {
            Phase2JobClaimOutcome::Claimed {
                input_watermark, ..
            } => {
                assert!(
                    input_watermark > 500,
                    "backfilled enqueue should advance dirty watermark beyond last success"
                );
            }
            other => panic!("unexpected backfilled phase2 claim outcome: {other:?}"),
        }

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn phase2_failure_fallback_updates_unowned_running_job() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        runtime
            .enqueue_global_consolidation(/*input_watermark*/ 400)
            .await
            .expect("enqueue global consolidation");

        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner");
        let claim = runtime
            .try_claim_global_phase2_job(owner, /*lease_seconds*/ 3_600)
            .await
            .expect("claim global consolidation");
        let ownership_token = match claim {
            Phase2JobClaimOutcome::Claimed {
                ownership_token, ..
            } => ownership_token,
            other => panic!("unexpected claim outcome: {other:?}"),
        };

        sqlx::query("UPDATE jobs SET ownership_token = NULL WHERE kind = ? AND job_key = ?")
            .bind("memory_consolidate_global")
            .bind("global")
            .execute(runtime.pool.as_ref())
            .await
            .expect("clear ownership token");

        assert_eq!(
            runtime
                .mark_global_phase2_job_failed(
                    ownership_token.as_str(),
                    "lost",
                    /*retry_delay_seconds*/ 3_600
                )
                .await
                .expect("mark phase2 failed with strict ownership"),
            false,
            "strict failure update should not match unowned running job"
        );
        assert!(
            runtime
                .mark_global_phase2_job_failed_if_unowned(
                    ownership_token.as_str(),
                    "lost",
                    /*retry_delay_seconds*/ 3_600
                )
                .await
                .expect("fallback failure update should match unowned running job"),
            "fallback failure update should transition the unowned running job"
        );

        let claim = runtime
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 3_600)
            .await
            .expect("claim after fallback failure");
        assert_eq!(claim, Phase2JobClaimOutcome::SkippedNotDirty);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    async fn seed_stage1_output(
        runtime: &Arc<StateRuntime>,
        codex_home: &Path,
        owner: ThreadId,
        workspace: &str,
        source_updated_at: i64,
        rollout_summary: &str,
        rollout_slug: Option<&str>,
    ) -> ThreadId {
        seed_stage1_output_with_raw_memory(
            runtime,
            codex_home,
            owner,
            workspace,
            source_updated_at,
            "raw memory body",
            rollout_summary,
            rollout_slug,
        )
        .await
    }

    async fn seed_stage1_output_with_raw_memory(
        runtime: &Arc<StateRuntime>,
        codex_home: &Path,
        owner: ThreadId,
        workspace: &str,
        source_updated_at: i64,
        raw_memory: &str,
        rollout_summary: &str,
        rollout_slug: Option<&str>,
    ) -> ThreadId {
        let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("thread id");
        let mut metadata = test_thread_metadata(codex_home, thread_id, codex_home.join(workspace));
        metadata.updated_at = Utc
            .timestamp_opt(source_updated_at + 60, 0)
            .single()
            .expect("updated_at timestamp");
        runtime
            .upsert_thread(&metadata)
            .await
            .expect("upsert thread");

        let claim = runtime
            .try_claim_stage1_job(
                thread_id,
                owner,
                source_updated_at,
                /*lease_seconds*/ 3_600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 job");
        let Stage1JobClaimOutcome::Claimed { ownership_token } = claim else {
            panic!("unexpected stage1 claim outcome: {claim:?}");
        };
        assert!(
            runtime
                .mark_stage1_job_succeeded(
                    thread_id,
                    ownership_token.as_str(),
                    source_updated_at,
                    raw_memory,
                    rollout_summary,
                    rollout_slug,
                )
                .await
                .expect("mark stage1 success"),
            "stage1 success should persist output"
        );
        thread_id
    }

    fn high_value_raw_memory(command: &str, path: &str) -> String {
        format!(
            "\
---
description: High-value memory scoring signal
task: high-value-memory
task_group: memory
keywords: memory, scoring, preference, command, recovery
---

### Task 1: Build session value scoring
task: build-session-value-scoring
task_group: memory
task_outcome: success

Preference signals:
- prefer deterministic, explainable scoring over opaque heuristics
- keep schema changes minimal when the same signal can be derived from current state

Decision signals:
- make the score influence phase2 selection order directly instead of storing it as debug metadata only
- use the same score to bias retention decisions so low-value sessions are pruned first

Scope and cwd notes:
- primary cwd is `C:\\CodexSource\\codex`; keep the score scoped to the current Codex checkout

Reusable knowledge:
- parse the structured Phase 1 sections to recover durable preference, decision, and command signals
- keep duplicate sessions from crowding out unique sessions during consolidation

Failures and how to do differently:
- when low-information sessions are newer than richer sessions, do not let recency alone dominate selection
- if a session is duplicated, preserve one canonical copy and penalize the rest instead of promoting every clone

High-value commands or paths:
- `{command}`
- `{path}`
"
        )
    }

    fn medium_value_raw_memory(command: &str) -> String {
        format!(
            "\
---
description: Medium-value memory scoring signal
task: medium-value-memory
task_group: memory
keywords: memory, scoring, command
---

### Task 1: Keep a reusable command note
task: keep-reusable-command-note
task_group: memory
task_outcome: success

Preference signals:
- prefer small, targeted runtime changes before broader memory UI work

Decision signals:
- keep the first session-value scoring pass heuristic and deterministic

Scope and cwd notes:
- this applies to the local memory pipeline workstream

Reusable knowledge:
- one reusable runtime hook is enough when it changes selection behavior deterministically

Failures and how to do differently:
- avoid coupling scoring to dashboards in the first pass

High-value commands or paths:
- `{command}`
"
        )
    }

    fn low_information_raw_memory() -> &'static str {
        "\
---
description: Low information memory
task: low-information-memory
task_group: memory
keywords: memory
---

### Task 1: Keep going
task: keep-going
task_group: memory
task_outcome: partial

Preference signals:

Decision signals:

Scope and cwd notes:

Reusable knowledge:

Failures and how to do differently:

High-value commands or paths:
"
    }

    #[tokio::test]
    async fn list_memory_peek_entries_returns_recent_outputs_with_summary_excerpt() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");

        let older_thread = seed_stage1_output(
            &runtime,
            codex_home.as_path(),
            owner,
            "workspace-a",
            1_710_000_000,
            "Older summary",
            Some("older"),
        )
        .await;
        let newer_summary = "Newer summary with enough words to confirm the excerpt path is using the rollout summary instead of the raw memory payload.";
        let newer_thread = seed_stage1_output(
            &runtime,
            codex_home.as_path(),
            owner,
            "workspace-b",
            1_710_000_100,
            newer_summary,
            Some("newer"),
        )
        .await;

        let entries = runtime
            .list_memory_peek_entries(/*limit*/ 2)
            .await
            .expect("load memory peek entries");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].thread_id, newer_thread);
        assert_eq!(entries[1].thread_id, older_thread);
        assert_eq!(
            entries[0].rollout_summary_file,
            format!(
                "rollout_summaries/{}",
                rollout_summary_file_name_from_parts(
                    newer_thread,
                    Utc.timestamp_opt(1_710_000_100, 0)
                        .single()
                        .expect("source timestamp"),
                    Some("newer"),
                )
            )
        );
        assert!(
            entries[0].summary_excerpt.contains("Newer summary"),
            "expected rollout summary excerpt, got {:?}",
            entries[0]
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn inspect_memory_health_reports_ok_for_consistent_layout() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let thread_id = seed_stage1_output(
            &runtime,
            codex_home.as_path(),
            owner,
            "workspace-healthy",
            1_710_100_000,
            "Healthy rollout summary",
            Some("healthy"),
        )
        .await;

        let memory_root = codex_home.join("memories");
        let rollout_dir = memory_root.join("rollout_summaries");
        tokio::fs::create_dir_all(&rollout_dir)
            .await
            .expect("create rollout summaries dir");
        tokio::fs::write(memory_root.join("MEMORY.md"), "# Memory\n\nHealthy\n")
            .await
            .expect("write MEMORY.md");
        tokio::fs::write(
            memory_root.join("memory_summary.md"),
            "Healthy memory summary\n",
        )
        .await
        .expect("write memory_summary.md");
        tokio::fs::write(
            memory_root.join("raw_memories.md"),
            "# Raw Memories\n\nHealthy raw memory\n",
        )
        .await
        .expect("write raw_memories.md");
        tokio::fs::write(
            rollout_dir.join(rollout_summary_file_name_from_parts(
                thread_id,
                Utc.timestamp_opt(1_710_100_000, 0)
                    .single()
                    .expect("source timestamp"),
                Some("healthy"),
            )),
            "thread_id: healthy\n\nHealthy rollout summary\n",
        )
        .await
        .expect("write rollout summary");

        let report = runtime
            .inspect_memory_health(
                /*max_raw_memories_for_consolidation*/ 10, /*max_unused_days*/ 36_500,
            )
            .await
            .expect("inspect memory health");

        assert!(report.ok, "expected healthy report, got {report:?}");
        assert!(
            report.issues.is_empty(),
            "expected no issues, got {report:?}"
        );
        assert_eq!(report.rollout_summaries.expected.len(), 1);
        assert!(report.rollout_summaries.expected[0].exists);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn inspect_memory_health_reports_missing_and_stale_sources() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");
        let owner = ThreadId::from_string(&Uuid::new_v4().to_string()).expect("owner id");
        let _thread_id = seed_stage1_output(
            &runtime,
            codex_home.as_path(),
            owner,
            "workspace-broken",
            1_710_200_000,
            "Broken rollout summary",
            Some("broken"),
        )
        .await;

        let rollout_dir = codex_home.join("memories").join("rollout_summaries");
        tokio::fs::create_dir_all(&rollout_dir)
            .await
            .expect("create rollout summaries dir");
        tokio::fs::write(rollout_dir.join("stale.md"), "stale summary\n")
            .await
            .expect("write stale rollout summary");
        tokio::fs::write(rollout_dir.join("orphan.txt"), "not a markdown source\n")
            .await
            .expect("write malformed retrieval source");

        let report = runtime
            .inspect_memory_health(
                /*max_raw_memories_for_consolidation*/ 10, /*max_unused_days*/ 36_500,
            )
            .await
            .expect("inspect broken memory health");

        assert!(!report.ok, "expected unhealthy report, got {report:?}");
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "missing_memory"),
            "expected missing MEMORY.md issue, got {report:?}"
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "missing_memory_summary"),
            "expected missing memory_summary.md issue, got {report:?}"
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "missing_rollout_summary"),
            "expected missing rollout summary issue, got {report:?}"
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "stale_rollout_summary"),
            "expected stale rollout summary issue, got {report:?}"
        );
        assert!(
            report.issues.iter().any(|issue| {
                issue.code == "malformed_retrieval_source"
                    && issue.severity == MemoryHealthSeverity::Warning
            }),
            "expected malformed retrieval source warning, got {report:?}"
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }
}
