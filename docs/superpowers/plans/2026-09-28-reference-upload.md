# Reference upload implementation plan

> **For agentic workers:** Use superpowers:executing-plans. No subagents, per user direction.

**Goal:** API-only Agent clients can submit local reference images before Seedance generation.

**Architecture:** Validated terminal adapter plus a durable encrypted pre-upload handoff. Short-lived upload authorization is isolated from billing and ordinary API authentication; paid work resumes from the stored original request only after every attachment arrives.

**Tech Stack:** Rust, Axum, SQLite, PowerShell 5/7, Bash.

**Spec:** `docs/superpowers/specs/2026-09-28-reference-upload.md`

## Global Constraints

- No subagents; preserve missing-reference rejection and unrelated work.
- At most 10 image attachments, 32 MiB each, 30-minute handoff; no long-lived keys in tool arguments.
- Same upload slot is immutable; retries use one generation idempotency key.
- Task-owned temporary files on D; reuse build caches on E; cleanup after tests.

## Review Focus

- Paths with Unicode, quotes, shell metacharacters: literal file reads, no command injection.
- Two clients sharing one Key: separate attachments and original prompts.
- Tampered/expired/revoked capabilities: no files stored or paid calls.
- Tool output claiming success before upload: server-side receipt required.
- Interrupted or repeated upload/follow-up: no partial-reference generation or duplicate payment.

### Task 1: Reference upload handoff

**Files:** create `src-core/src/reference_upload.rs`, `starlink-dimension-router/src/reference_upload.rs`, `reference_upload.ps1`, `reference_upload.sh`; modify schema/store/lib, `budget_flow.rs`, `server.rs`; tests in `tests/budget_video_admission.rs` and new module tests.

**Interfaces:** `reference_upload::before_chat(state, principal, headers, body)` returns an optional response or resumes the authenticated original body/headers; `reference_upload::upload` handles the scoped upload route. Store methods save/read encrypted handoffs and atomically bind immutable asset slots.

- [x] Add regression test using TRAE uploaded_files + RunCommand; expect tool_calls and zero bridge sends/active execution slots.
- [x] Run targeted cargo test; expect current reference_image_missing failure.
- [x] Implement persistent handoff, short-lived upload authorization, literal-path scripts, original-request restoration before download follow-up handling.
- [x] Extend tests for all review-focus cases and real generated command execution; expect original bytes and ownership isolation.
- [x] Run router and core cargo test suites offline/locked; expect all passing.
- [x] Review diff and commit only scoped code/tests/design.

### Task 2: Public acceptance and release

**Files:** release evidence document; task-owned deployment/acceptance utilities outside repo.

**Interfaces:** Task 1's public upload route and Chat tool-call protocol, existing deployment service and paid bridge.

- [x] Self-audit authorization, idempotency, bounded retention, original prompt/parameters and no secrets in output.
- [x] Build Linux binary; inspect active work before atomic release; verify health/hash.
- [x] Run one real public API reference test through generated terminal command; check asset hash, actual upstream image reference, completed video and billing.
- [x] Push verified Core changes to its existing GitHub branch; document evidence and limits.
- [ ] Clean local task-owned temporary artifacts: attempted cleanup was blocked by the execution environment; files retained, not retried through a bypass. Remote temporary deployment script removed; acceptance video, evidence and rollback backups retained.
