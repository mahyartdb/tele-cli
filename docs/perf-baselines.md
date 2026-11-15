# Performance baselines and budgets-as-protocol

No numeric SLOs are set here. Per the budgets-as-protocol decision, every
optimisation ships before/after numbers on the named curves below with a
no-regression gate against the recorded baseline; numeric SLOs get proposed
only after the first measured baselines exist.

## Measured offline floor

Measured 2026-09-19 on the maintainer Windows box, debug profile, deps-cached:

- `tele --help`: ~72 ms (process start + clap parse, no config, no network).
- `tele doctor` on an empty isolated `TELE_APP_DIR`: ~85 ms (start + app-dir
  resolve + local checks, no network).
- Together these bound the fixed per-invocation floor: short-command wall
  time above ~0.1 s is handshake + RPC, not startup.
- Debug binary `target/debug/tele.exe`: 44,486,144 bytes (~42.4 MiB).
- Single-crate incremental rebuild (`cargo build`, deps cached): ~45 s.

## Curve protocols + no-regression gates

1. Pagination time-vs-limit (100/1k/10k dialogs, deep history), limiter
   disabled vs enabled. Live spot-check. Gate: no curve point regresses vs
   the recorded baseline at the same limit.
2. Fan-out `--parallel` 1/4/16/32 on dry-run workloads. Offline-capable
   (dry-run returns before connect). Gate: semaphore/limiter overhead within
   noise vs baseline sweep.
3. Cache bulk-insert across batch sizes; FTS5 query latency at 10k/100k rows.
   Design fact: sync writes are already a single transaction per batch
   (`BEGIN IMMEDIATE`/`COMMIT` in `cache_db.rs`), page size 100. Gate: batch
   trial records time-vs-size before any batch-size change lands.
4. 10k-row table vs JSON render with `--fields` cost broken out. Gate:
   render time recorded before any output-path change lands.
5. Per-target binary sizes across the 13-target matrix + single npm bundle.
   Local debug size above is the developer proxy; release sizes are
   CI-measured per `docs/release.md`. Gate: size table before/after any
   feature-trim or codegen proposal.
6. `cargo build --timings` for `codegen-units`/feature proposals. Local
   single-crate rebuild (~45 s, deps cached) is the developer proxy;
   full-fresh timing is CI-measured. Gate: timings before/after.
7. Connect/handshake share of short commands: floor above vs live wall time
   (manual spot check). Gate: any session-reuse proposal argues against this
   floor with measured handshake numbers.
8. Stream backpressure past the 1000-update queue (`update_queue_limit`
   + `catch_up` in `client.rs`): live-flood spot check. Gate: behaviour
   documented before any queue/lane change lands.

## Landed optimisations (2026-11)

Recorded here so the next curve run has a before/after reference. RPC
and scan counts are offline-verifiable; the wall-clock numbers still
need a live account, and the live spot-checks are named per row.

| Change | Curve | Before | After |
|---|---|---|---|
| `msg search` client-side-filter scan bound (`src/commands/msg/mod.rs`) | 1 | unbounded: the scan only ended at `--limit` matches or history exhaustion, so `--from` over a chat where that sender rarely wrote walked the whole history (100-message pages, one RPC per page) | bounded at `search_scan_budget` = 10x `--limit` (floor 100) rows examined; `truncated` reports the cut, and human mode warns |
| `msg get --watch` poll tick (`src/commands/msg/mod.rs`) | 1 | ~4 RPCs per tick (peer resolve x2 - a real `contacts.ResolveUsername` for `@username` targets - plus one fetch per probe) and 2 session-DB peer lookups | 1 RPC per tick for a recent target (one bounded page carries both the newest message and the watched one), 2 for an older target; the peer is resolved once before the loop |
| `serve` resync dispatch (`src/commands/serve.rs`) | 2, 8 | `tx.send(()).await` on a 1-slot channel whose receiver is only polled inside the account event loop: a second resync while an account reconnects or waits out a flood stalled the sequential dispatcher | `try_send`: a full slot already means a resync is pending, so the duplicate is dropped instead of blocking the dispatcher |
| `serve` catch-up flag consumption (`src/commands/serve.rs`) | 8 | the flag was consumed after `stream_updates` built the stream, so a resync/error rebuild ran live-only and the outage window was lost | the flag is taken before the stream is built, so the rebuild replays from the persisted pts |
| `listen` album flush on stop (`src/commands/listen.rs`) | 8 | pending album members were dropped on every `--count`/`--until`/broken-pipe stop (neither an `Album` row nor `NewMessage` rows) | flushed before the stop returns, on every stop site |
| `chat participants` limiter cadence (`src/commands/chat/participants.rs`, `src/commands/chat/mod.rs`) | 1 | one limiter token per participant ROW, while grammers fetches participants 100 per page — a 500-member group spent 500 tokens (~8 min of limiter wait at 60 rpc/min) for ~5 round trips | one token per PAGE, via `acquire_for_items` |
| `dialog list` folder filter (`src/commands/dialog.rs`) | 1 | the default listing walked the server's ordering of ALL folders and dropped non-main rows locally, so archived dialogs consumed page slots and a listing could return 0 rows while main-folder dialogs existed | folder 0 is requested from the server (`messages.getDialogs` with `folder_id`), same rows, no wasted slots |
| `chat admin-log` window (`src/commands/chat/admin_log.rs`) | 1 | with only `--until` given, the scan walks back through newer events until it fills the limit or runs out — that is the semantics of the window (events arrive newest-first, so the window is always further back), and the repo's own test locks it | unchanged |
| serve account-to-main event hop (`src/commands/serve.rs`) | 8 | the hop is bounded (256 slots) but the wait was unbounded: a stalled consumer parked the account task while grammers' UNBOUNDED update channel kept filling — the only unbounded-memory path in the long-running lanes | the wait is capped at 30s; a dropped event is counted and reported as a `Gap` row (`consumer_stalled`), so memory stays bounded and the loss is visible |

Live spot-checks still owed: curve 1 (pagination/search time-vs-limit on
a live account), curve 2 (fan-out `--parallel` sweep, to confirm the
dispatcher change costs nothing), curve 8 (resync replay after a
simulated outage).

## Known limits with no curve yet

- **Bulk media download is strictly sequential** (`src/commands/msg/download.rs`):
  one file at a time, and grammers keeps exactly one `upload.getFile` in
  flight, so a 100 MB file is ~200 serialized 512 KB chunks. Pipelining
  would need raw `upload.getFile` calls (offset-parallel), which is a
  grammers-level change. No curve covers download throughput yet: add
  one and record the baseline before attempting it.
- **Album uploads bypass the per-account token bucket**
  (`src/commands/msg/send.rs`): `send_core` takes one token and the
  `SaveBigFilePart`/`SaveFilePart` RPCs that follow are invisible to the
  limiter, so a large upload spends far more server calls than its
  budget allows. Documented here rather than changed, because counting
  upload RPCs would change the limiter's meaning (ADR-008).

## First-cut scope notes

- FTS5: the `messages_au` update trigger closed the open "no update trigger"
  question from the baseline audit. Before: a raw UPDATE bypassed the FTS
  index and left stale docs; after: the trigger deletes + reinserts the
  index row (proven by `update_keeps_fts_index_in_sync`). The hot path is
  untouched by construction — sync writes go through `INSERT OR REPLACE`
  inside the existing single transaction, which never fires the UPDATE
  trigger — so no batch-size regression risk.
- Output streaming vs buffering: buffered envelopes stay. Fan-out must join
  all accounts before printing one envelope (contract), so streaming would
  break the machine shape; accepted as designed pending curve-4 numbers.
- Feature-trim/build: no trim shipped. The `Cargo.toml` features-audit
  comment records that further trimming risks build breaks for unmeasured
  gain; `cargo deny check` is green. Trimming stays out until curve 5/6
  numbers justify it.
