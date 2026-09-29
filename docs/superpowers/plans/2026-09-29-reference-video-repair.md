# Video attachment and continuation repair implementation plan

> Implement inline with executing-plans and test-driven-development; no subagents.

**Goal:** accept owned video attachments, honor explicit tail-frame reference, and prevent storyboard/history text from corrupting the current specification.

**Architecture:** retain the scoped immutable upload session and existing budget identities. Route assets by verified MIME. Reuse the AI Work bounded frame extractor for uploaded MP4 bytes through an authenticated bridge endpoint; never accept arbitrary server/client paths at that endpoint.

**Tech Stack:** Rust, Axum, SQLite, PowerShell/Python upload clients.

**Spec:** user-approved repair scope in this conversation, with deployment held until the user pauses all tasks.

## Global constraints

- Local edits and isolated tests only. No deployment, push, live restart, launcher change, paid generation, or production accounting changes.
- Preserve unrelated AI Work BitBrowser changes and active work.
- Keep API-key ownership, upload expiry, immutable slots, size limits and request idempotency.
- Explicit tail-frame extraction must not silently substitute whole-video reference or native extension.

## Review focus

- Mixed images/video and extension/content mismatches: reject mismatches before any paid work.
- Latest user_input versus system-reminder/history: honor the current turn without inventing spec fields.
- Timeline 0–2/2–4/4–7/7–10: infer 10 seconds only from a coherent timeline, never 2.
- Frame failures/cross-key references: fail closed with useful errors and no video submission.
- Retried upload/tool/stream rounds: preserve one operation; never globally dedupe independent client tasks.

## Task 1: typed attachments

Files: reference_upload.rs, reference_upload.ps1/.sh, tests/support/reference_upload_cases.rs.
Interface: original encrypted paths define slot kind; complete receipts restore image_asset_ids and video_asset_ids separately.
- [x] Add MP4/mixed/mismatch/replay tests; run to confirm failure.
- [x] Allow MP4/WebM upload with format checks, generic asset wording and typed restoration.
- [x] Run the reference-upload integration tests including actual PS5/PS7 execution.

## Task 2: current intent and timing

Files: work_planner.rs, tests/work_planner.rs, work_continuation.rs.
Interface: current_text selects current user_input outside system-reminder; requested mode honors explicit tail intent.
- [x] Add storyboard, explicit duration, wrapped history and tail-mode tests; observe failure.
- [x] Parse coherent timelines and current fields; reject unsupported explicit specs.
- [x] Run planner and continuation regression tests.

## Task 3: uploaded-video tail extraction

Files: Core bridge_client.rs, work_execution.rs; AI Work bridge_v2_api.rs/server.rs/video_frames.rs and relevant tests.
Interface: authenticated POST /internal/bridge/v2/reference-last-frame accepts bounded MP4 bytes; response PNG includes source/frame hashes and dimensions. Core verifies bytes against owned input and substitutes only the extracted image in the paid payload.
- [x] Add real extractor + bridge route tests, plus Core owned-media dispatch/failure tests; observe failure.
- [x] Reuse the bounded extractor with private temporary source and automatic cleanup; implement typed verified response.
- [x] Run targeted suites; inspect helper replay and terminal failures without production mutation.

## Task 4: audit and handoff

- [x] Run proportionate broader suites in isolated data/temp directories and inspect git diff.
- [x] Record test evidence and any remaining limitations.
- [x] Stop before deployment; notify user to pause tasks and await confirmation.
