# Codex Memory Enhancement Roadmap

This document turns the earlier analysis into an implementation-ready plan for `openai/codex`.
It is written for direct execution in the current repository, with concrete module targets,
data-flow changes, acceptance criteria, and test guidance.

Scope: prioritize the highest-value improvements that strengthen Codex's existing memory
pipeline without importing the full complexity of the external Atomic Memory system.

Non-goals:

- Do not port the external hook ecosystem wholesale.
- Do not introduce a mandatory external vector service in the first iteration.
- Do not start with team-management memory governance unless the product explicitly needs it.

## Summary

Codex already has a strong background memory pipeline:

- startup Phase 1 extraction of rollout memory
- Phase 2 consolidation into `MEMORY.md` / `memory_summary.md`
- thread-level memory mode and retention
- memory citation usage feedback (`usage_count`, `last_usage`)

The highest-return gap is that memory is still mostly passive:

- Codex prepares memory in the background
- then asks the model to decide whether to read memory files manually

The roadmap below upgrades Codex from passive memory availability to active memory retrieval,
more precise preference capture, and usable operator controls.

## Current Local Status

Date:

- 2026-04-26

Completed in the local repo:

- P0.1 pre-turn memory retrieval injection
- P0.2 explicit user preference and decision extraction hardening
- P0.3 memory peek and memory health observability
- P1.1 memory undo / negative feedback loop
- P1.2 post-consolidation artifact validator
- P1.3 stronger episodic vs durable separation
- P2.1 retrieval ranking upgrade
- P2.2 session value scoring for consolidation / retention decisions

Current observability surface for P0.3:

- experimental app-server RPCs:
  - `memory/peek`
  - `memory/health`
  - `memory/suppress`
- implemented first in:
  - `codex-rs/state`
  - `codex-rs/app-server-protocol`
  - `codex-rs/app-server`
- no additional CLI or TUI entrypoint was required for P0.3 acceptance in this pass

## P0

These items are the best immediate investments. They should be implemented first.

### P0.1 Pre-turn Memory Retrieval Injection

Goal:

- retrieve relevant memory before the model starts a turn
- inject a bounded, curated retrieval bundle instead of relying on the model to discover memory on its own

Current baseline:

- `codex-rs/core/src/session/mod.rs` injects developer instructions produced by
  `build_memory_tool_developer_instructions`
- that prompt teaches the model how to use `memory_summary.md`, `MEMORY.md`, and rollout summaries
- retrieval is delegated to the model

Target behavior:

1. Before the turn prompt is finalized, Codex computes a retrieval query from:
   - latest user text
   - current cwd
   - explicit file paths mentioned in the user turn
   - lightweight task cues such as "fix", "review", "refactor", "install", "memory", "config"
2. Codex searches local memory artifacts and selects the top relevant snippets from:
   - `memory_summary.md`
   - `MEMORY.md`
   - `rollout_summaries/*.md`
   - optional future `skills/*`
3. Codex injects a compact "memory retrieval bundle" as an additional developer section
4. The existing read-path instructions remain in place as fallback, but the model receives the
   best matches proactively

Implementation shape:

- Add a new module, preferably outside `codex-core` if practical. If a new crate is too much for
  the first pass, use a small module under:
  - `codex-rs/core/src/memories/retrieval.rs`
- Add a typed retrieval result, for example:
  - source path
  - source kind
  - score
  - short excerpt
  - optional line range
- Build retrieval using a light local strategy first:
  - exact keyword and phrase matches
  - path and cwd boosts
  - filename boosts
  - optional BM25/FTS as a second step
- Do not require embeddings in P0

Injection point:

- `codex-rs/core/src/session/mod.rs`
- place retrieval after memory developer instructions are considered, or just before them
- recommended structure:
  - existing memory usage policy
  - retrieved memory bundle
  - fallback instructions for deeper manual lookup

Suggested retrieval budget:

- total injected memory text target: 800-2000 tokens
- default candidate cap:
  - 1 summary excerpt
  - up to 3 `MEMORY.md` excerpts
  - up to 2 rollout summary excerpts

Acceptance criteria:

- Codex can answer a memory-relevant follow-up without first performing a manual memory file search
- unrelated memory is not injected into clearly self-contained turns
- memory injection remains bounded and deterministic

Tests:

- add focused unit tests for ranking and filtering
- add session/request tests asserting retrieved memory text is included in outbound prompt payloads
- cover:
  - keyword hit in `MEMORY.md`
  - cwd/path-specific boost
  - no-hit case
  - cap enforcement

Risk:

- over-injection can reduce prompt quality
- mitigate with aggressive top-k limits and source diversity caps

### P0.2 Explicit User Preference and Decision Extraction

Goal:

- capture durable user operating preferences and repeated workflow decisions more precisely than
  generic rollout summarization

Current baseline:

- Phase 1 extraction prompt already tries to preserve user preferences
- however, the stored stage-1 schema remains mostly free-form text:
  - `raw_memory`
  - `rollout_summary`
  - `rollout_slug`

Target behavior:

1. During Phase 1, extract preference-oriented structured data alongside existing summary text
2. Feed those structured sections into Phase 2 so `MEMORY.md` and `memory_summary.md` become more
   retrieval-friendly and less prose-heavy

Recommended first-pass design:

- keep DB schema changes minimal for the first iteration
- do not immediately add many new SQL columns
- instead, strengthen the Phase 1 prompt contract so `raw_memory` has required sections such as:
  - `Preference signals`
  - `Reusable knowledge`
  - `Failures and how to do differently`
  - `Scope/cwd`
  - `High-value commands or paths`

Why this shape:

- it improves downstream consolidation quickly
- it avoids a risky migration before we validate signal quality

Files to change:

- `codex-rs/core/templates/memories/stage_one_system.md`
- `codex-rs/core/templates/memories/stage_one_input.md` only if framing needs adjustment
- `codex-rs/core/src/memories/phase1.rs` if output validation needs stronger checks

Optional incremental schema work after prompt improvement:

- extend `StageOneOutput` with optional fields:
  - `keywords`
  - `task_family`
  - `cwd_scope`
  - `preference_signals`
- only do this after prompt stability is verified

Implementation checkpoint:

- the first implementation pass should keep the DB contract unchanged
  (`raw_memory`, `rollout_summary`, `rollout_slug`)
- harden Phase 1 by requiring deterministic task-block sections in `raw_memory`, including:
  - `Preference signals`
  - `Decision signals`
  - `Scope and cwd notes`
  - `Reusable knowledge`
  - `Failures and how to do differently`
  - `High-value commands or paths`
- add validation in `phase1.rs` so non-noop outputs that drift back to free-form prose fail fast
  instead of being persisted

Acceptance criteria:

- at least one repeated or explicit user operating preference can survive from rollout to
  consolidated memory without depending on broad prose summaries
- Phase 2 can create more grep-friendly `MEMORY.md` content from Phase 1 outputs

Tests:

- prompt snapshot tests for Phase 1 templates
- parsing tests if `StageOneOutput` schema changes
- end-to-end memory tests asserting a user correction is preserved into consolidated artifacts

Risk:

- extraction can drift into over-remembering transient instructions
- mitigate with prompt rules that require durability and future utility

### P0.3 Memory Peek and Memory Health Commands

Goal:

- make memory observable and debuggable from inside Codex

Current baseline:

- Codex has reset and settings support for memories
- Codex does not yet have a direct operator-facing way to inspect what was extracted or whether
  artifacts are healthy

Target behavior:

- add app-server or CLI/TUI-visible commands for:
  - memory peek
  - memory health

Recommended P0 scope:

1. `memory/peek`
   - show recent Phase 1 outputs selected for consolidation
   - include thread id, updated_at, cwd, rollout summary filename, and a short summary excerpt
2. `memory/health`
   - validate memory root existence
   - validate `MEMORY.md`, `memory_summary.md`, `raw_memories.md`
   - report stale or missing rollout summary references
   - report malformed retrieval bundle sources once P0.1 exists

Likely implementation locations:

- app-server protocol:
  - `codex-rs/app-server-protocol/src/protocol/v2.rs`
- app-server handler:
  - `codex-rs/app-server/src/codex_message_processor.rs`
- runtime helpers:
  - `codex-rs/state/src/runtime/memories.rs`
- TUI exposure can come after the app-server/CLI API exists

Acceptance criteria:

- an operator can inspect current memory state without opening files manually
- artifact breakage is visible immediately

Tests:

- app-server protocol tests for new RPC methods
- state/runtime tests for inspection results

Implementation checkpoint:

- complete in the local repo as of 2026-04-24
- current response shape is intentionally structured and deterministic
- `memory/peek` returns recent persisted stage-1 outputs with:
  - thread id
  - thread updated-at
  - source updated-at
  - generated-at
  - cwd
  - rollout summary filename
  - short summary excerpt
- `memory/health` returns:
  - memory root status
  - durable artifact status for `MEMORY.md`, `memory_summary.md`, `raw_memories.md`
  - rollout summaries directory status
  - expected rollout summary files for the current phase-2 selection
  - stale rollout summaries
  - structured warning/error issues for malformed retrieval sources and missing artifacts

Risk:

- low; these are observability features and should be isolated

## P1

These are the next most valuable once P0 is working.

### P1.1 Memory Undo / Negative Feedback Loop

Goal:

- let users correct bad automatic memory capture
- use that correction as a ranking signal, not just a file deletion

Current baseline:

- Codex records positive usage via memory citations
- there is no explicit "this memory was wrong or unhelpful" operator path

Target behavior:

- support undoing or suppressing a memory item, with a reason
- store negative feedback so later ranking and retention can downweight similar outputs

Recommended first implementation:

- do not build a full rejected-memory filesystem taxonomy yet
- add a small negative-feedback store keyed by thread id or stage-1 output identity
- reasons can be simple:
  - transient
  - incorrect inference
  - wrong scope
  - privacy-sensitive
  - duplicate

Likely implementation slices:

- new runtime table or lightweight extension to state DB
- app-server endpoint for memory suppression or rejection
- optional TUI action later

Acceptance criteria:

- a user can suppress a bad memory
- future phase-2 selection reduces the chance of re-promoting that memory

Implementation checkpoint:

- complete in the local repo as of 2026-04-24
- current scope intentionally stays minimal and deterministic:
  - no rejected-memory filesystem taxonomy
  - no new TUI workflow
  - no broad ranking-framework rewrite
- implemented first in:
  - `codex-rs/state/migrations/0028_memory_negative_feedback.sql`
  - `codex-rs/state/src/model/memories.rs`
  - `codex-rs/state/src/runtime/memories.rs`
  - `codex-rs/app-server-protocol/src/protocol/v2.rs`
  - `codex-rs/app-server-protocol/src/protocol/common.rs`
  - `codex-rs/app-server/src/codex_message_processor.rs`
- current API surface:
  - experimental app-server RPC `memory/suppress`
- current persistence shape:
  - sqlite table `memory_negative_feedback`
  - keyed by persisted stage-1 snapshot identity:
    - `thread_id`
    - `source_updated_at`
  - stores:
    - `rollout_slug`
    - `reason`
    - `created_at`
    - `updated_at`
- current reason set:
  - `transient`
  - `incorrect_inference`
  - `wrong_scope`
  - `privacy_sensitive`
  - `duplicate`
- current suppression behavior:
  - the app-server accepts a target identified by `thread_id` plus optional
    `source_updated_at` / `rollout_slug`
  - the runtime resolves that target to the current persisted stage-1 snapshot
  - recording negative feedback enqueues a fresh global phase-2 run
  - phase-2 input selection excludes exact stage-1 snapshots that have
    persisted negative feedback
  - if the suppressed snapshot participated in the previous successful phase-2
    baseline, it appears as `removed` in the next consolidation diff
- intentionally not done in this pass:
  - suppressing specific `MEMORY.md` blocks directly
  - unsuppress / undo UI
  - similarity-based penalties across related memories

### P1.2 Post-consolidation Artifact Validator

Goal:

- ensure `MEMORY.md` and `memory_summary.md` remain structurally useful

Current baseline:

- consolidation quality depends heavily on the model prompt
- no deterministic post-check enforces output quality

Target behavior:

- after Phase 2 agent completion and before success is finalized:
  - validate file presence
  - validate format invariants
  - validate referenced rollout summaries exist
  - validate content is not empty or degenerate when inputs exist
- if validation fails:
  - mark job failed
  - optionally retry once with a corrective prompt in a future iteration

Implementation locations:

- `codex-rs/core/src/memories/phase2.rs`
- add a small validator module such as:
  - `codex-rs/core/src/memories/validate.rs`

Validation rules worth implementing first:

- `memory_summary.md` exists when selected inputs exist
- `MEMORY.md` exists when selected inputs exist
- rollout summary filenames referenced from `MEMORY.md` exist under `rollout_summaries/`
- files are non-empty and above a minimal size

Acceptance criteria:

- malformed consolidation artifacts no longer quietly become the active memory state

Implementation checkpoint:

- complete in the local repo as of 2026-04-24
- added deterministic validation in:
  - `codex-rs/core/src/memories/validate.rs`
  - `codex-rs/core/src/memories/phase2.rs`
- current validator blocks Phase 2 success finalization when artifact inputs
  exist and any of the following is true:
  - `MEMORY.md` is missing
  - `memory_summary.md` is missing
  - `raw_memories.md` is missing or still contains the empty placeholder
  - an expected `rollout_summaries/*.md` file is missing
  - `MEMORY.md` references a rollout summary file that is absent on disk
  - `MEMORY.md` omits a rollout summary file from the current materialized
    phase-2 artifact set
  - `MEMORY.md` or `memory_summary.md` is empty or structurally degenerate
- current scope intentionally does not add retry prompting or agent retry flow
  yet; failures are reported deterministically and leave the Phase 2 job in the
  failed path

### P1.3 Stronger Episodic vs Durable Separation

Goal:

- separate evidence artifacts from consolidated reusable guidance more clearly

Current baseline:

- Codex has rollout summaries and stage-1 raw memories, but the conceptual separation between
  episodic evidence and durable reusable memory is only implicit

Target behavior:

- explicitly treat:
  - rollout summaries as episodic evidence
  - `MEMORY.md` / `memory_summary.md` as durable operational memory
- retrieval should prefer durable memory first, then episodic evidence when needed

Implementation shape:

- retrieval policy change, not a new major storage system
- update prompt wording and retrieval source ordering

Acceptance criteria:

- most turns use durable memory by default
- episodic summaries are pulled in only when exact evidence or prior run detail is needed

Implementation checkpoint:

- complete in the local repo as of 2026-04-26
- scope intentionally stayed inside the current markdown + state DB design:
  - no schema migration
  - no embedding / vector service
  - no P2.1 ranking overhaul
- implemented first in:
  - `codex-rs/core/src/memories/retrieval.rs`
  - `codex-rs/core/src/memories/prompts.rs`
  - `codex-rs/core/src/memories/phase2.rs`
  - `codex-rs/core/src/memories/validate.rs`
  - `codex-rs/core/src/memories/README.md`
  - `codex-rs/core/templates/memories/consolidation.md`
  - `codex-rs/core/templates/memories/stage_one_system.md`
- current behavior:
  - pre-turn retrieval now separates durable memory (`MEMORY.md`, `memory_summary.md`) from
    episodic evidence (`rollout_summaries/*.md`)
  - durable memory is selected and injected first as the default behavioral baseline
  - episodic evidence is injected only after a stronger scope match, biased toward current
    task/cwd/path relevance
  - the retrieval bundle now renders durable and episodic sections separately
  - Phase 2 prompts now explicitly distinguish:
    - content that should stay in rollout summaries / raw memories as episodic evidence
    - content that is eligible for promotion into durable artifacts
  - post-consolidation validation now rejects durable artifacts that directly reference
    `raw_memories.md`

## P2

These are valuable, but should follow once the fundamentals above are stable.

### P2.1 Retrieval Ranking Upgrade

Goal:

- improve relevance ordering using actual usage and scope signals

Current baseline:

- Phase 2 already ranks selection using:
  - `usage_count`
  - `last_usage`
  - `source_updated_at`

Target behavior:

- retrieval ranking should also use:
  - cwd/project similarity
  - source kind weighting
  - explicit preference memory boosts
  - negative feedback penalties
  - recency decay tuned by memory type

Implementation notes:

- keep the first version transparent and debuggable
- avoid opaque scoring formulas until inspection tools exist

Implementation checkpoint:

- complete in the local repo as of 2026-04-26
- scope intentionally stayed inside the current markdown + state DB design:
  - no schema migration
  - no vector / embedding retrieval
  - no rollout reprocessing / session scoring work
- implemented first in:
  - `codex-rs/core/src/memories/retrieval.rs`
  - `codex-rs/core/src/memories/prompts.rs`
  - `codex-rs/core/src/session/turn.rs`
  - `codex-rs/state/src/model/memories.rs`
  - `codex-rs/state/src/model/mod.rs`
  - `codex-rs/state/src/lib.rs`
  - `codex-rs/state/src/runtime/memories.rs`
  - `codex-rs/core/src/memories/README.md`
  - `docs/memory-build-guide-windows.md`
- current behavior:
  - pre-turn retrieval still uses local markdown artifacts, but ranking now also reads
    state-backed rollout metadata when available
  - deterministic scoring now combines:
    - query keyword / filename matches
    - cwd and path matches from both file text and state metadata
    - task-cue matches
    - durable-vs-episodic source bias
    - `usage_count`
    - `last_usage`
    - negative feedback / suppression penalties
    - stale penalties tuned by source kind
  - durable ordering is now explicit and testable:
    - `memory_summary.md` keeps a higher durable-summary bias
    - `MEMORY.md` can still outrank a stale `memory_summary.md` when freshness differs materially
  - rollout summary ordering is now explicit and testable:
    - rollout summaries with matching cwd/path metadata and recent positive usage rise
    - suppressed, orphaned, or stale rollout summaries are demoted and can fall below the relevance floor
  - the retrieval bundle header now explains the ranking dimensions at a high level without adding
    a large per-snippet trace payload

### P2.2 Session Value Scoring for Reprocessing and Backfill

Goal:

- estimate whether a past session is worth memory extraction or re-extraction

Why P2:

- this is useful, but it is not the first-order reason Codex memory feels weak today
- memory feels weak mainly because retrieval is passive and preference capture is underspecified

Target behavior:

- derive a light session score from:
  - number of useful outputs captured
  - usage later observed
  - amount of user correction
  - token cost
  - extraction confidence
- use it for:
  - targeted backfill
  - prioritizing re-extraction after prompt changes

Acceptance criteria:

- session scoring influences maintenance or backfill workflows, not just dashboards

Implementation checkpoint:

- complete in the local repo as of 2026-04-26
- scope intentionally stayed minimal and deterministic:
  - no schema migration
  - no learning model
  - no dashboard / UI work
  - no phase-1 prompt rewrite beyond the existing structured sections from P0.2
- implemented first in:
  - `codex-rs/state/src/model/memories.rs`
  - `codex-rs/state/src/model/mod.rs`
  - `codex-rs/state/src/lib.rs`
  - `codex-rs/state/src/runtime/memories.rs`
  - `codex-rs/core/src/memories/README.md`
  - `docs/memory-build-guide-windows.md`
- current behavior:
  - each persisted stage-1 output now gets a deterministic heuristic `SessionValueScore`
    at selection time
  - the score combines:
    - novelty from unique structured Phase-1 bullets
    - preference / decision density
    - reusable command / path signal
    - failure-and-recovery signal
    - successful completion / observed citation signal from `usage_count` / `last_usage`
    - negative feedback / suppression penalty
    - duplication penalty within the current candidate set
    - low-information penalty
  - phase-2 selection no longer orders only by usage / recency:
    - candidates are now score-ranked first
    - suppressed snapshots are still excluded from active selection
    - usage / recency remain deterministic tie-breakers after score
  - stale retention pruning also uses the same score:
    - suppressed / low-value / duplicate stale memories are pruned before older
      but higher-value ones
  - the first pass intentionally wires score into:
    - consolidation selection
    - maintenance / retention
    - but not yet startup phase-1 claim ordering

## Recommended Execution Order

1. P0.1 pre-turn retrieval injection
2. P0.2 stronger preference and decision extraction
3. P0.3 memory peek and health
4. P1.2 post-consolidation validator
5. P1.1 memory undo / negative feedback
6. P1.3 episodic vs durable retrieval policy refinement
7. P2.1 ranking upgrade
8. P2.2 session value scoring

## Suggested File Map

The list below is the most likely initial write-set for implementation.

P0.1:

- `codex-rs/core/src/session/mod.rs`
- `codex-rs/core/src/memories/prompts.rs`
- `codex-rs/core/src/memories/mod.rs`
- new: `codex-rs/core/src/memories/retrieval.rs`

P0.2:

- `codex-rs/core/templates/memories/stage_one_system.md`
- optionally `codex-rs/core/src/memories/phase1.rs`
- possibly `codex-rs/core/src/memories/prompts_tests.rs`

P0.3:

- `codex-rs/app-server-protocol/src/protocol/v2.rs`
- `codex-rs/app-server/src/codex_message_processor.rs`
- `codex-rs/state/src/runtime/memories.rs`
- tests in corresponding crates

P1.2:

- `codex-rs/core/src/memories/phase2.rs`
- new: `codex-rs/core/src/memories/validate.rs`

P1.1:

- `codex-rs/state/migrations/...`
- `codex-rs/state/src/runtime/memories.rs`
- app-server/TUI call sites once the API exists

## Validation Strategy

For each phase:

- unit-test the selection and ranking logic
- integration-test prompt/request assembly
- keep artifact validation deterministic
- verify docs if API/config behavior changes

For the overall feature:

- reproduce one memory-relevant turn with no manual memory file search
- confirm retrieved memory appears in outbound prompt construction
- confirm positive and negative feedback affect later selection

## Stop Conditions

Pause and reassess if any of the following happens:

- retrieval bundles routinely exceed budget and degrade answer quality
- preference extraction begins preserving transient or private user content
- validator failures show prompt design is too unstable to support the next phase

## Recommended First Coding Session

When starting implementation, begin with P0.1 only.

Concrete first session objective:

- add a minimal local retrieval helper
- inject one bounded retrieval bundle into developer context
- write tests that prove it appears in the turn prompt

Do not combine P0.1 and schema migrations in the first implementation session.

## Final Stabilization Checkpoint

Date:

- 2026-04-26

Scope:

- final stabilization / acceptance pass only
- no new phases
- no retrieval architecture expansion
- no vector / embedding work
- no new operator surface beyond the current experimental RPCs

Acceptance result:

- the current local repo can be treated as a usable milestone for the intended
  memory-enhancement scope
- the memory data flow is closed end to end:
  - Phase 1 extracts structured raw memory plus rollout summaries
  - Phase 2 selects, materializes, and consolidates episodic evidence
  - post-consolidation validation blocks malformed durable artifacts
  - pre-turn retrieval injects bounded memory before the model starts work
  - `memory/peek` and `memory/health` expose operator observability
  - `memory/suppress` persists negative feedback and re-dirties Phase 2
  - durable and episodic sources stay separated in retrieval and validation
  - retrieval ranking and session-value scoring are active policy signals

Focused verification completed in the local repo:

- `cargo test -p codex-core --lib memories::retrieval::tests`
- `cargo test -p codex-core --lib memories::prompts::tests`
- `cargo test -p codex-core --lib memories::validate::tests`
- `cargo test -p codex-core includes_memory_retrieval_bundle_in_initial_request_when_memories_match`
- `cargo test -p codex-state get_phase2_input_selection_`
- `cargo test -p codex-state prune_stage1_outputs_for_retention_`
- `cargo test -p codex-state memory_negative_feedback`
- `cargo test -p codex-app-server-protocol memory_`
- `cargo test -p codex-app-server --test all suite::v2::memory_observability::memory_peek_returns_recent_entries -- --exact`
- `cargo test -p codex-app-server --test all suite::v2::memory_observability::memory_health_reports_missing_and_stale_sources -- --exact`
- `cargo test -p codex-app-server --test all suite::v2::memory_observability::memory_suppress_persists_feedback_and_updates_phase2_selection -- --exact`
- `cargo test -p codex-app-server --test all suite::v2::experimental_api::memory_suppress_requires_experimental_api_capability -- --exact`
- `cargo build -p codex-cli`

Verification note:

- in this Windows environment, broad filtered invocation
  `cargo test -p codex-app-server --test all memory_`
  can still hit a `BrokenPipe` test-listing failure while enumerating tests
- exact test-name invocation passed and is the reliable local workaround for
  this acceptance pass

Residual risks:

- retrieval quality is still heuristic and markdown-based; it is transparent and
  testable, but not semantically deep
- suppression is snapshot-scoped and deterministic; there is still no unsuppress
  flow or similarity-based penalty across related memories
- session-value scoring affects selection and retention, but not startup
  phase-1 claim prioritization yet
- there is still no dedicated operator-facing score inspection surface
- validator failure currently stops Phase 2 deterministically, but does not yet
  retry with a corrective prompt

What is worth doing later:

- upstream-friendly cleanup, naming polish, and commit slicing
- broader acceptance / smoke automation for the experimental RPCs
- startup claim prioritization from the existing session-value score
- optional score observability if operators need to debug selection decisions

What is not worth doing yet:

- adding a new phase
- vector retrieval or embedding service work
- broad memory-governance architecture
- heavy retrieval-framework refactors before the current milestone is exercised
