---
title: Agent Panel Performance
description: "Profile of the ACP streaming/render hot path, measured findings, and remaining optimization opportunities."
---

# Agent Panel Performance

This page documents an investigation into the complaint that external agents
(Codex, Claude Code, and other ACP servers) feel slow while streaming in Zed,
the fix that landed, and the remaining opportunities, ranked.

## The streaming hot path

Every streamed token travels this path:

1. `agent_servers::acp::handle_session_notification` receives an
   `acp::SessionUpdate` from the agent process.
2. `AcpThread::handle_session_update` (`crates/acp_thread/src/acp_thread.rs`)
   routes it. Text chunks for an existing Markdown block go into a
   `StreamingTextBuffer`, which a 16 ms timer task drains into the target
   `Markdown` entity for smooth reveal.
3. `AcpThread` emits `AcpThreadEvent`s. Subscribers in `agent_ui`:
   - `ConversationView::handle_thread_event`: on `EntryUpdated` it runs
     `EntryViewState::sync_entry`, `ListState::remeasure_items` (rebuilds the
     list's item SumTree), elicitation sync, thought auto-expand, and the
     generating-indicator sync.
   - `AgentDiff::handle_acp_thread_event`: scans the entry for diffs.
   - `ThreadSearchBar` (when active): reschedules a full-thread search.
4. `Markdown::append` schedules a background re-parse of the entire source;
   completion calls `cx.notify()` and `cx.refresh_windows()`.

## Methodology

- Read the full path above and classified per-chunk work as O(1), O(entry), or
  O(thread).
- Added deterministic instrumented tests in
  `crates/acp_thread/src/acp_thread.rs` that stream chunks through the real
  `handle_session_update` path on GPUI's test executor, counting emitted
  events and timing the dispatch loop:
  - `test_streaming_text_coalesces_entry_updated_events` streams 1000 text
    chunks at 1 chunk/ms of virtual time, then drains the reveal buffer.
  - `test_flush_streaming_text_emits_entry_updated` pins the event sequence
    around buffering and flushing.

Event counts on the virtual clock are fully deterministic, so before/after
ratios are exact rather than noisy wall-time samples.

## Findings

**`EntryUpdated` was emitted once per network chunk, and at the wrong time.**
`push_assistant_content_block_with_message_id` emitted
`AcpThreadEvent::EntryUpdated` for every incoming text chunk *before* placing
the text in the streaming buffer. At that moment the entry's visible content
had not changed — the text becomes visible only when the 16 ms reveal tick
appends it to the `Markdown` entity, and the reveal tick emitted nothing.
Consequences:

- Chunk rates are unbounded (Codex can deliver hundreds of chunks/second), so
  every subscriber in `agent_ui` ran its `EntryUpdated` work per chunk:
  `sync_entry`, a `remeasure_items` SumTree rebuild, the diff scan, and three
  view syncs — all for content that had not changed yet.
- The final flush (`flush_streaming_text`, run at turn end, on cancel, or when
  a new entry is pushed) appended buffered text *without* emitting
  `EntryUpdated`, so nothing told views to remeasure the last appended text.
  This was masked by the next per-chunk emission when one arrived.

**Measured baseline:** streaming 1000 chunks produced **999 `EntryUpdated`
emissions** (one per chunk after the first), each one a full subscriber
fanout.

**Secondary finding:** `ContentBlock::update_text_in_place` (the tool-call
output streaming path — Codex streams tool output as repeated full-content
snapshots) copied the entire current Markdown source to a fresh `String` on
every snapshot just to run a prefix comparison: an O(len) allocation per
update, O(n²) cumulative over a long tool output.

## Fix landed

`crates/acp_thread/src/acp_thread.rs`:

- `StreamingTextBuffer` now records the index of the entry it streams into.
- The per-chunk `EntryUpdated` emission was removed from the buffered text
  path. Instead, the reveal tick emits `EntryUpdated` after each append, and
  `flush_streaming_text` emits it when it appends pending text (fixing the
  missing-remeasure gap at turn end).
- `update_text_in_place` compares the incoming snapshot against the Markdown
  source in place instead of copying it out first.

Event fanout is now bounded by the reveal tick rate (at most one per 16 ms per
thread) instead of the network chunk rate, and it fires exactly when content
actually changes.

**Measured result** (`test_streaming_text_coalesces_entry_updated_events`,
1000 chunks over 2 s of virtual time):

| Metric | Before | After |
| --- | --- | --- |
| `EntryUpdated` emissions | 999 | 75 (**13.3x fewer**) |
| Dispatch loop wall time (no UI subscribers attached) | 6.98 ms | 5.37 ms (−23%) |

The wall-time delta above excludes subscriber work; in the app each avoided
event also skips the `ConversationView`/`AgentDiff` handler work in
`agent_ui`, so the end-to-end saving is larger and scales with chunk rate.

## Remaining opportunities, ranked

1. **Incremental Markdown parsing** (`crates/markdown/src/markdown.rs`,
   `Markdown::append` → `parse`). Every append re-parses the entire source on
   a background thread (coalesced via `pending_parse`/`should_reparse`, but
   still O(total) per parse → O(n²) cumulative over one long message), and
   `append` itself does `self.source.to_string() + text`, an O(total)
   main-thread copy per 16 ms tick. An append-aware parse that reuses events
   for the unchanged prefix blocks is the largest algorithmic win left.
2. **`cx.refresh_windows()` on every parse completion**
   (`crates/markdown/src/markdown.rs`, end of `start_background_parse`). Each
   completed parse refreshes *every window*, invalidating all views up to ~60
   times/second during streaming, instead of only notifying views that render
   that Markdown entity.
3. **Idle reveal-task ticks** (`AcpThread::start_streaming_reveal`). The 16 ms
   timer keeps ticking (entity update per tick) while `pending` is empty,
   until the next flush drops the buffer. Bounded by turn structure, but it
   wakes the main thread at 60 Hz between message chunks for no work.
4. **Per-event `remeasure_items`**
   (`ConversationView::handle_thread_event`). Each `EntryUpdated` rebuilds the
   list item SumTree. Now bounded by the tick rate; could be batched to once
   per frame with a dirty-index set if it still shows up in profiles.
5. **`index_for_tool_call` linear scan** (`acp_thread.rs`). Reverse scan over
   all entries per `ToolCallUpdate`. Fine while updates target the tail, but
   O(entries) when agents interleave many concurrent tool calls in long
   threads.

## Regression guard

`test_streaming_text_coalesces_entry_updated_events` asserts the emission
count stays below one quarter of the chunk count, so a reintroduced
per-chunk emission fails CI. `test_flush_streaming_text_emits_entry_updated`
pins the flush-time emission that keeps view measurements correct.
