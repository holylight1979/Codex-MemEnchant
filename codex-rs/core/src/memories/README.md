# Memories Pipeline (Core)

This module runs a startup memory pipeline for eligible sessions.

## Prompt Templates

Memory prompt templates live under `codex-rs/core/templates/memories/`.

- The undated template files are the canonical latest versions used at runtime:
  - `stage_one_system.md`
  - `stage_one_input.md`
  - `consolidation.md`
  - `read_path.md`
- In `codex`, edit those undated template files in place.
- The dated snapshot-copy workflow is used in the separate `openai/project/agent_memory/write` harness repo, not here.

## When it runs

The pipeline is triggered when a root session starts, and only if:

- the session is not ephemeral
- the memory feature is enabled
- the session is not a sub-agent session
- the state DB is available

It runs asynchronously in the background and executes two phases in order: Phase 1, then Phase 2.

## Phase 1: Rollout Extraction (per-thread)

Phase 1 finds recent eligible rollouts and extracts a structured memory from each one.

Eligible rollouts are selected from the state DB using startup claim rules. In practice this means
the pipeline only considers rollouts that are:

- from allowed interactive session sources
- within the configured age window
- idle long enough (to avoid summarizing still-active/fresh rollouts)
- not already owned by another in-flight phase-1 worker
- within startup scan/claim limits (bounded work per startup)

What it does:

- claims a bounded set of rollout jobs from the state DB (startup claim)
- filters rollout content down to memory-relevant response items
- sends each rollout to a model (in parallel, with a concurrency cap)
- expects structured output containing:
  - a detailed `raw_memory`
  - a compact `rollout_summary`
  - an optional `rollout_slug`
- requires `raw_memory` task blocks to use a fixed Phase 1 skeleton:
  - `Preference signals:`
  - `Decision signals:`
  - `Scope and cwd notes:`
  - `Reusable knowledge:`
  - `Failures and how to do differently:`
  - `High-value commands or paths:`
- redacts secrets from the generated memory fields
- validates the stage-1 output structure before persisting it
- stores successful outputs back into the state DB as stage-1 outputs

Concurrency / coordination:

- Phase 1 runs multiple extraction jobs in parallel (with a fixed concurrency cap) so startup memory generation can process several rollouts at once.
- Each job is leased/claimed in the state DB before processing, which prevents duplicate work across concurrent workers/startups.
- Failed jobs are marked with retry backoff, so they are retried later instead of hot-looping.

Job outcomes:

- `succeeded` (memory produced)
- `succeeded_no_output` (valid run but nothing useful generated)
- `failed` (with retry backoff/lease handling in DB)

Phase 1 is the stage that turns individual rollouts into DB-backed memory records.

## Phase 2: Global Consolidation

Phase 2 consolidates the latest stage-1 outputs into the filesystem memory artifacts and then runs a dedicated consolidation agent.

What it does:

- claims a single global phase-2 job (so only one consolidation runs at a time)
- loads a bounded set of stage-1 outputs from the state DB using phase-2
  selection rules:
  - ignores memories whose `last_usage` falls outside the configured
    `max_unused_days` window
  - for memories with no `last_usage`, falls back to `generated_at` so fresh
    never-used memories can still be selected
  - computes a deterministic session-value score from:
    - novelty in structured Phase-1 bullets
    - preference / decision density
    - reusable command / path signals
    - failure-and-recovery signals
    - successful completion and observed citation signals from
      `usage_count` / `last_usage`
    - negative feedback / suppression
    - duplicate-session penalties within the current candidate set
    - low-information penalties
  - ranks eligible memories by session-value score first, then uses
    `usage_count` and the most recent `last_usage` / `generated_at` as
    deterministic tie-breakers
- computes a completion watermark from the claimed watermark + newest input timestamps
- syncs local memory artifacts under the memories root:
  - `raw_memories.md` (merged raw memories, latest first)
  - `rollout_summaries/` (one summary file per retained rollout)
- prunes stale rollout summaries that are no longer retained
- finds old resource files from memory extensions under
  `memories_extensions/<extension>/resources/` for extension directories that
  have an `instructions.md`, using the memory module retention window
- if there are no Phase 1 inputs or old extension resources, marks the job
  successful and exits

If there is input, it then:

- spawns an internal consolidation sub-agent
- builds the Phase 2 prompt with a diff of the current Phase 1 input
  selection versus the last successful Phase 2 selection (`added`,
  `retained`, `removed`)
- includes old extension resource paths in the prompt diff
- runs it with no approvals, no network, and local write access only
- disables collab for that agent (to prevent recursive delegation)
- watches the agent status and heartbeats the global job lease while it runs
- runs a deterministic post-consolidation artifact validator before
  recording Phase 2 success
- fails the Phase 2 job instead of finalizing success when the validator
  detects missing/degenerate durable artifacts or rollout-reference
  mismatches
- marks the phase-2 job success/failure in the state DB when the agent finishes
- prunes old extension resource files after the consolidation agent completes
  and the successful Phase 2 job is recorded

Selection diff behavior:

- successful Phase 2 runs mark the exact stage-1 snapshots they consumed with
  `selected_for_phase2 = 1` and persist the matching
  `selected_for_phase2_source_updated_at`
- Phase 1 upserts preserve the previous `selected_for_phase2` baseline until
  the next successful Phase 2 run rewrites it
- the next Phase 2 run compares the current top-N stage-1 inputs against that
  prior snapshot selection to label inputs as `added` or `retained`; a
  refreshed thread stays `added` until Phase 2 successfully selects its newer
  snapshot
- rows that were previously selected but still exist outside the current top-N
  selection are surfaced as `removed`
- before the agent starts, local `rollout_summaries/` and `raw_memories.md`
  keep the union of the current selection and the previous successful
  selection, so removed-thread evidence stays available during forgetting
- stale retention also reuses the session-value score so maintenance prunes
  low-value / duplicate / suppressed stale memories before older but richer
  ones

Watermark behavior:

- The global phase-2 job claim includes an input watermark representing the latest input timestamp known when the job was claimed.
- Phase 2 recomputes a `new_watermark` using the max of:
  - the claimed watermark
  - the newest `source_updated_at` timestamp in the stage-1 inputs it actually loaded
- On success, Phase 2 stores that completion watermark in the DB.
- This lets later phase-2 runs know whether new stage-1 data arrived since the last successful consolidation (dirty vs not dirty), while also avoiding moving the watermark backwards.

In practice, this phase is responsible for refreshing the on-disk memory workspace and producing/updating the higher-level consolidated memory outputs.

Artifact roles and retrieval policy:

- Durable artifacts:
  - `MEMORY.md`
  - `memory_summary.md`
  - optional `skills/*`
- Episodic artifacts:
  - `raw_memories.md`
  - `rollout_summaries/*.md`
- Phase 2 uses episodic artifacts as evidence inputs, but should promote only reusable,
  stable guidance into durable artifacts.
- Pre-turn retrieval now treats the two classes differently:
  - durable memory is injected first and acts as the default behavioral baseline
  - episodic evidence is injected only when it is strongly tied to the current task,
    cwd, or explicit path mentions
  - ranking is deterministic and now combines lexical matches, cwd/path scope,
    durable-vs-episodic source bias, usage/recency metadata when available, and
    stale / suppression penalties
  - episodic evidence should support or audit a likely match, not silently become a
    durable default
- A useful rule of thumb:
  - if the point mainly helps reconstruct one prior run, keep it in episodic artifacts
  - if the point should change future default behavior, it is a candidate for durable memory

Post-consolidation validator coverage currently checks:

- `MEMORY.md` exists for non-empty Phase 2 artifact inputs
- `memory_summary.md` exists for non-empty Phase 2 artifact inputs
- `raw_memories.md` exists and is not still the empty placeholder when
  inputs exist
- every rollout summary file expected from the current materialized
  phase-2 artifact set exists under `rollout_summaries/`
- `MEMORY.md` references only rollout summary files that exist in the
  current materialized set
- current materialized rollout summaries are not silently omitted from
  `MEMORY.md`
- durable artifacts do not directly reference `raw_memories.md`
- `MEMORY.md` / `memory_summary.md` are not empty or obviously
  degenerate

## Why it is split into two phases

- Phase 1 scales across many rollouts and produces normalized per-rollout memory records.
- Phase 2 serializes global consolidation so the shared memory artifacts are updated safely and consistently.
