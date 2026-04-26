# Codex Memory Workstream Build Guide (Windows)

This guide is the operator handoff for the Codex memory enhancement work. Its purpose is simple:
after implementation is complete, the same machine should be able to build, run, and verify the
modified Codex binary without rediscovering the workflow.

Primary workspace:

- repo root: `C:\CodexSource\codex`
- Rust workspace root: `C:\CodexSource\codex\codex-rs`

Important upstream note:

- the current upstream install guide documents Windows support primarily through WSL2
- this workstream should still keep a practical Windows PowerShell path documented because the
  local machine is operating from Windows paths today

## One-time prerequisites

Install or verify:

- Rust toolchain with `cargo`, `rustfmt`, `clippy`
- `just`
- optional: `cargo-nextest`
- optional for later: Bazel if you need release-style remote builds, but not required for the
  first implementation loop

Recommended checks in PowerShell:

```powershell
cargo --version
rustup --version
just --version
```

If `rustfmt` or `clippy` is missing:

```powershell
rustup component add rustfmt
rustup component add clippy
```

If `just` is missing:

```powershell
cargo install just
```

Optional:

```powershell
cargo install --locked cargo-nextest
```

## Standard developer loop

All commands below assume:

- shell: PowerShell
- working directory: `C:\CodexSource\codex`

### 1. Build the CLI from source

Fastest practical build for local work:

```powershell
Set-Location C:\CodexSource\codex\codex-rs
cargo build -p codex-cli
```

This should produce the local debug binary at:

- `C:\CodexSource\codex\codex-rs\target\debug\codex.exe`

### 2. Run Codex from source

From repo root:

```powershell
Set-Location C:\CodexSource\codex
just codex -- "explain this codebase to me"
```

Equivalent direct invocation from `codex-rs`:

```powershell
Set-Location C:\CodexSource\codex\codex-rs
cargo run --bin codex -- "explain this codebase to me"
```

For non-interactive checks:

```powershell
Set-Location C:\CodexSource\codex
just exec -- "summarize the current memory design"
```

### 3. Run the targeted tests

Run the narrowest crate first, based on the files touched.

Examples:

```powershell
Set-Location C:\CodexSource\codex\codex-rs
cargo test -p codex-core
cargo test -p codex-state
cargo test -p codex-app-server
```

If the change touches only docs, these are unnecessary.

If the change touches shared memory logic in `core`, `protocol`, or `state`, prefer a focused
combination of affected crates before considering anything larger.

Per repo rules:

- project-specific tests can run without asking
- full workspace `cargo test` or `just test` should only be run with explicit approval when the
  change is large enough to justify it

### 4. Format and lint

After Rust code changes:

```powershell
Set-Location C:\CodexSource\codex
just fmt
```

Then run a scoped clippy fix where practical:

```powershell
Set-Location C:\CodexSource\codex
just fix -p codex-core
```

Use the actual touched crate instead of `codex-core` whenever possible.

## Workstream-specific validation

These checks matter for the memory enhancement roadmap specifically.

### Retrieval injection validation

Expected after P0.1:

- a memory-relevant turn should include a bounded retrieved memory bundle in the developer context
- the model should not need to manually open memory files for obvious follow-up tasks

Validation approach:

1. add or update tests that inspect outbound request construction
2. run the relevant crate tests
3. if needed, add temporary trace logging during development, then remove it before finalizing

### Memory artifact validation

Expected after P1.2:

- `MEMORY.md` and `memory_summary.md` should exist when Phase 2 has inputs
- referenced rollout summary files should exist

Validation approach:

1. run memory-related tests in `codex-core` and `codex-state`
2. add a regression test for malformed consolidation output

### App-server inspection RPC validation

Expected after P0.3:

- `memory/peek` and `memory/health` style endpoints return stable structured output

Validation approach:

```powershell
Set-Location C:\CodexSource\codex\codex-rs
cargo test -p codex-app-server-protocol
cargo test -p codex-app-server
```

### Negative feedback / suppression validation

Expected after P1.1:

- `memory/suppress` should persist negative feedback into sqlite
- the next phase-2 selection should stop prioritizing the suppressed stage-1 snapshot
- suppressing a snapshot that was part of the previous phase-2 baseline should
  enqueue a fresh consolidation pass so forgetting can happen deterministically

Validation approach:

```powershell
Set-Location C:\CodexSource\codex\codex-rs
cargo test -p codex-state memory_negative_feedback
cargo test -p codex-app-server-protocol memory_suppress
cargo test -p codex-app-server --test all memory_suppress
```

## Producing a locally usable binary

For normal local use, the debug binary is enough:

- `C:\CodexSource\codex\codex-rs\target\debug\codex.exe`

If you want a release-like local binary:

```powershell
Set-Location C:\CodexSource\codex\codex-rs
cargo build -p codex-cli --release
```

Expected path:

- `C:\CodexSource\codex\codex-rs\target\release\codex.exe`

Use it directly:

```powershell
& C:\CodexSource\codex\codex-rs\target\release\codex.exe
```

If you want the repo-managed source-run path instead:

```powershell
Set-Location C:\CodexSource\codex
just codex --
```

## Logging and debugging

The upstream docs note that TUI logs go to `~/.codex/log/codex-tui.log`.

On Windows this typically means:

- `C:\Users\holylight\.codex\log\codex-tui.log`

Useful pattern:

```powershell
Get-Content C:\Users\holylight\.codex\log\codex-tui.log -Wait
```

If you need more signal for one run, set `RUST_LOG` before launching:

```powershell
$env:RUST_LOG = "codex_core=debug,codex_tui=info,codex_rmcp_client=info"
Set-Location C:\CodexSource\codex
just codex -- "debug the memory retrieval flow"
```

Clear the variable afterward if needed:

```powershell
Remove-Item Env:RUST_LOG
```

## Recommended implementation order for future sessions

When reopening this workstream, start here:

1. read `docs/memory-enhancement-roadmap.md`
2. implement P0.1 only
3. run the smallest relevant tests
4. verify `cargo build -p codex-cli`
5. only then move to P0.2

Do not start with schema migrations, vector infrastructure, or session scoring.

## Definition of done for "usable on this computer"

The memory enhancement work should be considered locally usable when all of the following are true:

1. the modified source builds locally with:
   - `cargo build -p codex-cli`
2. the produced binary launches:
   - `target\debug\codex.exe`
3. the memory feature under development is exercised by at least one automated test
4. the relevant crate tests pass
5. the operator can run Codex again from this machine without reconstructing the workflow from chat

That is the purpose of this guide: it should eliminate the need to rediscover build and run
commands in a later session.

## Current implementation checkpoint

Date:

- 2026-04-24

Status:

- P0.1 minimal implementation is complete in the local repo.
- P0.2 Phase 1 structure hardening is complete in the local repo.
- Scope held to:
  - local pre-turn memory retrieval
  - bounded developer-context injection
  - existing memory read-path instructions kept as fallback
  - stronger Phase 1 extraction structure for preference and decision capture
- Explicitly still not started in this checkpoint:
  - schema migration
  - external vector service
  - session scoring
  - P0.3 work

Current write-set:

- `codex-rs/core/src/memories/mod.rs`
- `codex-rs/core/src/memories/phase1.rs`
- `codex-rs/core/src/memories/phase1_tests.rs`
- `codex-rs/core/src/memories/prompts.rs`
- `codex-rs/core/src/memories/prompts_tests.rs`
- `codex-rs/core/src/memories/retrieval.rs`
- `codex-rs/core/src/memories/README.md`
- `codex-rs/core/src/session/turn.rs`
- `codex-rs/core/templates/memories/consolidation.md`
- `codex-rs/core/templates/memories/stage_one_system.md`
- `codex-rs/core/tests/suite/client.rs`

Design decisions captured here:

- Retrieval is turn-local and deterministic.
- The injected retrieval bundle is added only to the first outbound request of the turn.
- The retrieval bundle is not persisted into long-term conversation history.
- Source caps currently follow the roadmap guidance:
  - `memory_summary.md`: up to 1 excerpt
  - `MEMORY.md`: up to 3 excerpts
  - `rollout_summaries/*.md`: up to 2 excerpts
- Ranking is intentionally simple for P0.1:
  - keyword hits
  - path mention boosts
  - cwd-term boosts
  - light task-cue boosts
- P0.2 keeps the Phase 1 DB contract unchanged:
  - `raw_memory`
  - `rollout_summary`
  - `rollout_slug`
- P0.2 strengthens structure inside `raw_memory` task blocks instead of adding new state columns.
- Required task-level Phase 1 sections now include:
  - `Preference signals:`
  - `Decision signals:`
  - `Scope and cwd notes:`
  - `Reusable knowledge:`
  - `Failures and how to do differently:`
  - `High-value commands or paths:`
- Phase 1 now rejects non-noop outputs that do not match the required skeleton.

Verification plan for the next step:

1. run `cargo fmt --all`
2. run focused `codex-core` tests for:
   - retrieval ranking/filtering
   - request payload inclusion
   - Phase 1 structure validation
   - memory prompt template coverage
3. run `cargo build -p codex-cli`
4. verify the produced binary path:
   - `C:\CodexSource\codex\codex-rs\target\debug\codex.exe`

Verification results captured in this checkpoint:

- `cargo fmt --all`: passed
- focused retrieval unit tests: passed
- focused request-level memory bundle test: passed
- focused Phase 1 structure validation tests: passed
- focused memory prompt template tests: passed
- focused structured artifact persistence test: passed
- focused startup memory pipeline integration test for structured Phase 1 outputs: passed
- `cargo build -p codex-cli`: passed
- produced binary:
  - `C:\CodexSource\codex\codex-rs\target\debug\codex.exe`
- launch smoke check:
  - `codex.exe --version` returned `codex-cli 0.0.0`

## Active implementation checkpoint

Date:

- 2026-04-24

Status:

- P0.3 implementation has started in the local repo.
- Current target order:
  - `codex-rs/state` runtime inspection helpers
  - `codex-rs/app-server-protocol` request/response DTOs
  - `codex-rs/app-server` request handlers
  - TUI or CLI exposure only if app-server observability needs a local entrypoint beyond RPC

P0.3 scope being implemented:

- `memory/peek`
  - recent persisted stage-1 outputs selected for operator inspection
  - structured fields:
    - thread id
    - thread updated-at
    - stage-1 source-updated-at
    - cwd
    - rollout summary filename
    - short rollout summary excerpt
- `memory/health`
  - memory root existence
  - presence and basic readability of:
    - `MEMORY.md`
    - `memory_summary.md`
    - `raw_memories.md`
  - rollout summary directory scan and stale/missing reference detection
  - retrieval-source validation aligned with the current P0.1 source set:
    - `MEMORY.md`
    - `memory_summary.md`
    - `rollout_summaries/*.md`

Expected write-set for the active session:

- `codex-rs/state/src/model/memories.rs`
- `codex-rs/state/src/model/mod.rs`
- `codex-rs/state/src/lib.rs`
- `codex-rs/state/src/runtime/memories.rs`
- `codex-rs/app-server-protocol/src/protocol/common.rs`
- `codex-rs/app-server-protocol/src/protocol/v2.rs`
- `codex-rs/app-server/src/codex_message_processor.rs`
- `codex-rs/app-server/tests/suite/v2/*.rs`
- `codex-rs/app-server/README.md`
- this file

Verification plan for the active session:

1. run `cargo fmt --all`
2. run focused crate tests:
   - `cargo test -p codex-state`
   - `cargo test -p codex-app-server-protocol`
   - `cargo test -p codex-app-server memory_`
3. run `cargo build -p codex-cli`
4. if RPC shape or handler coverage changes materially, keep the app-server README updated in the same session

Remaining work before P0.3 can be called complete:

1. implement runtime inspection DTOs and queries
2. implement `memory/peek` and `memory/health` protocol surfaces
3. wire both RPC methods through `codex_message_processor`
4. add regression tests for:
   - recent peek ordering and summary truncation
   - healthy artifact case
   - missing or stale artifact case
5. complete local build and focused test verification

Completion status:

- P0.3 is complete in the local repo.
- No extra CLI or TUI entrypoint was added in this pass because the app-server RPC surface satisfied the observability acceptance criteria.

P0.3 write-set completed:

- `codex-rs/state/src/lib.rs`
- `codex-rs/state/src/model/memories.rs`
- `codex-rs/state/src/model/mod.rs`
- `codex-rs/state/src/runtime/memories.rs`
- `codex-rs/app-server-protocol/src/protocol/common.rs`
- `codex-rs/app-server-protocol/src/protocol/v2.rs`
- `codex-rs/app-server-protocol/schema/...`
- `codex-rs/app-server/src/codex_message_processor.rs`
- `codex-rs/app-server/tests/suite/v2/memory_observability.rs`
- `codex-rs/app-server/tests/suite/v2/mod.rs`
- `codex-rs/app-server/README.md`

P0.3 verification results:

- `cargo fmt --all`: passed
- `cargo test -p codex-state`: passed
- `cargo test -p codex-app-server-protocol`: passed
- `cargo test -p codex-app-server --test all memory_`: passed
- `cargo build -p codex-cli`: passed
- binary smoke check:
  - `C:\CodexSource\codex\codex-rs\target\debug\codex.exe --version`
  - returned `codex-cli 0.0.0`

## Current implementation checkpoint

Date:

- 2026-04-24

Status:

- P1.2 post-consolidation artifact validator is complete in the local repo.
- Scope held to the roadmap's minimum viable validator before any retry flow.

P1.2 write-set completed:

- `codex-rs/core/src/memories/mod.rs`
- `codex-rs/core/src/memories/phase2.rs`
- `codex-rs/core/src/memories/validate.rs`
- `codex-rs/core/src/memories/README.md`
- `docs/memory-enhancement-roadmap.md`
- `docs/memory-build-guide-windows.md`

P1.2 validator behavior:

- runs after Phase 2 agent completion and before Phase 2 success is finalized
- validates:
  - `MEMORY.md`
  - `memory_summary.md`
  - `raw_memories.md` when artifact inputs exist
  - current `rollout_summaries/*.md` materialization
  - `MEMORY.md` rollout-summary references against the materialized rollout set
- current failure mode:
  - deterministic failure report in logs
  - Phase 2 job is marked failed instead of finalized as success
  - no retry prompt / retry loop yet

P1.2 focused verification results:

- `cargo test -p codex-core --lib memories::validate::tests::validator_accepts_consistent_phase2_artifacts -- --exact`: passed
- `cargo test -p codex-core --lib memories::validate::tests::validator_reports_missing_and_inconsistent_artifacts -- --exact`: passed
- note:
  - broad `cargo test -p codex-core` filtering hit a Windows-side `BrokenPipe`
    issue while enumerating tests in this environment, so verification stayed on
    focused `--lib` tests for this pass

## Current implementation checkpoint

Date:

- 2026-04-24

Status:

- P1.1 memory undo / negative feedback loop is complete in the local repo.
- Scope held to the roadmap's minimum viable suppression pipeline.

P1.1 write-set completed:

- `codex-rs/state/migrations/0028_memory_negative_feedback.sql`
- `codex-rs/state/src/lib.rs`
- `codex-rs/state/src/model/memories.rs`
- `codex-rs/state/src/model/mod.rs`
- `codex-rs/state/src/runtime/memories.rs`
- `codex-rs/app-server-protocol/src/protocol/common.rs`
- `codex-rs/app-server-protocol/src/protocol/v2.rs`
- `codex-rs/app-server-protocol/schema/...`
- `codex-rs/app-server/src/codex_message_processor.rs`
- `codex-rs/app-server/tests/suite/v2/memory_observability.rs`
- `codex-rs/app-server/tests/suite/v2/experimental_api.rs`
- `codex-rs/app-server/README.md`
- `docs/memory-enhancement-roadmap.md`
- this file

P1.1 behavior now in repo:

- adds sqlite-backed `memory_negative_feedback`
- stores deterministic negative feedback for a persisted stage-1 snapshot with:
  - `thread_id`
  - `source_updated_at`
  - `rollout_slug`
  - `reason`
  - `created_at`
  - `updated_at`
- exposes experimental app-server RPC:
  - `memory/suppress`
- current supported reasons:
  - `transient`
  - `incorrect_inference`
  - `wrong_scope`
  - `privacy_sensitive`
  - `duplicate`
- phase-2 selection now excludes snapshots that have persisted negative feedback
- recording suppression also re-dirties the global phase-2 job so forgetting can
  happen on the next run

P1.1 focused verification results:

- `cargo fmt --all`: passed
- `cargo test -p codex-state memory_negative_feedback`: passed
- `cargo test -p codex-app-server-protocol memory_suppress`: passed
- `cargo test -p codex-app-server --test all memory_suppress`: passed

## Current implementation checkpoint

Date:

- 2026-04-26

Status:

- P1.3 stronger episodic vs durable separation is complete in the local repo.
- Scope held to retrieval policy, prompt policy, and minimal validator tightening.

P1.3 write-set completed:

- `codex-rs/core/src/memories/retrieval.rs`
- `codex-rs/core/src/memories/prompts.rs`
- `codex-rs/core/src/memories/phase2.rs`
- `codex-rs/core/src/memories/validate.rs`
- `codex-rs/core/src/memories/README.md`
- `codex-rs/core/src/memories/prompts_tests.rs`
- `codex-rs/core/tests/suite/memories.rs`
- `codex-rs/core/templates/memories/consolidation.md`
- `codex-rs/core/templates/memories/stage_one_system.md`
- `docs/memory-enhancement-roadmap.md`
- this file

P1.3 behavior now in repo:

- pre-turn retrieval now separates:
  - durable artifacts:
    - `MEMORY.md`
    - `memory_summary.md`
  - episodic evidence:
    - `rollout_summaries/*.md`
    - `raw_memories.md` as a consolidation-only episodic source
- durable memory is injected first and framed as the default operating baseline
- episodic evidence requires stronger contextual relevance and is rendered in a separate
  bundle section so it is not treated as automatically durable
- Phase 2 prompts now explicitly tell the consolidation agent:
  - what should stay in rollout summaries / raw memories
  - what is eligible for durable promotion
- validator now rejects durable artifacts that directly reference `raw_memories.md`

P1.3 focused verification results:

- `cargo fmt --all`: passed
- `cargo test -p codex-core --lib memories::retrieval::tests`: passed
- `cargo test -p codex-core --lib memories::prompts::tests`: passed
- `cargo test -p codex-core --lib memories::validate::tests`: passed
- `cargo test -p codex-core includes_memory_retrieval_bundle_in_initial_request_when_memories_match`: passed
- `cargo build -p codex-cli`: passed
- binary smoke check:
  - `C:\CodexSource\codex\codex-rs\target\debug\codex.exe --version`
  - returned `codex-cli 0.0.0`

## Current implementation checkpoint

Date:

- 2026-04-26

Status:

- P2.1 retrieval ranking upgrade is complete in the local repo.
- Scope held to transparent heuristic scoring on top of the current markdown + state DB memory design.

P2.1 write-set completed:

- `codex-rs/core/src/memories/retrieval.rs`
- `codex-rs/core/src/memories/prompts.rs`
- `codex-rs/core/src/session/turn.rs`
- `codex-rs/state/src/model/memories.rs`
- `codex-rs/state/src/model/mod.rs`
- `codex-rs/state/src/lib.rs`
- `codex-rs/state/src/runtime/memories.rs`
- `codex-rs/core/src/memories/README.md`
- `docs/memory-enhancement-roadmap.md`
- this file

P2.1 behavior now in repo:

- pre-turn retrieval still reads:
  - `memory_summary.md`
  - `MEMORY.md`
  - `rollout_summaries/*.md`
- ranking now optionally enriches rollout-summary candidates with state-backed metadata:
  - cwd
  - rollout path
  - `usage_count`
  - `last_usage`
  - negative feedback reason
- deterministic ranking now combines:
  - keyword / filename matches
  - cwd / path matches
  - task cue matches
  - durable vs episodic source bias
  - usage and recent-usage boosts
  - stale penalties by source kind
  - suppression / negative-feedback penalties
- durable ordering is now explicit:
  - `memory_summary.md` starts with a stronger durable-summary bias
  - `MEMORY.md` can overtake stale summary content when file freshness differs enough
- rollout summary ordering is now explicit:
  - matching cwd metadata and recent useful usage lift a rollout summary
  - suppressed, stale, and orphaned rollout summaries are demoted or filtered out

P2.1 focused verification results:

- `cargo fmt --all`: passed
- `cargo test -p codex-core --lib memories::retrieval::tests -- --nocapture`: passed
- `cargo test -p codex-core --lib memories::prompts::tests -- --nocapture`: passed
- `cargo test -p codex-core includes_memory_retrieval_bundle_in_initial_request_when_memories_match -- --nocapture`: passed
- `cargo build -p codex-cli`: passed

## Current implementation checkpoint

Date:

- 2026-04-26

Status:

- P2.2 session value scoring is complete in the local repo.
- Scope held to deterministic heuristic scoring that directly affects
  consolidation selection and stale-memory retention.

P2.2 write-set completed:

- `codex-rs/state/src/model/memories.rs`
- `codex-rs/state/src/model/mod.rs`
- `codex-rs/state/src/lib.rs`
- `codex-rs/state/src/runtime/memories.rs`
- `codex-rs/core/src/memories/README.md`
- `docs/memory-enhancement-roadmap.md`
- this file

P2.2 behavior now in repo:

- stage-1 outputs now receive a deterministic heuristic session-value score at
  selection time; no new DB column or migration was added in this pass
- the score combines:
  - novelty from unique Phase-1 structured bullets
  - preference / decision density
  - reusable command / path signal
  - failure-and-recovery signal
  - successful completion and observed citation signal from
    `usage_count` / `last_usage`
  - negative feedback / suppression penalty
  - duplicate-session penalty within the active candidate set
  - low-information penalty
- phase-2 input selection now ranks by session value score first, then uses
  usage / recency as deterministic tie-breakers
- exact suppressed snapshots still stay excluded from current phase-2
  selection, but suppression also participates as a scoring penalty for
  retention / pruning decisions
- stale retention pruning no longer removes only the oldest memories first:
  low-value / duplicate / suppressed stale memories are pruned before older
  but higher-value ones

P2.2 focused verification results:

- `cargo fmt --all`: passed
- `cargo test -p codex-state get_phase2_input_selection_ -- --nocapture`: passed
- `cargo test -p codex-state prune_stage1_outputs_for_retention_ -- --nocapture`: passed
- `cargo build -p codex-cli`: passed

## Final stabilization acceptance pass

Date:

- 2026-04-26

Scope:

- final stabilization only
- no new memory features added in this pass

Focused verification run for acceptance:

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

Results:

- all focused acceptance checks above passed locally
- `cargo build -p codex-cli` passed locally

Windows note for app-server tests:

- this environment can still hit a `BrokenPipe` failure when using broad filtered
  test listing such as:
  - `cargo test -p codex-app-server --test all memory_`
- when that happens, prefer exact test-name execution for the memory RPC tests
  listed above; those exact invocations passed in this acceptance pass

Milestone judgment:

- the memory enhancement stack is locally usable on this machine for the
  implemented scope
- the next sensible step is commit/cleanup/upstream preparation, not feature
  expansion
