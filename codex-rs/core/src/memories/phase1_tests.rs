use super::JobOutcome;
use super::JobResult;
use super::StageOneOutput;
use super::aggregate_stats;
use super::job::serialize_filtered_rollout_response_items;
use super::validate_stage_one_output;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::TokenUsage;
use pretty_assertions::assert_eq;

#[test]
fn serializes_memory_rollout_with_agents_removed_but_environment_kept() {
    let mixed_contextual_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>"
                    .to_string(),
            },
            ContentItem::InputText {
                text: "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>".to_string(),
            },
        ],
        end_turn: None,
        phase: None,
    };
    let skill_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "<skill>\n<name>demo</name>\n<path>skills/demo/SKILL.md</path>\nbody\n</skill>"
                .to_string(),
        }],
        end_turn: None,
        phase: None,
    };
    let subagent_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "<subagent_notification>{\"agent_id\":\"a\",\"status\":\"completed\"}</subagent_notification>"
                .to_string(),
        }],
        end_turn: None,
        phase: None,
    };

    let serialized = serialize_filtered_rollout_response_items(&[
        RolloutItem::ResponseItem(mixed_contextual_message),
        RolloutItem::ResponseItem(skill_message),
        RolloutItem::ResponseItem(subagent_message.clone()),
    ])
    .expect("serialize");
    let parsed: Vec<ResponseItem> = serde_json::from_str(&serialized).expect("parse");

    assert_eq!(
        parsed,
        vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>"
                        .to_string(),
                }],
                end_turn: None,
                phase: None,
            },
            subagent_message,
        ]
    );
}

#[test]
fn serializes_memory_rollout_redacts_secrets_before_prompt_upload() {
    let serialized = serialize_filtered_rollout_response_items(&[RolloutItem::ResponseItem(
        ResponseItem::FunctionCallOutput {
            call_id: "call_123".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(
                    r#"{"token":"sk-abcdefghijklmnopqrstuvwxyz123456"}"#.to_string(),
                ),
                success: Some(true),
            },
        },
    )])
    .expect("serialize");

    assert!(!serialized.contains("sk-abcdefghijklmnopqrstuvwxyz123456"));
    assert!(serialized.contains("[REDACTED_SECRET]"));
}

#[test]
fn count_outcomes_sums_token_usage_across_all_jobs() {
    let counts = aggregate_stats(vec![
        JobResult {
            outcome: JobOutcome::SucceededWithOutput,
            token_usage: Some(TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 2,
                output_tokens: 3,
                reasoning_output_tokens: 1,
                total_tokens: 13,
            }),
        },
        JobResult {
            outcome: JobOutcome::SucceededNoOutput,
            token_usage: Some(TokenUsage {
                input_tokens: 7,
                cached_input_tokens: 1,
                output_tokens: 2,
                reasoning_output_tokens: 0,
                total_tokens: 9,
            }),
        },
        JobResult {
            outcome: JobOutcome::Failed,
            token_usage: None,
        },
    ]);

    assert_eq!(counts.claimed, 3);
    assert_eq!(counts.succeeded_with_output, 1);
    assert_eq!(counts.succeeded_no_output, 1);
    assert_eq!(counts.failed, 1);
    assert_eq!(
        counts.total_token_usage,
        Some(TokenUsage {
            input_tokens: 17,
            cached_input_tokens: 3,
            output_tokens: 5,
            reasoning_output_tokens: 1,
            total_tokens: 22,
        })
    );
}

#[test]
fn count_outcomes_keeps_usage_empty_when_no_job_reports_it() {
    let counts = aggregate_stats(vec![
        JobResult {
            outcome: JobOutcome::SucceededWithOutput,
            token_usage: None,
        },
        JobResult {
            outcome: JobOutcome::Failed,
            token_usage: None,
        },
    ]);

    assert_eq!(counts.claimed, 2);
    assert_eq!(counts.total_token_usage, None);
}

#[test]
fn validate_stage_one_output_allows_structured_output() {
    let output = sample_stage_one_output();
    validate_stage_one_output(&output).expect("structured stage-1 output should validate");
}

#[test]
fn validate_stage_one_output_allows_full_noop() {
    let output = StageOneOutput {
        raw_memory: String::new(),
        rollout_summary: String::new(),
        rollout_slug: None,
    };

    validate_stage_one_output(&output).expect("full no-op should validate");
}

#[test]
fn validate_stage_one_output_rejects_missing_task_section() {
    let mut output = sample_stage_one_output();
    output.raw_memory = output.raw_memory.replace("Decision signals:\n", "");

    let err = validate_stage_one_output(&output).expect_err("missing task section should fail");
    assert!(err.to_string().contains("Decision signals:"));
}

#[test]
fn validate_stage_one_output_rejects_section_without_bullets() {
    let mut output = sample_stage_one_output();
    output.raw_memory = output.raw_memory.replace(
        "High-value commands or paths:\n- `cargo test -p codex-core memories::tests`\n",
        "High-value commands or paths:\n",
    );

    let err = validate_stage_one_output(&output).expect_err("section without bullets should fail");
    assert!(err.to_string().contains("must include at least one bullet"));
}

#[test]
fn validate_stage_one_output_rejects_rollout_summary_without_task_block() {
    let mut output = sample_stage_one_output();
    output.rollout_summary =
        "# Summary only\n\nRollout context: memory pipeline hardening.\n".to_string();

    let err = validate_stage_one_output(&output).expect_err("missing task block should fail");
    assert!(err.to_string().contains("task section"));
}

fn sample_stage_one_output() -> StageOneOutput {
    StageOneOutput {
        raw_memory: "\
---
description: Hardened phase-1 memory extraction structure
task: codex-memory-p0-2
task_group: codex-memory
task_outcome: success
cwd: C:\\CodexSource\\codex
keywords: codex-memory, phase1, preference-signals, decision-signals
---

### Task 1: Harden phase-1 memory structure

task: harden-phase1-memory-structure
task_group: codex-memory
task_outcome: success

Preference signals:
- when implementing memory work, the user asked to strengthen preference extraction without starting schema migration -> prefer prompt and phase-1 structure changes first for similar work

Decision signals:
- preserve the prompt-first phase-1 contract

Scope and cwd notes:
- primary cwd is `C:\\CodexSource\\codex`; keep this memory scoped to the local Codex checkout

Reusable knowledge:
- `raw_memory` should keep deterministic task sections so Phase 2 can consolidate it more reliably

Failures and how to do differently:
- avoid returning free-form prose blocks because they are harder for Phase 2 to consolidate consistently

High-value commands or paths:
- `cargo test -p codex-core memories::tests`
"
        .to_string(),
        rollout_summary: "\
# Hardened phase-1 memory structure

Rollout context: improved preference and decision extraction for Codex memories.

## Task 1: Harden phase-1 memory structure

Outcome: success

Preference signals:
- the user asked to focus on prompt/Phase 1 structure instead of DB schema work
"
        .to_string(),
        rollout_slug: Some("harden-phase1-memory-structure".to_string()),
    }
}
