use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use std::path::PathBuf;
use uuid::Uuid;

use super::ThreadMetadata;

/// Stored stage-1 memory extraction output for a single thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage1Output {
    pub thread_id: ThreadId,
    pub rollout_path: PathBuf,
    pub source_updated_at: DateTime<Utc>,
    pub raw_memory: String,
    pub rollout_summary: String,
    pub rollout_slug: Option<String>,
    pub cwd: PathBuf,
    pub git_branch: Option<String>,
    pub generated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage1OutputRef {
    pub thread_id: ThreadId,
    pub source_updated_at: DateTime<Utc>,
    pub rollout_slug: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Phase2InputSelection {
    pub selected: Vec<Stage1Output>,
    pub previous_selected: Vec<Stage1Output>,
    pub retained_thread_ids: Vec<ThreadId>,
    pub removed: Vec<Stage1OutputRef>,
}

/// Deterministic heuristic score used to decide whether a session / rollout is
/// worth retaining, consolidating, or pruning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionValueScore {
    pub total: i64,
    pub novelty: i64,
    pub preference_decision_density: i64,
    pub reusable_command_path_signal: i64,
    pub failure_recovery_signal: i64,
    pub completion_citation_signal: i64,
    pub negative_feedback_penalty: i64,
    pub duplication_penalty: i64,
    pub low_information_penalty: i64,
    pub suppressed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPeekEntry {
    pub thread_id: ThreadId,
    pub thread_updated_at: DateTime<Utc>,
    pub source_updated_at: DateTime<Utc>,
    pub generated_at: DateTime<Utc>,
    pub cwd: PathBuf,
    pub rollout_summary_file: String,
    pub summary_excerpt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDirectoryHealth {
    pub path: PathBuf,
    pub exists: bool,
    pub is_directory: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryHealthArtifact {
    pub kind: String,
    pub path: PathBuf,
    pub exists: bool,
    pub is_file: bool,
    pub readable: bool,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryHealthRolloutSummary {
    pub file_name: String,
    pub path: PathBuf,
    pub exists: bool,
    pub is_file: bool,
    pub readable: bool,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryHealthSeverity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryHealthIssue {
    pub severity: MemoryHealthSeverity,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryHealthRolloutSummarySet {
    pub directory: MemoryDirectoryHealth,
    pub expected: Vec<MemoryHealthRolloutSummary>,
    pub stale: Vec<MemoryHealthRolloutSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryHealthReport {
    pub ok: bool,
    pub memory_root: MemoryDirectoryHealth,
    pub artifacts: Vec<MemoryHealthArtifact>,
    pub rollout_summaries: MemoryHealthRolloutSummarySet,
    pub issues: Vec<MemoryHealthIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryNegativeFeedbackTarget {
    pub thread_id: ThreadId,
    pub source_updated_at: Option<DateTime<Utc>>,
    pub rollout_slug: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryNegativeFeedbackReason {
    Transient,
    IncorrectInference,
    WrongScope,
    PrivacySensitive,
    Duplicate,
}

impl MemoryNegativeFeedbackReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::IncorrectInference => "incorrect_inference",
            Self::WrongScope => "wrong_scope",
            Self::PrivacySensitive => "privacy_sensitive",
            Self::Duplicate => "duplicate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryNegativeFeedbackRecord {
    pub thread_id: ThreadId,
    pub source_updated_at: DateTime<Utc>,
    pub rollout_slug: Option<String>,
    pub reason: MemoryNegativeFeedbackReason,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRetrievalRecord {
    pub thread_id: ThreadId,
    pub source_updated_at: DateTime<Utc>,
    pub rollout_slug: Option<String>,
    pub cwd: PathBuf,
    pub rollout_path: PathBuf,
    pub usage_count: i64,
    pub last_usage: Option<DateTime<Utc>>,
    pub negative_feedback_reason: Option<MemoryNegativeFeedbackReason>,
}

#[derive(Debug)]
pub(crate) struct Stage1OutputRow {
    thread_id: String,
    rollout_path: String,
    source_updated_at: i64,
    raw_memory: String,
    rollout_summary: String,
    rollout_slug: Option<String>,
    cwd: String,
    git_branch: Option<String>,
    generated_at: i64,
}

impl Stage1OutputRow {
    pub(crate) fn try_from_row(row: &SqliteRow) -> Result<Self> {
        Ok(Self {
            thread_id: row.try_get("thread_id")?,
            rollout_path: row.try_get("rollout_path")?,
            source_updated_at: row.try_get("source_updated_at")?,
            raw_memory: row.try_get("raw_memory")?,
            rollout_summary: row.try_get("rollout_summary")?,
            rollout_slug: row.try_get("rollout_slug")?,
            cwd: row.try_get("cwd")?,
            git_branch: row.try_get("git_branch")?,
            generated_at: row.try_get("generated_at")?,
        })
    }
}

impl TryFrom<Stage1OutputRow> for Stage1Output {
    type Error = anyhow::Error;

    fn try_from(row: Stage1OutputRow) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            thread_id: ThreadId::try_from(row.thread_id)?,
            rollout_path: PathBuf::from(row.rollout_path),
            source_updated_at: epoch_seconds_to_datetime(row.source_updated_at)?,
            raw_memory: row.raw_memory,
            rollout_summary: row.rollout_summary,
            rollout_slug: row.rollout_slug,
            cwd: PathBuf::from(row.cwd),
            git_branch: row.git_branch,
            generated_at: epoch_seconds_to_datetime(row.generated_at)?,
        })
    }
}

fn epoch_seconds_to_datetime(secs: i64) -> Result<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(secs, 0)
        .ok_or_else(|| anyhow::anyhow!("invalid unix timestamp: {secs}"))
}

pub(crate) fn stage1_output_ref_from_parts(
    thread_id: String,
    source_updated_at: i64,
    rollout_slug: Option<String>,
) -> Result<Stage1OutputRef> {
    Ok(Stage1OutputRef {
        thread_id: ThreadId::try_from(thread_id)?,
        source_updated_at: epoch_seconds_to_datetime(source_updated_at)?,
        rollout_slug,
    })
}

pub(crate) fn memory_negative_feedback_reason_from_str(
    value: &str,
) -> Result<MemoryNegativeFeedbackReason> {
    match value {
        "transient" => Ok(MemoryNegativeFeedbackReason::Transient),
        "incorrect_inference" => Ok(MemoryNegativeFeedbackReason::IncorrectInference),
        "wrong_scope" => Ok(MemoryNegativeFeedbackReason::WrongScope),
        "privacy_sensitive" => Ok(MemoryNegativeFeedbackReason::PrivacySensitive),
        "duplicate" => Ok(MemoryNegativeFeedbackReason::Duplicate),
        _ => anyhow::bail!("unknown memory negative feedback reason: {value}"),
    }
}

pub(crate) fn rollout_summary_file_name_from_parts(
    thread_id: ThreadId,
    source_updated_at: DateTime<Utc>,
    rollout_slug: Option<&str>,
) -> String {
    format!(
        "{}.md",
        rollout_summary_file_stem_from_parts(thread_id, source_updated_at, rollout_slug)
    )
}

fn rollout_summary_file_stem_from_parts(
    thread_id: ThreadId,
    source_updated_at: DateTime<Utc>,
    rollout_slug: Option<&str>,
) -> String {
    const ROLLOUT_SLUG_MAX_LEN: usize = 60;
    const SHORT_HASH_ALPHABET: &[u8; 62] =
        b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const SHORT_HASH_SPACE: u32 = 14_776_336;

    let thread_id = thread_id.to_string();
    let (timestamp_fragment, short_hash_seed) = match Uuid::parse_str(&thread_id) {
        Ok(thread_uuid) => {
            let timestamp = thread_uuid
                .get_timestamp()
                .and_then(|uuid_timestamp| {
                    let (seconds, nanos) = uuid_timestamp.to_unix();
                    i64::try_from(seconds)
                        .ok()
                        .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, nanos))
                })
                .unwrap_or(source_updated_at);
            let short_hash_seed = (thread_uuid.as_u128() & 0xFFFF_FFFF) as u32;
            (
                timestamp.format("%Y-%m-%dT%H-%M-%S").to_string(),
                short_hash_seed,
            )
        }
        Err(_) => {
            let mut short_hash_seed = 0u32;
            for byte in thread_id.bytes() {
                short_hash_seed = short_hash_seed
                    .wrapping_mul(31)
                    .wrapping_add(u32::from(byte));
            }
            (
                source_updated_at.format("%Y-%m-%dT%H-%M-%S").to_string(),
                short_hash_seed,
            )
        }
    };

    let mut short_hash_value = short_hash_seed % SHORT_HASH_SPACE;
    let mut short_hash_chars = ['0'; 4];
    for idx in (0..short_hash_chars.len()).rev() {
        let alphabet_idx = (short_hash_value % SHORT_HASH_ALPHABET.len() as u32) as usize;
        short_hash_chars[idx] = SHORT_HASH_ALPHABET[alphabet_idx] as char;
        short_hash_value /= SHORT_HASH_ALPHABET.len() as u32;
    }
    let short_hash: String = short_hash_chars.iter().collect();
    let file_prefix = format!("{timestamp_fragment}-{short_hash}");

    let Some(raw_slug) = rollout_slug else {
        return file_prefix;
    };

    let mut slug = String::with_capacity(ROLLOUT_SLUG_MAX_LEN);
    for ch in raw_slug.chars() {
        if slug.len() >= ROLLOUT_SLUG_MAX_LEN {
            break;
        }

        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else {
            slug.push('_');
        }
    }

    while slug.ends_with('_') {
        slug.pop();
    }

    if slug.is_empty() {
        file_prefix
    } else {
        format!("{file_prefix}-{slug}")
    }
}

/// Result of trying to claim a stage-1 memory extraction job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage1JobClaimOutcome {
    /// The caller owns the job and should continue with extraction.
    Claimed { ownership_token: String },
    /// Existing output is already newer than or equal to the source rollout.
    SkippedUpToDate,
    /// Another worker currently owns a fresh lease for this job.
    SkippedRunning,
    /// The job is in backoff and should not be retried yet.
    SkippedRetryBackoff,
    /// The job has exhausted retries and should not be retried automatically.
    SkippedRetryExhausted,
}

/// Claimed stage-1 job with thread metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage1JobClaim {
    pub thread: ThreadMetadata,
    pub ownership_token: String,
}

#[derive(Debug, Clone, Copy)]
pub struct Stage1StartupClaimParams<'a> {
    pub scan_limit: usize,
    pub max_claimed: usize,
    pub max_age_days: i64,
    pub min_rollout_idle_hours: i64,
    pub allowed_sources: &'a [String],
    pub lease_seconds: i64,
}

/// Result of trying to claim a phase-2 consolidation job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase2JobClaimOutcome {
    /// The caller owns the global lock and should spawn consolidation.
    Claimed {
        ownership_token: String,
        /// Snapshot of `input_watermark` at claim time.
        input_watermark: i64,
    },
    /// The global job is not pending consolidation (or is already up to date).
    SkippedNotDirty,
    /// Another worker currently owns a fresh global consolidation lease.
    SkippedRunning,
}
