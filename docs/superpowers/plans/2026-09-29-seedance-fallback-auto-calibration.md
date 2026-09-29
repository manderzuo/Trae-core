# Seedance fallback and failure persistence implementation

Approved design: ../specs/2026-09-29-seedance-fallback-auto-calibration-design.md

Execute inline in the existing two worktrees; no subagents. Preserve account routing, historical money facts and the paused monitor. Do not activate an unverified price or deploy over live work.

## Task 1 — AI Work quote coverage and explicit fallback

Add failing tests for an enabled 15s/720p fallback covering valid missing reference profiles, disabled/expired/invalid fallback, unsupported specs, covering-reference neighbors, and priority of observed calibration. Implement in bridge_planner. Configuration stores final hold_microcredits, not a price ceiling. Preserve the actual pricing profile and upstream payload. Retain receipt-backed, account-scoped learning.

## Task 2 — Core first-failure persistence

Add failing status-query regression and atomic first-failure tests. Extend execution completion with a bounded RequestResult without changing the existing wrapper. Query persisted reasons only after existing ownership checks; whitelist public error feedback. Preserve legacy missing reasons and financial reservations.

## Task 3 — Verification and release preparation

Run targeted RED/GREEN checks and relevant full Rust/frontend suites using the shared E: cache. Independently review the complete diff without an agent. Obtain an auditable production baseline before enabling fallback. Record any deployment or paid-test limitations explicitly; no inferred historical cause.

## Review focus

Money overflow, ambiguity, malformed/expired configuration, wrong Key/account isolation, duplicate receipt and error idempotency, unchanged execution parameters, late settlement and no blanket balance freeze. A fallback is a temporary risk allowance, not a verified maximum bill.
