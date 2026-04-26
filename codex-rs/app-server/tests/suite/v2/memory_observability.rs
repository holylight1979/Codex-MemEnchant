use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::to_response;
use chrono::Utc;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::MemoryHealthResponse;
use codex_app_server_protocol::MemoryNegativeFeedbackReason;
use codex_app_server_protocol::MemoryPeekResponse;
use codex_app_server_protocol::MemorySuppressResponse;
use codex_app_server_protocol::RequestId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_state::Phase2JobClaimOutcome;
use codex_state::Stage1JobClaimOutcome;
use codex_state::StateRuntime;
use codex_state::ThreadMetadataBuilder;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::time::timeout;
use uuid::Uuid;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn memory_peek_returns_recent_entries() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path())?;
    let state_db = init_state_db(codex_home.path()).await?;
    let now = Utc::now().timestamp();

    seed_stage1_output(
        &state_db,
        codex_home.path(),
        /*source_updated_at*/ now - 100,
        "Older summary",
    )
    .await?;
    seed_stage1_output(
        &state_db,
        codex_home.path(),
        /*source_updated_at*/ now - 10,
        "Newer summary for the operator-facing peek output",
    )
    .await?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp.send_raw_request("memory/peek", /*params*/ None).await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let response: MemoryPeekResponse = to_response::<MemoryPeekResponse>(response)?;

    assert_eq!(response.entries.len(), 2);
    assert!(
        response.entries[0]
            .summary_excerpt
            .contains("Newer summary for the operator-facing peek output"),
        "unexpected memory peek response: {response:?}"
    );
    assert!(
        response.entries[0]
            .rollout_summary_file
            .starts_with("rollout_summaries/"),
        "unexpected rollout summary file: {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn memory_health_reports_missing_and_stale_sources() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path())?;
    let state_db = init_state_db(codex_home.path()).await?;
    let now = Utc::now().timestamp();

    seed_stage1_output(
        &state_db,
        codex_home.path(),
        /*source_updated_at*/ now - 10,
        "Broken summary",
    )
    .await?;

    let rollout_dir = codex_home.path().join("memories").join("rollout_summaries");
    tokio::fs::create_dir_all(&rollout_dir).await?;
    tokio::fs::write(rollout_dir.join("stale.md"), "stale summary\n").await?;
    tokio::fs::write(
        rollout_dir.join("orphan.txt"),
        "unexpected retrieval source\n",
    )
    .await?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp
        .send_raw_request("memory/health", /*params*/ None)
        .await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let response: MemoryHealthResponse = to_response::<MemoryHealthResponse>(response)?;

    assert!(!response.ok, "expected unhealthy response: {response:?}");
    assert!(
        response
            .issues
            .iter()
            .any(|issue| issue.code == "missing_memory"),
        "expected missing_memory issue, got {response:?}"
    );
    assert!(
        response
            .issues
            .iter()
            .any(|issue| issue.code == "missing_rollout_summary"),
        "expected missing_rollout_summary issue, got {response:?}"
    );
    assert!(
        response
            .issues
            .iter()
            .any(|issue| issue.code == "stale_rollout_summary"),
        "expected stale_rollout_summary issue, got {response:?}"
    );
    assert!(
        response
            .issues
            .iter()
            .any(|issue| issue.code == "malformed_retrieval_source"),
        "expected malformed_retrieval_source issue, got {response:?}"
    );

    Ok(())
}

#[tokio::test]
async fn memory_suppress_persists_feedback_and_updates_phase2_selection() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path())?;
    let state_db = init_state_db(codex_home.path()).await?;
    let now = Utc::now().timestamp();
    let owner = ThreadId::from_string(&Uuid::new_v4().to_string())?;

    let suppressed_thread = seed_stage1_output(
        &state_db,
        codex_home.path(),
        /*source_updated_at*/ now - 20,
        "Suppressed summary",
    )
    .await?;
    let retained_thread = seed_stage1_output(
        &state_db,
        codex_home.path(),
        /*source_updated_at*/ now - 10,
        "Retained summary",
    )
    .await?;

    let initial_selection = state_db
        .get_phase2_input_selection(/*n*/ 2, /*max_unused_days*/ 36_500)
        .await?;
    assert_eq!(initial_selection.selected.len(), 2);

    let phase2_claim = state_db.try_claim_global_phase2_job(owner, 3_600).await?;
    let (phase2_token, input_watermark) = match phase2_claim {
        Phase2JobClaimOutcome::Claimed {
            ownership_token,
            input_watermark,
        } => (ownership_token, input_watermark),
        other => anyhow::bail!("unexpected initial phase2 claim outcome: {other:?}"),
    };
    assert!(
        state_db
            .mark_global_phase2_job_succeeded(
                phase2_token.as_str(),
                input_watermark,
                &initial_selection.selected,
            )
            .await?,
        "initial phase2 success should finalize"
    );

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp
        .send_raw_request(
            "memory/suppress",
            Some(json!({
                "target": {
                    "threadId": suppressed_thread.to_string()
                },
                "reason": "duplicate"
            })),
        )
        .await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let response: MemorySuppressResponse = to_response(response)?;

    assert_eq!(response.feedback.thread_id, suppressed_thread.to_string());
    assert_eq!(
        response.feedback.reason,
        MemoryNegativeFeedbackReason::Duplicate
    );

    let selection = state_db
        .get_phase2_input_selection(/*n*/ 2, /*max_unused_days*/ 36_500)
        .await?;
    assert_eq!(selection.selected.len(), 1);
    assert_eq!(selection.selected[0].thread_id, retained_thread);
    assert_eq!(selection.removed.len(), 1);
    assert_eq!(selection.removed[0].thread_id, suppressed_thread);

    let next_claim = state_db
        .try_claim_global_phase2_job(ThreadId::from_string(&Uuid::new_v4().to_string())?, 3_600)
        .await?;
    assert!(
        matches!(next_claim, Phase2JobClaimOutcome::Claimed { .. }),
        "memory suppression should enqueue a fresh phase2 pass"
    );

    Ok(())
}

async fn seed_stage1_output(
    state_db: &Arc<StateRuntime>,
    codex_home: &Path,
    source_updated_at: i64,
    rollout_summary: &str,
) -> Result<ThreadId> {
    let now = Utc::now();
    let thread_id = ThreadId::from_string(&Uuid::new_v4().to_string())?;
    let worker_id = ThreadId::from_string(&Uuid::new_v4().to_string())?;
    let mut builder = ThreadMetadataBuilder::new(
        thread_id,
        codex_home
            .join("sessions")
            .join(format!("{thread_id}.jsonl")),
        now,
        SessionSource::Cli,
    );
    builder.updated_at = Some(
        chrono::DateTime::<Utc>::from_timestamp(source_updated_at + 60, 0)
            .expect("updated_at timestamp"),
    );
    builder.cwd = codex_home.join(format!("workspace-{thread_id}"));
    let metadata = builder.build("mock_provider");
    state_db.upsert_thread(&metadata).await?;

    let claim = state_db
        .try_claim_stage1_job(
            thread_id,
            worker_id,
            source_updated_at,
            /*lease_seconds*/ 3600,
            /*max_running_jobs*/ 64,
        )
        .await?;
    let Stage1JobClaimOutcome::Claimed { ownership_token } = claim else {
        anyhow::bail!("unexpected stage1 claim outcome: {claim:?}");
    };
    assert!(
        state_db
            .mark_stage1_job_succeeded(
                thread_id,
                ownership_token.as_str(),
                source_updated_at,
                "raw memory",
                rollout_summary,
                Some("test"),
            )
            .await?,
        "stage1 success should be recorded"
    );

    Ok(thread_id)
}

async fn init_state_db(codex_home: &Path) -> Result<Arc<StateRuntime>> {
    let state_db = StateRuntime::init(codex_home.to_path_buf(), "mock_provider".into()).await?;
    state_db
        .mark_backfill_complete(/*last_watermark*/ None)
        .await?;
    Ok(state_db)
}

fn create_config_toml(codex_home: &Path) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"
model_provider = "mock_provider"
suppress_unstable_features_warning = true

[features]
sqlite = true

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "http://127.0.0.1:9/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#,
    )
}
