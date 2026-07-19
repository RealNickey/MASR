# MASR Architecture Audit & Download Fix Plan

## Skills Installed

| Skill                          | Package                                     | Reason                                                     |
| ------------------------------ | ------------------------------------------- | ---------------------------------------------------------- |
| rust-async-patterns            | `wshobson/agents@rust-async-patterns`       | Rust async correctness, `spawn_blocking`, connection reuse |
| rust-best-practices            | `apollographql/skills@rust-best-practices`  | Rust idioms, Mutex poisoning, lock ordering                |
| vercel-react-best-practices    | `vercel-labs/agent-skills`                  | React/TS render perf, store batching                       |
| performance                    | `addyosmani/web-quality-skills@performance` | Web/runtime perf profile                                   |
| react-performance-optimization | `nickcrew/claude-ctx-plugin`                | React state update minimization                            |

---

## Architecture Audit Summary

### What's working well

- Clean Tauri v2 multi-window setup (primary / settings / overlay / prompt)
- Strong manager pattern with `Arc`-wrapped shared state
- Proper RAII cleanup via `DownloadCleanup`
- SHA256 verification correctly offloaded to `spawn_blocking`
- Progress throttling to 10 events/sec on the backend

### Critical Issues Found (6)

---

## CRITICAL ISSUE 1: Unbuffered, no-BufWriter file writes in download path

**Files:** `src-tauri/src/managers/model.rs:1128–1169`  
**Impact:** ~400K syscalls per 1GB file. Measurable download slowdown.  
**Fix:** Wrap file handle in `BufWriter::with_capacity(256KB)`. Flush once at end instead of per-chunk `write_all`.

---

## CRITICAL ISSUE 2: Per-download `reqwest::Client::new()` — no connection reuse

**File:** `src-tauri/src/managers/model.rs:1080`  
**Impact:** Each download/retry spins up a fresh DNS resolver, connection pool, TLS session. Resume retries and sequential model fetches waste ~50–200ms each on TLS handshakes. 6+ independent clients across the codebase.  
**Fix:** Add a single `reqwest::Client` field to `ModelManager` (created in `new()`). For the rest of the codebase, extract a shared client builder in `lib.rs` or a new `utils.rs`.

---

## CRITICAL ISSUE 3: No HTTP timeouts anywhere

**Files:** `model.rs` (all HTTP calls), `llm_client.rs:96`, `google_api.rs`, `google_oauth.rs`  
**Impact:** A stalled or very slow CDN response blocks Tokio tasks forever. This is the most likely cause of "downloads getting stopped/delayed" — if any single chunk stalls, there is no read timeout to recover and retry.  
**Fix:** Add `.timeout(Duration::from_secs(30))` (or 60s for large model chunks) to all HTTP clients. Also add `.connect_timeout(Duration::from_secs(10))`.

---

## CRITICAL ISSUE 4: No `User-Agent` on model download requests

**File:** `src-tauri/src/managers/model.rs:1080`  
**Impact:** Default reqwest UA is `reqwest/0.12.x`. The CDN (`blob.handy.computer`) and GitHub Releases may throttle, rate-limit, or drop requests from unknown user agents. This is a very likely cause of intermittent download failures and "exponential backoff" behavior from the CDN side.  
**Fix:** Set header `User-Agent: ThegAi/1.0` on the model download client (consistent with `llm_client.rs:69`).

---

## CRITICAL ISSUE 5: Sequential auto-download with unbounded retry loop

**File:** `src-tauri/src/lib.rs:198–239`  
**Impact:** The 2 models are downloaded one-after-another (by design, conservatively). Both retry forever with flat 5-second backoff, no matter the error. If the server is unreachable, the tasks run for the lifetime of the process — holding `Arc<ModelManager>` and generating repeated errors. Combined with missing timeouts (#3), a stalled connection can block the retry loop, causing the second model to never even start.  
**Fix:**

- Add a max retry count (e.g., 10 retries) then stop and surface a status event to the frontend.
- Add exponential backoff: 5s → 10s → 20s, capped at 60s.
- Add a cancellation token or graceful shutdown on app exit.
- Fire a `model-auto-download-status` event so the frontend can show "retrying..." vs "failed".

---

## CRITICAL ISSUE 6: Parallel download of the same model not prevented (TOCTOU race)

**File:** `src-tauri/src/managers/model.rs:1019–1061`  
**Impact:** `download_model` reads `is_downloading` then later sets it without a check-and-set. Two concurrent calls (auto-download + manual) can both start downloading the same file, corrupting the `.partial` and causing SHA256 failure — which deletes the partial, forcing a restart from zero.  
**Fix:** Move the `is_downloading` check earlier, under the mutex, returning early if true. Add a per-model `Mutex<Option<()>>` or ` parking_lot::Mutex<()>` field as a concurrent-download guard.

---

## HIGH-PRIORITY ISSUES

### Extraction blocks the async runtime

**File:** `model.rs:1275–1300`  
Tar.gz extraction is fully synchronous inside an async function. For large models, this freezes the Tauri runtime / UI.  
**Fix:** Wrap extraction block in `tokio::task::spawn_blocking`.

### Missing `verifyingModels` in `useMemo` deps

**File:** `src/components/settings/models/ModelsSettings.tsx:196`  
Verifying models render in the wrong section.

### Global status checks in ModelSelector

**File:** `src/stores/modelStore.ts:238–243`  
Uses `Object.keys(verifyingModels).length > 0` (global) instead of per-model check. All models show "verifying" when any model is verifying.

### Frontend progress state reset on concurrent auto+manual download

**File:** `src/stores/modelStore.ts:168–181`  
`downloadModel` action unconditionally resets `downloadProgress` to 0. If already auto-downloading, the progress bar jumps backward.

### Frontend double `set()` per progress event

**File:** `src/stores/modelStore.ts:283–326`  
Each backend event fires two `set(produce(...))` calls (progress + stats). Backend throttles to 10/sec, so frontend updates 20 times/sec per model.  
**Fix:** Batch both updates into a single `set()` call.

### Speed calculation uses stale `totalDownloaded`

**File:** `src/stores/modelStore.ts:292–324`  
Events with `timeDiff <= 0.5` are dropped for speed calc, but `totalDownloaded` is not updated. The next qualifying event computes `bytesDiff` over the entire gap, producing speed spikes.

---

## MEDIUM-PRIORITY ISSUES

### Lock ordering inversion → deadlock risk

**Files:** `model.rs:74–87` (`DownloadCleanup::drop`) vs `model.rs:1480–1510` (`cancel_download`)  
`DownloadCleanup` locks `available_models` → `cancel_flags`. `cancel_download` locks `cancel_flags` → `available_models`.  
**Fix:** Standardize lock ordering (e.g., always lock `available_models` first).

### `wait_for_download` busy-polls every 1s

**File:** `transcription.rs:454–493`  
Uses `std::thread::sleep` in a polling loop. The `loading_condvar` already exists but is not used here.  
**Fix:** Replace with Condvar-based wait to eliminate the 1-second wakeup latency.

### `cancel_download` doesn't prevent auto-download restart

**File:** `model.rs:1498` vs `lib.rs:212–214`  
User cancels → `is_downloading = false` → auto-download loop restarts it after 5s.  
**Fix:** Add a `user_cancelled: HashSet<String>` field; auto-download loop checks and skips cancelled models.

### No user-facing "auto-download in progress" status

**Fix:** Emit a Tauri event on auto-download start/retry/complete so the frontend can show a subtle indicator without blocking the UI.

### Progress space bug

**File:** `ProgressBar.tsx:56` — `"1.5MB/s"` → `"1.5 MB/s"`

---

## LOW-PRIORITY / LATENT

### `has_any_models_or_downloads` ignores `is_downloading`

**File:** `commands/models.rs:222–228` — name says "or_downloads" but only checks `is_downloaded`.

### `get_settings`/`write_settings` have no lock

**File:** `settings.rs:1023–1053`  
Concurrent `read-modify-write` from two Tauri command handlers can silently lose updates. Fix with a write lock or compare-and-swap.

### Mutex poisoning on panic

**Files:** `model.rs` — all `.unwrap()` on `Mutex::lock()`  
Any panic while holding the lock propagates poison to all waiters, aborting the process.  
**Fix:** Use `.ok()?` or `map_err` to handle poisoned state gracefully.

### html in `moonshine-small-streaming-en` URL

**File:** `model.rs` line for `moonshine-small-streaming-en`  
URL reads `https://blob.handy.comcomputer/...` (missing dot). Confirm if this is a typo.

---

## Recommended Implementation Order

### Phase 1 — Download reliability (fixes the slow/stopping/failing issue)

1. Add shared `reqwest::Client` with `timeout`, `connect_timeout`, `User-Agent` to `ModelManager`
2. Wrap file write in `BufWriter`
3. Wrap extraction in `spawn_blocking`
4. Fix `has_any_models_or_downloads` and per-model download guard
5. Fix `cancel_download` + auto-download interaction

### Phase 2 — Speed and UX

6. Add retry budget + exponential backoff to auto-download loop
7. Batch frontend `set()` calls in `modelStore.ts`
8. Fix global status checks in `ModelSelector.tsx`
9. Fix `downloadModel` action to not reset progress on concurrent download
10. Fix speed calculation stale `totalDownloaded`

### Phase 3 — Correctness hardening

11. Standardize Mutex lock ordering in `model.rs`
12. Replace `wait_for_download` polling with Condvar wait
13. Replace settings `unwrap()` mutex locks with poisoned-state handling
14. Add settings write lock / CAS
15. Fix `ModelsSettings.tsx` `useMemo` deps
16. Verify `moonshine-small-streaming-en` URL typo
