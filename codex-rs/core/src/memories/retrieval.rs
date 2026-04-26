use crate::memories::memory_root;
use crate::memories::rollout_summaries_dir;
use crate::memories::storage::rollout_summary_file_stem_from_parts;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::user_input::UserInput;
use codex_state::MemoryNegativeFeedbackReason;
use codex_state::MemoryRetrievalRecord;
use codex_state::StateRuntime;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
use tokio::fs;
use tracing::debug;

const MAX_KEYWORDS: usize = 16;
const MAX_MEMORY_RESULTS: usize = 3;
const MAX_ROLLOUT_RESULTS: usize = 2;
const MAX_SUMMARY_RESULTS: usize = 1;
const MEMORY_EXCERPT_TOKEN_LIMIT: usize = 160;
const ROLLOUT_EXCERPT_TOKEN_LIMIT: usize = 140;
const SUMMARY_EXCERPT_TOKEN_LIMIT: usize = 180;
const MIN_RELEVANCE_SCORE: i32 = 8;
const TASK_CUES: &[&str] = &["fix", "review", "refactor", "install", "memory", "config"];
const STOPWORDS: &[&str] = &[
    "about", "after", "again", "also", "and", "before", "being", "build", "from", "have", "into",
    "just", "local", "need", "only", "please", "that", "them", "then", "this", "using", "with",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RetrievalSourceKind {
    MemorySummary,
    Memory,
    RolloutSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetrievalMemoryClass {
    Durable,
    EpisodicEvidence,
}

impl RetrievalMemoryClass {
    fn label(self) -> &'static str {
        match self {
            Self::Durable => "durable_memory",
            Self::EpisodicEvidence => "episodic_evidence",
        }
    }
}

impl RetrievalSourceKind {
    fn label(self) -> &'static str {
        match self {
            Self::MemorySummary => "memory_summary",
            Self::Memory => "durable_memory",
            Self::RolloutSummary => "rollout_summary",
        }
    }

    fn memory_class(self) -> RetrievalMemoryClass {
        match self {
            Self::MemorySummary | Self::Memory => RetrievalMemoryClass::Durable,
            Self::RolloutSummary => RetrievalMemoryClass::EpisodicEvidence,
        }
    }

    fn excerpt_token_limit(self) -> usize {
        match self {
            Self::MemorySummary => SUMMARY_EXCERPT_TOKEN_LIMIT,
            Self::Memory => MEMORY_EXCERPT_TOKEN_LIMIT,
            Self::RolloutSummary => ROLLOUT_EXCERPT_TOKEN_LIMIT,
        }
    }

    fn result_cap(self) -> usize {
        match self {
            Self::MemorySummary => MAX_SUMMARY_RESULTS,
            Self::Memory => MAX_MEMORY_RESULTS,
            Self::RolloutSummary => MAX_ROLLOUT_RESULTS,
        }
    }

    fn source_bias(self) -> i32 {
        match self {
            Self::MemorySummary => 4,
            Self::Memory => 3,
            Self::RolloutSummary => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetrievedMemorySnippet {
    pub(crate) source_path: String,
    pub(crate) source_kind: RetrievalSourceKind,
    pub(crate) score: i32,
    pub(crate) excerpt: String,
    pub(crate) line_range: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
struct RetrievalQuery {
    keywords: Vec<String>,
    task_cues: Vec<&'static str>,
    path_mentions: Vec<String>,
    cwd_terms: Vec<String>,
}

#[derive(Debug, Clone)]
struct RetrievalCandidate {
    source_path: String,
    source_kind: RetrievalSourceKind,
    line_range: Option<(usize, usize)>,
    text: String,
    updated_at: Option<DateTime<Utc>>,
    rollout_state: Option<RolloutRetrievalState>,
}

#[derive(Debug, Clone, Copy, Default)]
struct CandidateSignals {
    score: i32,
    keyword_hits: usize,
    filename_hits: usize,
    path_hits: usize,
    cwd_hits: usize,
    task_cue_hits: usize,
    usage_boost: i32,
    recent_usage_boost: i32,
    stale_penalty: i32,
    suppression_penalty: i32,
}

impl CandidateSignals {
    fn total_hits(self) -> usize {
        self.keyword_hits + self.filename_hits + self.path_hits + self.cwd_hits + self.task_cue_hits
    }

    fn is_episodic_priority_match(self) -> bool {
        self.path_hits > 0
            || self.cwd_hits > 0
            || self.keyword_hits >= 2
            || self.total_hits() >= 3
            || (self.usage_boost >= 6 && self.keyword_hits > 0)
    }
}

#[derive(Debug, Default)]
struct SourceSelectionCounts {
    summary: usize,
    memory: usize,
    rollout: usize,
}

#[derive(Debug, Clone)]
struct RolloutRetrievalState {
    cwd: String,
    rollout_path: String,
    source_updated_at: DateTime<Utc>,
    usage_count: i64,
    last_usage: Option<DateTime<Utc>>,
    negative_feedback_reason: Option<MemoryNegativeFeedbackReason>,
}

#[derive(Debug, Clone, Default)]
struct RetrievalStateIndex {
    rollout_by_path: HashMap<String, RolloutRetrievalState>,
    state_loaded: bool,
}

pub(crate) async fn retrieve_memory_snippets(
    state_db: Option<&StateRuntime>,
    codex_home: &AbsolutePathBuf,
    cwd: &Path,
    input: &[UserInput],
) -> std::io::Result<Vec<RetrievedMemorySnippet>> {
    let query = build_query(cwd, input);
    if query.keywords.is_empty() && query.path_mentions.is_empty() {
        debug!(
            cwd = %cwd.display(),
            "memory retrieval skipped because the turn did not produce any retrieval query terms"
        );
        return Ok(Vec::new());
    }

    let retrieval_state = match load_retrieval_state_index(state_db).await {
        Ok(index) => index,
        Err(err) => {
            debug!("memory retrieval ranking state unavailable: {err}");
            RetrievalStateIndex::default()
        }
    };

    let root = memory_root(codex_home);
    let mut candidates = Vec::new();

    if let Some(candidate) = read_candidate_file(
        root.join("memory_summary.md"),
        "memory_summary.md",
        RetrievalSourceKind::MemorySummary,
        CandidateSplitStrategy::WholeFile,
        None,
    )
    .await?
    {
        candidates.extend(candidate);
    }

    if let Some(candidate) = read_candidate_file(
        root.join("MEMORY.md"),
        "MEMORY.md",
        RetrievalSourceKind::Memory,
        CandidateSplitStrategy::MarkdownSections,
        None,
    )
    .await?
    {
        candidates.extend(candidate);
    }

    let rollout_dir = rollout_summaries_dir(&root);
    let mut read_dir = match fs::read_dir(&rollout_dir).await {
        Ok(dir) => dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(select_snippets(
                candidates,
                &query,
                retrieval_state.state_loaded,
                Utc::now(),
            ));
        }
        Err(err) => return Err(err),
    };

    while let Some(entry) = read_dir.next_entry().await? {
        let path = entry.path();
        if !entry.file_type().await?.is_file() {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }

        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let relative_path = format!("rollout_summaries/{file_name}");
        if let Some(candidate) = read_candidate_file(
            path,
            &relative_path,
            RetrievalSourceKind::RolloutSummary,
            CandidateSplitStrategy::WholeFile,
            retrieval_state.rollout_by_path.get(&relative_path).cloned(),
        )
        .await?
        {
            candidates.extend(candidate);
        }
    }

    let snippets = select_snippets(candidates, &query, retrieval_state.state_loaded, Utc::now());
    debug!(
        cwd = %cwd.display(),
        keywords = ?query.keywords,
        path_mentions = ?query.path_mentions,
        selected_sources = ?snippets
            .iter()
            .map(|snippet| snippet.source_path.as_str())
            .collect::<Vec<_>>(),
        "memory retrieval selected snippets for the current turn"
    );
    Ok(snippets)
}

pub(crate) fn render_memory_retrieval_bundle(
    snippets: &[RetrievedMemorySnippet],
) -> Option<String> {
    if snippets.is_empty() {
        return None;
    }

    let mut rendered = String::from(
        "<memory_retrieval_bundle>\n\
Pre-turn retrieval is split into durable memory and episodic evidence.\n\
- Ranking is deterministic and currently combines query/path/cwd/task matches with source bias, \
usage/recency metadata when available, and stale or suppressed-memory penalties.\n\
- Durable memory (`MEMORY.md`, `memory_summary.md`) is the default reusable operating baseline.\n\
- Episodic evidence (`rollout_summaries/*.md`) is prior-run detail. Use it only when it closely \
matches the current task, cwd, or cited paths, and do not treat it as automatically promoted \
durable memory.\n",
    );

    let durable = snippets
        .iter()
        .filter(|snippet| snippet.source_kind.memory_class() == RetrievalMemoryClass::Durable)
        .collect::<Vec<_>>();
    if !durable.is_empty() {
        rendered.push_str(
            "\n<durable_memory>\n\
Use these first. They should shape default behavior when they match the current task.\n",
        );
        render_retrieval_section(&mut rendered, &durable);
        rendered.push_str("</durable_memory>\n");
    }

    let episodic = snippets
        .iter()
        .filter(|snippet| {
            snippet.source_kind.memory_class() == RetrievalMemoryClass::EpisodicEvidence
        })
        .collect::<Vec<_>>();
    if !episodic.is_empty() {
        rendered.push_str(
            "\n<episodic_evidence>\n\
These are scoped prior-run artifacts. Use them as supporting evidence for closely matching \
task/cwd/path situations, not as broad durable defaults.\n",
        );
        render_retrieval_section(&mut rendered, &episodic);
        rendered.push_str("</episodic_evidence>\n");
    }

    rendered.push_str("</memory_retrieval_bundle>");
    Some(rendered)
}

fn render_retrieval_section(rendered: &mut String, snippets: &[&RetrievedMemorySnippet]) {
    for snippet in snippets {
        let _ = writeln!(rendered);
        let _ = writeln!(
            rendered,
            "- source: {} ({}, {})",
            snippet.source_path,
            snippet.source_kind.label(),
            snippet.source_kind.memory_class().label()
        );
        let _ = writeln!(rendered, "  score: {}", snippet.score);
        if let Some((start, end)) = snippet.line_range {
            let _ = writeln!(rendered, "  lines: {start}-{end}");
        }
        let _ = writeln!(rendered, "  excerpt:");
        let _ = writeln!(rendered, "  ```text");
        for line in snippet.excerpt.lines() {
            let _ = writeln!(rendered, "  {line}");
        }
        let _ = writeln!(rendered, "  ```");
    }
}

#[derive(Debug, Clone, Copy)]
enum CandidateSplitStrategy {
    WholeFile,
    MarkdownSections,
}

async fn load_retrieval_state_index(
    state_db: Option<&StateRuntime>,
) -> anyhow::Result<RetrievalStateIndex> {
    let Some(state_db) = state_db else {
        return Ok(RetrievalStateIndex::default());
    };

    let records = state_db.list_memory_retrieval_records().await?;
    Ok(retrieval_state_index_from_records(records))
}

fn retrieval_state_index_from_records(records: Vec<MemoryRetrievalRecord>) -> RetrievalStateIndex {
    let mut rollout_by_path = HashMap::with_capacity(records.len());
    for record in records {
        let source_path = format!(
            "rollout_summaries/{}.md",
            rollout_summary_file_stem_from_parts(
                record.thread_id,
                record.source_updated_at,
                record.rollout_slug.as_deref(),
            )
        );
        rollout_by_path.insert(
            source_path,
            RolloutRetrievalState {
                cwd: record.cwd.to_string_lossy().into_owned(),
                rollout_path: record.rollout_path.to_string_lossy().into_owned(),
                source_updated_at: record.source_updated_at,
                usage_count: record.usage_count,
                last_usage: record.last_usage,
                negative_feedback_reason: record.negative_feedback_reason,
            },
        );
    }

    RetrievalStateIndex {
        rollout_by_path,
        state_loaded: true,
    }
}

async fn read_candidate_file(
    path: impl AsRef<Path>,
    relative_path: &str,
    source_kind: RetrievalSourceKind,
    split_strategy: CandidateSplitStrategy,
    rollout_state: Option<RolloutRetrievalState>,
) -> std::io::Result<Option<Vec<RetrievalCandidate>>> {
    let path = path.as_ref();
    let updated_at = read_file_updated_at(path).await?;
    let content = match fs::read_to_string(path).await {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };

    let content = content.trim();
    if content.is_empty() {
        return Ok(None);
    }

    let candidates = match split_strategy {
        CandidateSplitStrategy::WholeFile => vec![RetrievalCandidate {
            source_path: relative_path.to_string(),
            source_kind,
            line_range: Some((1, content.lines().count().max(1))),
            text: content.to_string(),
            updated_at,
            rollout_state,
        }],
        CandidateSplitStrategy::MarkdownSections => markdown_section_candidates(
            relative_path,
            source_kind,
            content,
            updated_at,
            rollout_state,
        ),
    };

    Ok((!candidates.is_empty()).then_some(candidates))
}

async fn read_file_updated_at(path: &Path) -> std::io::Result<Option<DateTime<Utc>>> {
    let metadata = match fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    Ok(metadata.modified().ok().map(DateTime::<Utc>::from))
}

fn markdown_section_candidates(
    relative_path: &str,
    source_kind: RetrievalSourceKind,
    content: &str,
    updated_at: Option<DateTime<Utc>>,
    rollout_state: Option<RolloutRetrievalState>,
) -> Vec<RetrievalCandidate> {
    let lines = content.lines().collect::<Vec<_>>();
    let heading_indices = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.trim_start().starts_with('#').then_some(index))
        .collect::<Vec<_>>();

    if heading_indices.len() <= 1 {
        return vec![RetrievalCandidate {
            source_path: relative_path.to_string(),
            source_kind,
            line_range: Some((1, lines.len().max(1))),
            text: content.to_string(),
            updated_at,
            rollout_state,
        }];
    }

    let mut candidates = Vec::new();
    for (position, start_idx) in heading_indices.iter().copied().enumerate() {
        let end_idx = heading_indices
            .get(position + 1)
            .copied()
            .unwrap_or(lines.len());
        let chunk = lines[start_idx..end_idx].join("\n").trim().to_string();
        if chunk.is_empty() {
            continue;
        }
        candidates.push(RetrievalCandidate {
            source_path: relative_path.to_string(),
            source_kind,
            line_range: Some((start_idx + 1, end_idx.max(start_idx + 1))),
            text: chunk,
            updated_at,
            rollout_state: rollout_state.clone(),
        });
    }
    candidates
}

fn select_snippets(
    candidates: Vec<RetrievalCandidate>,
    query: &RetrievalQuery,
    retrieval_state_loaded: bool,
    now: DateTime<Utc>,
) -> Vec<RetrievedMemorySnippet> {
    let mut durable_ranked = Vec::new();
    let mut episodic_ranked = Vec::new();

    for candidate in candidates {
        let signals = score_candidate(&candidate, query, retrieval_state_loaded, now);
        if signals.score < MIN_RELEVANCE_SCORE {
            continue;
        }

        let snippet = RetrievedMemorySnippet {
            source_path: candidate.source_path,
            source_kind: candidate.source_kind,
            score: signals.score,
            excerpt: truncate_text(
                candidate.text.trim(),
                TruncationPolicy::Tokens(candidate.source_kind.excerpt_token_limit()),
            ),
            line_range: candidate.line_range,
        };

        match candidate.source_kind.memory_class() {
            RetrievalMemoryClass::Durable => durable_ranked.push((snippet, signals)),
            RetrievalMemoryClass::EpisodicEvidence if signals.is_episodic_priority_match() => {
                episodic_ranked.push((snippet, signals));
            }
            RetrievalMemoryClass::EpisodicEvidence => {}
        }
    }

    durable_ranked.sort_by(|left, right| {
        right
            .0
            .score
            .cmp(&left.0.score)
            .then_with(|| right.1.path_hits.cmp(&left.1.path_hits))
            .then_with(|| right.1.cwd_hits.cmp(&left.1.cwd_hits))
            .then_with(|| right.1.keyword_hits.cmp(&left.1.keyword_hits))
            .then_with(|| right.1.task_cue_hits.cmp(&left.1.task_cue_hits))
            .then_with(|| right.1.recent_usage_boost.cmp(&left.1.recent_usage_boost))
            .then_with(|| right.1.usage_boost.cmp(&left.1.usage_boost))
            .then_with(|| left.1.stale_penalty.cmp(&right.1.stale_penalty))
            .then_with(|| left.1.suppression_penalty.cmp(&right.1.suppression_penalty))
            .then_with(|| left.0.source_kind.cmp(&right.0.source_kind))
            .then_with(|| left.0.source_path.cmp(&right.0.source_path))
            .then_with(|| left.0.line_range.cmp(&right.0.line_range))
    });
    episodic_ranked.sort_by(|left, right| {
        right
            .1
            .path_hits
            .cmp(&left.1.path_hits)
            .then_with(|| right.1.cwd_hits.cmp(&left.1.cwd_hits))
            .then_with(|| right.1.keyword_hits.cmp(&left.1.keyword_hits))
            .then_with(|| right.1.filename_hits.cmp(&left.1.filename_hits))
            .then_with(|| right.1.task_cue_hits.cmp(&left.1.task_cue_hits))
            .then_with(|| right.1.recent_usage_boost.cmp(&left.1.recent_usage_boost))
            .then_with(|| right.1.usage_boost.cmp(&left.1.usage_boost))
            .then_with(|| left.1.stale_penalty.cmp(&right.1.stale_penalty))
            .then_with(|| left.1.suppression_penalty.cmp(&right.1.suppression_penalty))
            .then_with(|| right.0.score.cmp(&left.0.score))
            .then_with(|| left.0.source_path.cmp(&right.0.source_path))
            .then_with(|| left.0.line_range.cmp(&right.0.line_range))
    });

    let mut selected = Vec::new();
    let mut counts = SourceSelectionCounts::default();
    append_with_source_caps(
        &mut selected,
        &mut counts,
        durable_ranked.into_iter().map(|(snippet, _)| snippet),
    );
    append_with_source_caps(
        &mut selected,
        &mut counts,
        episodic_ranked.into_iter().map(|(snippet, _)| snippet),
    );

    selected
}

fn append_with_source_caps(
    selected: &mut Vec<RetrievedMemorySnippet>,
    counts: &mut SourceSelectionCounts,
    snippets: impl IntoIterator<Item = RetrievedMemorySnippet>,
) {
    for snippet in snippets {
        let current_count = match snippet.source_kind {
            RetrievalSourceKind::MemorySummary => &mut counts.summary,
            RetrievalSourceKind::Memory => &mut counts.memory,
            RetrievalSourceKind::RolloutSummary => &mut counts.rollout,
        };
        if *current_count >= snippet.source_kind.result_cap() {
            continue;
        }
        *current_count += 1;
        selected.push(snippet);
    }
}

fn score_candidate(
    candidate: &RetrievalCandidate,
    query: &RetrievalQuery,
    retrieval_state_loaded: bool,
    now: DateTime<Utc>,
) -> CandidateSignals {
    let normalized_text = normalize_for_match(&candidate.text);
    let normalized_path = normalize_for_match(&candidate.source_path);
    let mut signals = CandidateSignals {
        score: candidate.source_kind.source_bias(),
        ..Default::default()
    };

    for keyword in &query.keywords {
        if normalized_text.contains(keyword) {
            signals.keyword_hits += 1;
            signals.score += if keyword.len() >= 6 { 7 } else { 5 };
        }
        if normalized_path.contains(keyword) {
            signals.filename_hits += 1;
            signals.score += 10;
        }
    }

    for cue in &query.task_cues {
        if normalized_text.contains(cue) {
            signals.task_cue_hits += 1;
            signals.score += 2;
        }
    }

    for path_mention in &query.path_mentions {
        if normalized_path.contains(path_mention) {
            signals.path_hits += 1;
            signals.score += 18;
        }

        let basename = basename_from_pathish(path_mention);
        if !basename.is_empty() && normalized_text.contains(&basename) {
            signals.path_hits += 1;
            signals.score += 10;
        }
    }

    for cwd_term in &query.cwd_terms {
        if normalized_text.contains(cwd_term) {
            signals.cwd_hits += 1;
            signals.score += 7;
        }
        if normalized_path.contains(cwd_term) {
            signals.cwd_hits += 1;
            signals.score += 9;
        }
    }

    if let Some(rollout_state) = candidate.rollout_state.as_ref() {
        let normalized_cwd = normalize_for_match(&rollout_state.cwd);
        let normalized_rollout_path = normalize_for_match(&rollout_state.rollout_path);
        for path_mention in &query.path_mentions {
            if normalized_rollout_path.contains(path_mention)
                || normalized_cwd.contains(path_mention)
            {
                signals.path_hits += 1;
                signals.score += 16;
            }

            let basename = basename_from_pathish(path_mention);
            if !basename.is_empty()
                && (normalized_rollout_path.contains(&basename)
                    || normalized_cwd.contains(&basename))
            {
                signals.path_hits += 1;
                signals.score += 8;
            }
        }

        for cwd_term in &query.cwd_terms {
            if normalized_cwd.contains(cwd_term) {
                signals.cwd_hits += 1;
                signals.score += 10;
            }
            if normalized_rollout_path.contains(cwd_term) {
                signals.cwd_hits += 1;
                signals.score += 8;
            }
        }

        signals.usage_boost = rollout_state.usage_count.max(0).min(4) as i32 * 3;
        signals.score += signals.usage_boost;

        if let Some(last_usage) = rollout_state.last_usage {
            signals.recent_usage_boost = recent_usage_boost(candidate.source_kind, now, last_usage);
            signals.score += signals.recent_usage_boost;
        }

        if let Some(reason) = rollout_state.negative_feedback_reason {
            signals.suppression_penalty = negative_feedback_penalty(reason);
            signals.score -= signals.suppression_penalty;
        }
    } else if retrieval_state_loaded && candidate.source_kind == RetrievalSourceKind::RolloutSummary
    {
        signals.stale_penalty += 18;
        signals.score -= 18;
    }

    if signals.total_hits() == 0 {
        return CandidateSignals::default();
    }

    signals.stale_penalty += stale_penalty(candidate, now);
    signals.score -= signals.stale_penalty;

    if candidate.source_kind == RetrievalSourceKind::RolloutSummary && signals.total_hits() == 1 {
        signals.stale_penalty += 4;
        signals.score -= 4;
    }

    signals
}

fn recent_usage_boost(
    source_kind: RetrievalSourceKind,
    now: DateTime<Utc>,
    last_usage: DateTime<Utc>,
) -> i32 {
    let age_days = now.signed_duration_since(last_usage).num_days().max(0);
    match source_kind {
        RetrievalSourceKind::MemorySummary => match age_days {
            0..=7 => 8,
            8..=30 => 5,
            31..=90 => 2,
            _ => 0,
        },
        RetrievalSourceKind::Memory => match age_days {
            0..=7 => 7,
            8..=30 => 4,
            31..=90 => 2,
            _ => 0,
        },
        RetrievalSourceKind::RolloutSummary => match age_days {
            0..=7 => 6,
            8..=30 => 4,
            31..=60 => 1,
            _ => 0,
        },
    }
}

fn stale_penalty(candidate: &RetrievalCandidate, now: DateTime<Utc>) -> i32 {
    let updated_at = candidate
        .rollout_state
        .as_ref()
        .map(|state| state.source_updated_at)
        .or(candidate.updated_at);
    let Some(updated_at) = updated_at else {
        return 0;
    };

    let age_days = now.signed_duration_since(updated_at).num_days().max(0);
    match candidate.source_kind {
        RetrievalSourceKind::MemorySummary => match age_days {
            0..=60 => 0,
            61..=180 => 4,
            181..=365 => 9,
            _ => 14,
        },
        RetrievalSourceKind::Memory => match age_days {
            0..=90 => 0,
            91..=240 => 4,
            241..=365 => 8,
            _ => 12,
        },
        RetrievalSourceKind::RolloutSummary => match age_days {
            0..=14 => 0,
            15..=45 => 4,
            46..=120 => 10,
            _ => 16,
        },
    }
}

fn negative_feedback_penalty(reason: MemoryNegativeFeedbackReason) -> i32 {
    match reason {
        MemoryNegativeFeedbackReason::Transient => 12,
        MemoryNegativeFeedbackReason::IncorrectInference => 24,
        MemoryNegativeFeedbackReason::WrongScope => 18,
        MemoryNegativeFeedbackReason::PrivacySensitive => 28,
        MemoryNegativeFeedbackReason::Duplicate => 10,
    }
}

fn build_query(cwd: &Path, input: &[UserInput]) -> RetrievalQuery {
    let user_text = input
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let normalized_text = normalize_for_match(&user_text);
    let task_cues = TASK_CUES
        .iter()
        .copied()
        .filter(|cue| normalized_text.contains(cue))
        .collect::<Vec<_>>();

    let mut path_mentions = extract_path_mentions(&user_text);
    for path_mention in extract_structured_path_mentions(input) {
        if !path_mentions.contains(&path_mention) {
            path_mentions.push(path_mention);
        }
    }
    let mut keywords = extract_keywords(&user_text);
    for path in &path_mentions {
        for component in path
            .split(['/', '\\'])
            .flat_map(|part| part.split('.'))
            .map(normalize_for_match)
        {
            if should_keep_keyword(&component) && !keywords.contains(&component) {
                keywords.push(component);
            }
        }
    }

    keywords.truncate(MAX_KEYWORDS);

    RetrievalQuery {
        keywords,
        task_cues,
        path_mentions,
        cwd_terms: collect_cwd_terms(cwd),
    }
}

fn collect_cwd_terms(cwd: &Path) -> Vec<String> {
    let mut terms = Vec::new();
    let components = cwd
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(normalize_for_match)
        .filter(|part| should_keep_keyword(part))
        .collect::<Vec<_>>();

    if let Some(last) = components.last()
        && !terms.contains(last)
    {
        terms.push(last.clone());
    }
    if components.len() >= 2 {
        let second_last = &components[components.len() - 2];
        if !terms.contains(second_last) {
            terms.push(second_last.clone());
        }
    }

    terms
}

fn extract_keywords(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut keywords = Vec::new();

    for token in text
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .map(normalize_for_match)
    {
        if !should_keep_keyword(&token) || !seen.insert(token.clone()) {
            continue;
        }
        keywords.push(token);
    }

    keywords
}

fn extract_path_mentions(text: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    let mut seen = HashSet::new();

    for token in text.split_whitespace() {
        let normalized = normalize_pathish(token);
        if normalized.is_empty()
            || !looks_like_file_path(&normalized)
            || !seen.insert(normalized.clone())
        {
            continue;
        }
        mentions.push(normalized);
    }

    mentions
}

fn extract_structured_path_mentions(input: &[UserInput]) -> Vec<String> {
    let mut mentions = Vec::new();
    let mut seen = HashSet::new();

    for item in input {
        let candidate = match item {
            UserInput::LocalImage { path } | UserInput::Skill { path, .. } => {
                Some(path.to_string_lossy().into_owned())
            }
            UserInput::Mention { path, .. } => Some(path.clone()),
            _ => None,
        };

        let Some(candidate) = candidate else {
            continue;
        };
        let normalized = normalize_pathish(&candidate);
        if normalized.is_empty()
            || !looks_like_file_path(&normalized)
            || !seen.insert(normalized.clone())
        {
            continue;
        }
        mentions.push(normalized);
    }

    mentions
}

fn should_keep_keyword(token: &str) -> bool {
    token.len() >= 3 && !token.chars().all(|ch| ch.is_ascii_digit()) && !STOPWORDS.contains(&token)
}

fn looks_like_file_path(token: &str) -> bool {
    !token.contains("://")
        && (token.contains('/') || token.contains('\\') || has_known_path_extension(token))
}

fn has_known_path_extension(token: &str) -> bool {
    token
        .rsplit_once('.')
        .map(|(_, ext)| {
            matches!(
                ext,
                "c" | "cc"
                    | "cpp"
                    | "cs"
                    | "go"
                    | "h"
                    | "hpp"
                    | "java"
                    | "js"
                    | "json"
                    | "md"
                    | "py"
                    | "rb"
                    | "rs"
                    | "sh"
                    | "toml"
                    | "ts"
                    | "tsx"
                    | "txt"
                    | "yaml"
                    | "yml"
            )
        })
        .unwrap_or(false)
}

fn basename_from_pathish(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .map(normalize_for_match)
        .unwrap_or_default()
}

fn normalize_pathish(token: &str) -> String {
    token
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ':' | ';'
            )
        })
        .trim_end_matches('.')
        .replace('\\', "/")
        .to_lowercase()
}

fn normalize_for_match(text: impl AsRef<str>) -> String {
    text.as_ref().replace('\\', "/").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use chrono::Duration;
    use chrono::TimeZone;
    use chrono::Utc;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::SessionSource;
    use codex_state::MemoryNegativeFeedbackReason;
    use codex_state::MemoryNegativeFeedbackTarget;
    use codex_state::ThreadMetadataBuilder;
    use core_test_support::PathExt;
    use pretty_assertions::assert_eq;
    use std::fs::File;
    use std::fs::FileTimes;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::fs as tokio_fs;

    fn text_input(text: &str) -> Vec<UserInput> {
        vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }]
    }

    async fn init_state_db(codex_home: &AbsolutePathBuf) -> Arc<StateRuntime> {
        StateRuntime::init(codex_home.to_path_buf(), "test-provider".to_string())
            .await
            .expect("initialize state db")
    }

    async fn seed_rollout_memory(
        state_db: &StateRuntime,
        codex_home: &AbsolutePathBuf,
        workspace: &str,
        source_updated_at: DateTime<Utc>,
        rollout_summary: &str,
        rollout_slug: &str,
    ) -> ThreadId {
        let thread_id = ThreadId::new();
        let mut metadata_builder = ThreadMetadataBuilder::new(
            thread_id,
            codex_home
                .join(format!("rollout-{rollout_slug}.jsonl"))
                .to_path_buf(),
            source_updated_at,
            SessionSource::Cli,
        );
        metadata_builder.cwd = codex_home.join(workspace).to_path_buf();
        metadata_builder.model_provider = Some("test-provider".to_string());
        let metadata = metadata_builder.build("test-provider");
        state_db
            .upsert_thread(&metadata)
            .await
            .expect("upsert thread metadata");

        let claim = state_db
            .try_claim_stage1_job(
                thread_id,
                ThreadId::new(),
                source_updated_at.timestamp(),
                /*lease_seconds*/ 3_600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage1 job");
        let ownership_token = match claim {
            codex_state::Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage1 claim outcome: {other:?}"),
        };

        assert!(
            state_db
                .mark_stage1_job_succeeded(
                    thread_id,
                    ownership_token.as_str(),
                    source_updated_at.timestamp(),
                    "raw memory",
                    rollout_summary,
                    Some(rollout_slug),
                )
                .await
                .expect("mark stage1 success"),
            "stage1 success should persist output"
        );

        let rollout_dir = codex_home.join("memories").join("rollout_summaries");
        tokio_fs::create_dir_all(&rollout_dir)
            .await
            .expect("create rollout summaries dir");
        let rollout_file = rollout_dir.join(format!(
            "{}.md",
            rollout_summary_file_stem_from_parts(thread_id, source_updated_at, Some(rollout_slug))
        ));
        tokio_fs::write(
            rollout_file,
            format!(
                "thread_id: {thread_id}\ncwd: {}\nrollout_path: {}\n\n{rollout_summary}\n",
                metadata.cwd.display(),
                metadata.rollout_path.display(),
            ),
        )
        .await
        .expect("write rollout summary");

        thread_id
    }

    fn set_modified_time(path: &Path, modified_at: DateTime<Utc>) {
        let file = File::options()
            .write(true)
            .open(path)
            .expect("open file for time update");
        let times = FileTimes::new().set_modified(modified_at.into());
        file.set_times(times).expect("set file times");
    }

    #[tokio::test]
    async fn retrieve_memory_snippets_prefers_keyword_hit_in_memory_md() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().abs();
        let memories_dir = codex_home.join("memories");
        tokio_fs::create_dir_all(memories_dir.join("rollout_summaries"))
            .await
            .unwrap();
        tokio_fs::write(
            memories_dir.join("MEMORY.md"),
            "# Build habits\n\n## Tests\nUse cargo nextest on Windows for focused Rust verification.\n",
        )
        .await
        .unwrap();

        let snippets = retrieve_memory_snippets(
            None,
            &codex_home,
            temp.path(),
            &text_input("run nextest for this Rust fix"),
        )
        .await
        .unwrap();

        assert!(
            snippets
                .iter()
                .any(|snippet| snippet.source_path == "MEMORY.md"
                    && snippet.excerpt.contains("cargo nextest")),
            "expected a MEMORY.md snippet, got {snippets:?}"
        );
    }

    #[tokio::test]
    async fn retrieve_memory_snippets_applies_cwd_specific_boost() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().abs();
        let memories_dir = codex_home.join("memories");
        let rollout_dir = memories_dir.join("rollout_summaries");
        let cwd = temp.path().join("project-alpha");

        tokio_fs::create_dir_all(&rollout_dir).await.unwrap();
        tokio_fs::create_dir_all(&cwd).await.unwrap();
        tokio_fs::write(
            rollout_dir.join("alpha.md"),
            "# Alpha\ncwd: /workspace/project-alpha\nInvestigated config parsing failure.\n",
        )
        .await
        .unwrap();
        tokio_fs::write(
            rollout_dir.join("beta.md"),
            "# Beta\ncwd: /workspace/project-beta\nInvestigated config parsing failure.\n",
        )
        .await
        .unwrap();

        let snippets = retrieve_memory_snippets(
            None,
            &codex_home,
            &cwd,
            &text_input("fix the config parser"),
        )
        .await
        .unwrap();

        assert_eq!(
            snippets.first().map(|snippet| snippet.source_path.as_str()),
            Some("rollout_summaries/alpha.md")
        );
    }

    #[tokio::test]
    async fn retrieve_memory_snippets_returns_empty_when_no_candidate_matches() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().abs();
        let memories_dir = codex_home.join("memories");
        tokio_fs::create_dir_all(memories_dir.join("rollout_summaries"))
            .await
            .unwrap();
        tokio_fs::write(
            memories_dir.join("MEMORY.md"),
            "# Notes\n\n## Rust\nUse cargo fmt before committing.\n",
        )
        .await
        .unwrap();

        let snippets =
            retrieve_memory_snippets(None, &codex_home, temp.path(), &text_input("draft a poem"))
                .await
                .unwrap();

        assert!(
            snippets.is_empty(),
            "expected no snippets, got {snippets:?}"
        );
    }

    #[tokio::test]
    async fn retrieve_memory_snippets_enforces_source_caps() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().abs();
        let memories_dir = codex_home.join("memories");
        let rollout_dir = memories_dir.join("rollout_summaries");
        tokio_fs::create_dir_all(&rollout_dir).await.unwrap();
        tokio_fs::write(
            memories_dir.join("memory_summary.md"),
            "Fix summary: keep cargo test focused.\n",
        )
        .await
        .unwrap();
        tokio_fs::write(
            memories_dir.join("MEMORY.md"),
            "# Fixes\n\n## One\nfix parser config\n\n## Two\nfix parser config again\n\n## Three\nfix parser config third\n\n## Four\nfix parser config fourth\n",
        )
        .await
        .unwrap();
        for index in 0..4 {
            tokio_fs::write(
                rollout_dir.join(format!("match-{index}.md")),
                format!("# Rollout {index}\nfix parser config in rollout {index}\n"),
            )
            .await
            .unwrap();
        }

        let snippets = retrieve_memory_snippets(
            None,
            &codex_home,
            temp.path(),
            &text_input("fix the parser config"),
        )
        .await
        .unwrap();

        let summary_count = snippets
            .iter()
            .filter(|snippet| snippet.source_kind == RetrievalSourceKind::MemorySummary)
            .count();
        let memory_count = snippets
            .iter()
            .filter(|snippet| snippet.source_kind == RetrievalSourceKind::Memory)
            .count();
        let rollout_count = snippets
            .iter()
            .filter(|snippet| snippet.source_kind == RetrievalSourceKind::RolloutSummary)
            .count();

        assert_eq!(summary_count, 1);
        assert_eq!(memory_count, 3);
        assert_eq!(rollout_count, 2);
    }

    #[tokio::test]
    async fn retrieve_memory_snippets_uses_structured_skill_path_mentions() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().abs();
        let memories_dir = codex_home.join("memories");
        tokio_fs::create_dir_all(memories_dir.join("rollout_summaries"))
            .await
            .unwrap();
        tokio_fs::write(
            memories_dir.join("MEMORY.md"),
            "# Skills\n\n## Deploy\nKeep deploy skill notes in deploy/SKILL.md when working on release automation.\n",
        )
        .await
        .unwrap();

        let snippets = retrieve_memory_snippets(
            None,
            &codex_home,
            temp.path(),
            &[UserInput::Skill {
                name: "deploy".to_string(),
                path: temp.path().join("deploy").join("SKILL.md"),
            }],
        )
        .await
        .unwrap();

        assert!(
            snippets
                .iter()
                .any(|snippet| snippet.source_path == "MEMORY.md"
                    && snippet.excerpt.contains("deploy/SKILL.md")),
            "expected a MEMORY.md snippet from structured skill path input, got {snippets:?}"
        );
    }

    #[tokio::test]
    async fn retrieve_memory_snippets_prefers_durable_memory_before_episodic_evidence() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().abs();
        let memories_dir = codex_home.join("memories");
        let rollout_dir = memories_dir.join("rollout_summaries");
        tokio_fs::create_dir_all(&rollout_dir).await.unwrap();
        tokio_fs::write(
            memories_dir.join("MEMORY.md"),
            "# Parser\n\n## Fixes\nUse the durable parser checklist before revisiting prior runs.\n",
        )
        .await
        .unwrap();
        tokio_fs::write(
            rollout_dir.join("2026-04-24T12-00-00-abcd-parser.md"),
            "# Parser rollout\nInvestigated the parser checklist in one prior run.\n",
        )
        .await
        .unwrap();

        let snippets = retrieve_memory_snippets(
            None,
            &codex_home,
            temp.path(),
            &text_input("update the parser checklist"),
        )
        .await
        .unwrap();

        assert_eq!(
            snippets.first().map(|snippet| snippet.source_kind),
            Some(RetrievalSourceKind::Memory)
        );
        let rollout_position = snippets
            .iter()
            .position(|snippet| snippet.source_kind == RetrievalSourceKind::RolloutSummary);
        assert!(
            match rollout_position {
                None => true,
                Some(index) => index > 0,
            },
            "durable snippets should be injected before episodic evidence: {snippets:?}"
        );
    }

    #[tokio::test]
    async fn retrieve_memory_snippets_requires_stronger_match_for_episodic_evidence() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().abs();
        let memories_dir = codex_home.join("memories");
        let rollout_dir = memories_dir.join("rollout_summaries");
        tokio_fs::create_dir_all(&rollout_dir).await.unwrap();
        tokio_fs::write(
            memories_dir.join("MEMORY.md"),
            "# Parser\n\n## Durable\nKeep parser guidance in MEMORY.md.\n",
        )
        .await
        .unwrap();
        tokio_fs::write(
            rollout_dir.join("2026-04-24T12-00-00-abcd-parser.md"),
            "# Parser rollout\nparser\n",
        )
        .await
        .unwrap();

        let snippets =
            retrieve_memory_snippets(None, &codex_home, temp.path(), &text_input("parser"))
                .await
                .unwrap();

        assert!(
            snippets
                .iter()
                .all(|snippet| snippet.source_kind != RetrievalSourceKind::RolloutSummary),
            "generic single-keyword matches should stay out of episodic injection: {snippets:?}"
        );
    }

    #[test]
    fn render_memory_retrieval_bundle_separates_durable_and_episodic_sections() {
        let rendered = render_memory_retrieval_bundle(&[
            RetrievedMemorySnippet {
                source_path: "MEMORY.md".to_string(),
                source_kind: RetrievalSourceKind::Memory,
                score: 21,
                excerpt: "Durable guidance".to_string(),
                line_range: Some((1, 3)),
            },
            RetrievedMemorySnippet {
                source_path: "rollout_summaries/alpha.md".to_string(),
                source_kind: RetrievalSourceKind::RolloutSummary,
                score: 18,
                excerpt: "Prior run detail".to_string(),
                line_range: Some((1, 4)),
            },
        ])
        .expect("bundle should render");

        assert!(rendered.contains("<durable_memory>"));
        assert!(rendered.contains("<episodic_evidence>"));
        assert!(rendered.contains("do not treat it as automatically promoted durable memory"));
    }

    #[tokio::test]
    async fn retrieve_memory_snippets_prefers_memory_summary_over_memory_when_both_match() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().abs();
        let memories_dir = codex_home.join("memories");
        tokio_fs::create_dir_all(memories_dir.join("rollout_summaries"))
            .await
            .unwrap();
        tokio_fs::write(
            memories_dir.join("memory_summary.md"),
            "Parser summary: use the parser recovery checklist before broader edits.\n",
        )
        .await
        .unwrap();
        tokio_fs::write(
            memories_dir.join("MEMORY.md"),
            "# Parser\n\n## Recovery\nUse the parser recovery checklist before broader edits.\n",
        )
        .await
        .unwrap();

        let snippets = retrieve_memory_snippets(
            None,
            &codex_home,
            temp.path(),
            &text_input("update the parser recovery checklist"),
        )
        .await
        .unwrap();

        assert_eq!(
            snippets.first().map(|snippet| snippet.source_kind),
            Some(RetrievalSourceKind::MemorySummary),
            "memory_summary.md should outrank MEMORY.md for the same durable match: {snippets:?}"
        );
    }

    #[tokio::test]
    async fn retrieve_memory_snippets_boosts_recent_used_rollout_with_matching_cwd_metadata() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().abs();
        let memories_dir = codex_home.join("memories");
        let rollout_dir = memories_dir.join("rollout_summaries");
        let cwd = codex_home.join("workspace-parser");
        tokio_fs::create_dir_all(&rollout_dir).await.unwrap();
        tokio_fs::create_dir_all(&cwd).await.unwrap();
        let state_db = init_state_db(&codex_home).await;

        let recent_thread = seed_rollout_memory(
            state_db.as_ref(),
            &codex_home,
            "workspace-parser",
            Utc::now() - Duration::days(2),
            "Investigated parser config regression.",
            "parser-recent",
        )
        .await;
        let older_thread = seed_rollout_memory(
            state_db.as_ref(),
            &codex_home,
            "workspace-other",
            Utc::now() - Duration::days(2),
            "Investigated parser config regression.",
            "parser-other",
        )
        .await;

        state_db
            .record_stage1_output_usage(&[recent_thread, recent_thread])
            .await
            .expect("record rollout usage");
        state_db
            .record_stage1_output_usage(&[older_thread])
            .await
            .expect("record secondary rollout usage");

        let snippets = retrieve_memory_snippets(
            Some(state_db.as_ref()),
            &codex_home,
            &cwd,
            &text_input("fix the parser config regression"),
        )
        .await
        .unwrap();

        let expected_recent_path = format!(
            "rollout_summaries/{}.md",
            rollout_summary_file_stem_from_parts(
                recent_thread,
                state_db
                    .list_memory_retrieval_records()
                    .await
                    .expect("list retrieval records")
                    .into_iter()
                    .find(|record| record.thread_id == recent_thread)
                    .expect("recent retrieval record")
                    .source_updated_at,
                Some("parser-recent"),
            )
        );
        assert_eq!(
            snippets
                .iter()
                .find(|snippet| snippet.source_kind == RetrievalSourceKind::RolloutSummary)
                .map(|snippet| snippet.source_path.as_str()),
            Some(expected_recent_path.as_str()),
            "recent used cwd-matching rollout should rank first: {snippets:?}"
        );
    }

    #[tokio::test]
    async fn retrieve_memory_snippets_downranks_suppressed_and_stale_rollouts() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().abs();
        let memories_dir = codex_home.join("memories");
        let rollout_dir = memories_dir.join("rollout_summaries");
        tokio_fs::create_dir_all(&rollout_dir).await.unwrap();
        let state_db = init_state_db(&codex_home).await;

        let fresh_thread = seed_rollout_memory(
            state_db.as_ref(),
            &codex_home,
            "workspace-fresh",
            Utc::now() - Duration::days(3),
            "Parser recovery notes for config fixes.",
            "fresh-parser",
        )
        .await;
        let suppressed_thread = seed_rollout_memory(
            state_db.as_ref(),
            &codex_home,
            "workspace-suppressed",
            Utc::now() - Duration::days(3),
            "Parser recovery notes for config fixes.",
            "suppressed-parser",
        )
        .await;
        let _stale_thread = seed_rollout_memory(
            state_db.as_ref(),
            &codex_home,
            "workspace-stale",
            Utc.timestamp_opt(1_700_000_000, 0).single().unwrap(),
            "Parser recovery notes for config fixes.",
            "stale-parser",
        )
        .await;

        state_db
            .record_stage1_output_usage(&[fresh_thread])
            .await
            .expect("record fresh usage");
        state_db
            .record_memory_negative_feedback(
                &MemoryNegativeFeedbackTarget {
                    thread_id: suppressed_thread,
                    source_updated_at: None,
                    rollout_slug: None,
                },
                MemoryNegativeFeedbackReason::WrongScope,
            )
            .await
            .expect("record suppression");

        let snippets = retrieve_memory_snippets(
            Some(state_db.as_ref()),
            &codex_home,
            temp.path(),
            &text_input("fix the parser config recovery flow"),
        )
        .await
        .unwrap();

        let rollout_sources = snippets
            .iter()
            .filter(|snippet| snippet.source_kind == RetrievalSourceKind::RolloutSummary)
            .map(|snippet| snippet.source_path.clone())
            .collect::<Vec<_>>();
        assert!(
            !rollout_sources.is_empty(),
            "expected rollout summaries to remain eligible, got {snippets:?}"
        );
        assert!(
            rollout_sources
                .first()
                .is_some_and(|path| path.contains("fresh_parser")),
            "fresh recent rollout should rank first, got {rollout_sources:?}"
        );
        if let Some(suppressed_index) = rollout_sources
            .iter()
            .position(|path| path.contains("suppressed_parser"))
        {
            assert!(
                suppressed_index > 0,
                "suppressed rollout should not outrank fresh rollout: {rollout_sources:?}"
            );
        }
        if let Some(stale_index) = rollout_sources
            .iter()
            .position(|path| path.contains("stale_parser"))
        {
            assert!(
                stale_index > 0,
                "stale rollout should not outrank fresh rollout: {rollout_sources:?}"
            );
        }
    }

    #[tokio::test]
    async fn retrieve_memory_snippets_penalizes_orphan_rollout_files_when_state_is_available() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().abs();
        let memories_dir = codex_home.join("memories");
        let rollout_dir = memories_dir.join("rollout_summaries");
        tokio_fs::create_dir_all(&rollout_dir).await.unwrap();
        let state_db = init_state_db(&codex_home).await;

        let _thread = seed_rollout_memory(
            state_db.as_ref(),
            &codex_home,
            "workspace-live",
            Utc::now() - Duration::days(2),
            "Investigated parser config reload.",
            "live-parser",
        )
        .await;
        tokio_fs::write(
            rollout_dir.join("orphan-parser.md"),
            "# Orphan\nInvestigated parser config reload.\n",
        )
        .await
        .unwrap();

        let snippets = retrieve_memory_snippets(
            Some(state_db.as_ref()),
            &codex_home,
            temp.path(),
            &text_input("fix the parser config reload"),
        )
        .await
        .unwrap();

        let rollout_sources = snippets
            .iter()
            .filter(|snippet| snippet.source_kind == RetrievalSourceKind::RolloutSummary)
            .map(|snippet| snippet.source_path.clone())
            .collect::<Vec<_>>();
        assert!(
            rollout_sources
                .iter()
                .all(|path| path != "rollout_summaries/orphan-parser.md"),
            "orphan rollout should be filtered or demoted below the relevance floor: {rollout_sources:?}"
        );
    }

    #[tokio::test]
    async fn retrieve_memory_snippets_penalizes_stale_durable_artifacts() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().abs();
        let memories_dir = codex_home.join("memories");
        tokio_fs::create_dir_all(memories_dir.join("rollout_summaries"))
            .await
            .unwrap();
        let stale_time = Utc::now() - Duration::days(400);
        let fresh_time = Utc::now() - Duration::days(2);

        let summary_path = memories_dir.join("memory_summary.md");
        let memory_path = memories_dir.join("MEMORY.md");
        tokio_fs::write(
            &summary_path,
            "Parser summary: use the parser recovery checklist before broader edits.\n",
        )
        .await
        .unwrap();
        tokio_fs::write(
            &memory_path,
            "# Parser\n\n## Recovery\nUse the parser recovery checklist before broader edits.\n",
        )
        .await
        .unwrap();
        set_modified_time(&summary_path, stale_time);
        set_modified_time(&memory_path, fresh_time);

        let snippets = retrieve_memory_snippets(
            None,
            &codex_home,
            temp.path(),
            &text_input("update the parser recovery checklist"),
        )
        .await
        .unwrap();

        assert_eq!(
            snippets.first().map(|snippet| snippet.source_path.as_str()),
            Some("MEMORY.md"),
            "fresh durable artifact should outrank stale memory_summary.md: {snippets:?}"
        );
    }
}
