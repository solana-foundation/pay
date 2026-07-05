# `pay serve inference` — Design & Implementation Plan

Status: **draft / planning**
Branch: `feat/serve-inference`
Owner: @ludo

---

## 1. Overview

`pay serve inference` turns the machine's local AI stack into a supervised, observable
(and eventually monetizable) HTTP gateway:

1. **Discover** the local inference servers that are actually running (Ollama, LM Studio,
   llama.cpp, vLLM, exo — top 5, extensible registry).
2. **Proxy** them through the existing Pingora data-plane (`pay-proxy::Http402Gate`),
   one synthesized `ApiSpec` per discovered provider, streaming (SSE) preserved.
3. **Observe** every request live in:
   - a new **TUI** (left pane / content window, same visual language as the topup TUI), and
   - the **web UI** (the PDB app, renamed to `web-ui`, with a new "Pay Inference" view).
4. **Later**: attach token-based metering (the `unit: tokens` dimension already exists in
   the `ApiSpec` schema) so a local rig can sell inference over MPP/x402 with zero extra code.

The unifying idea: **we already have the whole pipeline** — YAML spec → `AppState` →
axum control-plane → Pingora gate → `record_exchange()` → PDB correlation →
`broadcast::Sender<SseMessage>` → SSE → React UI. This feature is mostly:
(a) synthesizing specs from discovery instead of a YAML file, (b) generalizing PDB events
beyond 402 flows, and (c) rendering the same event stream in a TUI.

### Non-goals (v1)

- Payment gating of local providers by default (v1 proxies **free/passthrough**; monetization is Phase 6).
- Remote/cluster discovery (localhost only; exo's own cluster handles multi-node).
- Model management (pull/load/unload) — we surface model lists read-only.
- Persistence of request history (PDB stays in-memory, 200-flow ring buffer).

---

## 2. UX

### 2.1 Command

The CLI today exposes `pay server {demo|start|scaffold|plans}`
(`rust/crates/cli/src/commands/mod.rs:34-100`, `commands/server/mod.rs:9-30`).

- Add `Inference(inference::InferenceCommand)` to `ServerCommand`.
- Add `alias = "serve"` on the top-level `Server` variant so **both**
  `pay serve inference` and `pay server inference` work (keeps the noun-form group intact,
  gives us the verb form asked for).

```
pay serve inference [FLAGS]

FLAGS:
  --bind <ADDR>            Public bind for the gateway        [default: 127.0.0.1:1402]
  --providers <LIST>       Only probe these (comma-sep slugs) [default: all known]
  --probe-timeout <MS>     Per-endpoint probe timeout         [default: 400]
  --no-tui                 Headless: log lines instead of TUI (implied when !isatty)
  --no-web                 Don't mount the web UI
  --watch                  Keep re-probing for providers that appear/disappear [default: on, 10s]
  --spec <PATH>            Merge an extra hand-written ApiSpec YAML (escape hatch)
  --public-url <URL>       Same semantics as `server start`
```

No `--recipient/--currency/network` in v1 — no payments, no signer, no RPC needed.
(Phase 6 adds `--monetize` which reuses the full `StartCommand` operator resolution.)

### 2.2 What the user sees

```
$ pay serve inference
⏺ probing local AI providers… ollama ✓ (11434, 3 models)  lm-studio ✓ (1234, 1 model)
⏺ gateway on http://127.0.0.1:1402  ·  web UI http://127.0.0.1:1402/__402/pdb
[TUI opens]
```

Requests are routed by **subdomain** (already supported by the gate,
`core/src/server/gate.rs:178-187`): `http://ollama.localhost:1402/v1/chat/completions`,
`http://lm-studio.localhost:1402/v1/models`. `*.localhost` resolves to loopback on
macOS/Linux/modern resolvers, so this needs zero config. The bare host
(`127.0.0.1:1402`) serves the web UI + a JSON index of discovered providers
(single-API fallback stays for the one-provider case: bare-host requests route to it).

---

## 3. Provider discovery

### 3.1 Registry (top 5)

Embedded data (`include_str!("providers.yml")` in the new module — same pattern as
`payment-debugger.yml` in `demo.rs`), merged with an optional user override at
`~/.config/pay/inference-providers.yml`.

| slug        | default port(s) | identify probe                          | positive match                             | models list          | API surface proxied                                             |
|-------------|-----------------|------------------------------------------|--------------------------------------------|----------------------|-----------------------------------------------------------------|
| `ollama`    | 11434           | `GET /api/version`                       | JSON body has `version`; `GET /` body `"Ollama is running"` | `GET /api/tags`      | native `/api/{chat,generate,embed,tags,show}` + OpenAI `/v1/*`  |
| `lm-studio` | 1234            | `GET /api/v0/models`, fallback `GET /v1/models` | `/api/v0` responds (LM Studio-only REST) or `Server` header | `GET /api/v0/models` | `/v1/{chat/completions,completions,embeddings,models}` + `/api/v0/*` |
| `llama-cpp` | 8080            | `GET /props`                             | JSON body has `default_generation_settings` | `GET /v1/models`     | `/v1/*`, `/completion`, `/infill`, `/embedding`, `/health`      |
| `vllm`      | 8000            | `GET /version`                           | JSON body has `version` AND `GET /v1/models` OK | `GET /v1/models`     | `/v1/{chat/completions,completions,embeddings,models}`          |
| `exo`       | 52415           | `GET /v1/models` (dashboard on same port) | responds + not matched by a higher-priority probe | `GET /v1/models`     | `/v1/chat/completions`, dashboard passthrough                   |

Registry entry schema (YAML → `ProviderSpec` struct):

```yaml
providers:
  - slug: ollama
    title: Ollama
    ports: [11434]
    identify:
      - { path: /api/version, expect_json_key: version }
      - { path: /,            expect_body_contains: "Ollama is running" }
    models: { path: /api/tags, json_pointer: /models, name_key: name }
    openai_compat: true          # exposes /v1/* — drives token/usage extraction
    color: "#22c55e"             # brand color used by TUI + web UI badges
```

### 3.2 Probe algorithm

`discover(registry, timeout) -> Vec<DiscoveredProvider>`:

1. For each registry entry × port, `GET http://127.0.0.1:<port><identify.path>` with the
   per-probe timeout (default 400ms). All probes run concurrently (`join_all`);
   full sweep completes in ~1 timeout window.
2. **Disambiguation on shared ports** (8080 is contested: llama.cpp, LocalAI, TGI…):
   identify probes are provider-*specific* (`/props` ⇒ llama.cpp, `/api/version` ⇒ ollama).
   A port only matches the first registry entry whose identify probe passes; registry
   order = priority. A bare `200 OK` on a generic path is *not* a match.
3. For each match, fetch the model list (best-effort; empty list is fine — exo may 404 it).
4. Result: `DiscoveredProvider { slug, title, base_url, models: Vec<String>, version: Option<String> }`.

With `--watch` (default on), a background tokio task re-probes every 10s and emits
`ProviderUp`/`ProviderDown`/`ModelsChanged` events (see §5.3) so the TUI/web sidebar
track providers being started/stopped without a gateway restart.

If **zero** providers are found: print the probe table with hints
("ollama not detected — is it running? `ollama serve`") and exit 1 (or stay up in
`--watch` mode showing an empty-state TUI).

### 3.3 Where it lives

New module, CLI crate (no new crate yet — discovery is ~300 lines and CLI-specific):

```
rust/crates/cli/src/commands/server/inference/
  mod.rs          # InferenceCommand (clap) + run()
  discovery.rs    # registry load/merge, probe, watch task
  providers.yml   # embedded registry (top 5)
  spec.rs         # DiscoveredProvider -> ApiSpec synthesis
```

---

## 4. Proxy layer

### 4.1 Spec synthesis — reuse everything

For each `DiscoveredProvider`, synthesize an `ApiSpec` (`pay_types::metering::ApiSpec`):

```yaml
name: ollama
subdomain: ollama              # -> ollama.localhost:1402
title: "Ollama"
category: ai_ml
routing:
  type: proxy
  url: "http://127.0.0.1:11434"
endpoints: []                  # empty = every path is free -> GateDecision::Passthrough
```

This exploits existing gate behavior (`gate.rs:204-246`): a path matching no metered/
subscription endpoint is `Passthrough` — forwarded upstream, unmetered, but **still
captured by `logging()` → `record_exchange()`** (`proxy/src/http402.rs:646-673`).
So v1 requires **no changes to the gate or the payment path at all.**

Launch path mirrors `StartCommand::run()` (`start.rs:780+`) but drastically simpler —
extract the shared skeleton rather than copy it:

```
InferenceCommand::run()
 ├─ discover() -> Vec<DiscoveredProvider>
 ├─ specs: Vec<ApiSpec>  (synthesized; + --spec merge)
 ├─ AppState { apis: Arc::new(specs), pdb: Some(PdbState::new(config)), ..Default-ish }
 │    (no mpps, no session_mpp, no x402, no signer — all None/empty)
 ├─ axum control-plane on 127.0.0.1:0  (openapi.json, /__402/pdb/*, provider index)
 ├─ tokio: watch task + PDB cleanup task
 ├─ if TUI: run gateway thread + TUI on main thread (see §6.4)
 └─ pay_proxy::run(state, bind, control_plane_addr, threads)
```

**Refactor note:** `start.rs` is 4,037 lines with the axum-router construction and
`AppState` buried inside `run()`. Extract into `server/common.rs`:
`build_control_plane(state) -> Router`, `spawn_control_plane(router) -> SocketAddr`,
and make `AppState` + its `PaymentState` impl `pub(crate)` and constructible with
payments disabled. `StartCommand` and `InferenceCommand` both consume these.

### 4.2 Dynamic upstreams

Routes load once at startup today (no runtime registration). With `--watch`, providers
can appear later. Two options:

- **(A) chosen:** make `AppState.apis` an `Arc<ArcSwap<Vec<ApiSpec>>>` (or `RwLock`).
  The gate reads `apis()` per request already (`PaymentState::apis()`), so swapping the
  vec on `ProviderUp/Down` gives dynamic registration with a tiny diff. `arc-swap` is a
  new (small, zero-dep) workspace dependency; `RwLock` is fine too if we'd rather not add it.
- (B) restart-on-change: simpler but drops in-flight requests and TUI state. Rejected.

### 4.3 Streaming (LLM-critical)

- Pingora streams response bodies natively; SSE/chunked pass through unbuffered.
  `suppress_error_log` already tolerates client disconnects mid-stream (`http402.rs:679`).
- PDB's body capture marks streaming bodies `<streaming>` (8MB cap otherwise) — fine for
  the log, but we want **token usage + TTFT**, which for OpenAI-compat streams lives in
  SSE `data:` chunks. Add a lightweight **stream observer** in `Http402Gate` using
  Pingora's `upstream_response_body_filter`: when `content-type: text/event-stream` and
  the upstream is a known-inference `ApiSpec` (mark synthesized specs, e.g.
  `category: ai_ml` + an `inference: true` extension flag), scan chunks for:
  - first-token timestamp (TTFT),
  - `"usage":{...}` in the final chunk (OpenAI-compat; clients get it when they set
    `stream_options: {include_usage: true}` — we parse it opportunistically),
  - Ollama-native `eval_count` / `prompt_eval_count` on the final `done:true` object.
  The observer only pattern-scans; it never buffers more than the current chunk tail.
  Results are attached to the `LogStart` ctx and flushed with `record_exchange()`.

### 4.4 Payment gating

None in v1 — synthesized specs have no metered endpoints, so everything is
`Passthrough`. Phase 6 (§9) adds `--monetize`, which flips synthesis to emit
`metering: { dimensions: [{ direction: usage, unit: tokens, ... }] }` per endpoint and
requires the standard operator flags. The schema, gate, MPP/x402 backends, and splits
all already support this.

---

## 5. Request tracking (PDB generalization)

### 5.1 Today

`pay-pdb` (`rust/crates/pdb`): `logging()` → `LogEntry` → `FlowCorrelation::ingest()`
creates `PaymentFlow`s **only when it sees 402 challenges / payment credentials** —
plain 200 passthrough traffic produces no flow. Events fan out on
`PdbState.tx: broadcast::Sender<SseMessage>` → SSE → React.

### 5.2 Changes

1. **Ingest mode.** `FlowCorrelation` gains a mode:
   ```rust
   pub enum CorrelationMode { PaymentFlows, AllExchanges }
   ```
   In `AllExchanges`, every `LogEntry` becomes a flow immediately
   (`FlowCreated` on request start is not possible today because `ingest()` runs at
   response time — see item 3), with payment-correlation logic still applied when 402s
   do occur (monetized phase reuses everything).
2. **Inference fields.** Extend `PaymentFlow` (additive, `Option`s, no breaking change
   to the TS mirror in `web-ui/api/types.ts`):
   ```rust
   pub struct InferenceInfo {
     pub provider: String,          // "ollama"
     pub model: Option<String>,     // parsed from request body ("model": …)
     pub endpoint_kind: Option<String>, // chat | completion | embeddings | other
     pub streamed: bool,
     pub tokens_prompt: Option<u64>,
     pub tokens_completion: Option<u64>,
     pub ttft_ms: Option<u64>,
     pub tokens_per_sec: Option<f64>,
   }
   // PaymentFlow { …, pub inference: Option<InferenceInfo> }
   ```
3. **In-flight visibility.** Today `record_exchange()` fires once, at response end — a
   90-second generation would be invisible until it finishes. Split the hook:
   - `record_request_start(meta) -> ExchangeHandle` called from `request_filter`
     (emits `FlowCreated`, status `InProgress` — new `FlowStatus::InProgress` variant),
   - `record_exchange(handle, response_meta)` at `logging()` (emits `FlowUpdated`).
   `PaymentState` gets the new method with a no-op default, so only `AppState` opts in.
   The stream observer (§4.3) can additionally emit a throttled `FlowUpdated` (~1/s)
   carrying running token counts, which gives the TUI/web a live tokens/sec ticker.
4. **Provider status events.** New `SseMessage` variants:
   ```rust
   ProviderStatus { providers: Vec<ProviderSummary> }   // full state, sent on change + to new subscribers
   ```
   (`ProviderSummary { slug, title, base_url, up, models, version, color }`.)
5. **Config surface.** `PdbState.config` is arbitrary JSON already; `serve inference`
   sets `{ "mode": "inference", "title": "Pay Inference", "providers": [...] }` and the
   frontend switches views on `mode` (§7).

### 5.3 Event bus = single source of truth

Both consumers subscribe to the same `broadcast::Sender<SseMessage>`:

```
Http402Gate hooks ──> record_request_start/record_exchange ──> FlowCorrelation
                                                                    │ broadcast
                                     ┌──────────────────────────────┼──────────────┐
                                     ▼                              ▼              ▼
                              SSE /logs/stream                TUI bridge      (future: OTLP)
                              (web-ui, unchanged)         (broadcast→std mpsc)
```

The TUI bridge is a tokio task doing `rx.recv().await` → `std_tx.send(msg)`, matching
the existing topup pattern of draining a `std::sync::mpsc` with `try_recv()` in the
50 ms render loop (`tui.rs:717-871`). No new sync machinery.

---

## 6. TUI

### 6.1 Refactor first: `tui.rs` → `tui/` module

`rust/crates/cli/src/tui.rs` is a 3,234-line monolith holding terminal plumbing,
generic widgets, and two flows (topup, session-setup). Split **without behavior change**:

```
rust/crates/cli/src/tui/
  mod.rs          # pub use — public surface unchanged (run_topup_flow, setup_session)
  term.rs         # with_terminal(), DowngradeBackend, supports_truecolor(), SPINNER
  theme.rs        # CARD_BG, TOPUP_SIDEBAR_BG/MAIN_BG, SOLANA_{PURPLE,BLUE,GREEN}, border conventions
  widgets.rs      # render_slider_box, render_topup_slider (→ render_slider), render_qr_code,
                  # render_money_flow, render_success_checkmark, spinner helpers, logo,
                  # NEW: sidebar_card (extracted from topup option cards), controls_bar
  topup.rs        # run_topup + topup-specific rendering (PollState stays here)
  session.rs      # setup_session + its rendering
  inference.rs    # NEW (§6.2)
```

Extraction targets (from the current file): `render_slider_box` (line 2640, already
shared), `render_money_flow` (1785, already generic), `render_qr_code` (1874),
`render_success_checkmark` (989), the 38-col sidebar + option-card pattern
(1061-1145 — parameterize into `sidebar_card(title, subtitle, color, selected)`), and
the bottom controls bar (1934 — parameterize as `controls_bar(&[(key, label)], status)`).
Topup-specific pieces (QR stable sizing, onramp intro, PollState) stay in `topup.rs`.

### 6.2 Inference TUI layout

Same visual language as topup: 38-col dark sidebar + content window + 1-row controls,
rounded borders, Solana palette, spinner in the corner.

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ ▚▚ Pay Inference                 gateway http://127.0.0.1:1402      ⠹ live   │
│┌────────────────────┐┌──────────────────────────────────────────────────────┐│
││ PROVIDERS          ││  REQUESTS                                   127 total ││
││                    ││  time      prov    model         path     st    tok/s ││
││ ● Ollama           ││ ▸12:01:22  ollama  llama3.2:3b   /v1/chat  ⣷ …   41.2 ││
││   :11434 · 3 models││  12:01:14  ollama  llama3.2:3b   /v1/chat  200   38.9 ││
││   ▸ llama3.2:3b    ││  12:00:58  lm-st…  qwen2.5-7b    /v1/emb…  200     —  ││
││   ▸ qwen2.5-coder  ││  12:00:41  ollama  llama3.2:3b   /api/tags 200     —  ││
││   ▸ nomic-embed    ││ ─────────────────────────────────────────────────────││
││                    ││  DETAIL  POST /v1/chat/completions · ollama          ││
││ ● LM Studio        ││  model llama3.2:3b   stream ✓   status in-progress   ││
││   :1234 · 1 model  ││  ttft 182ms   tokens 214/512   41.2 tok/s            ││
││                    ││  12:01:22.101  request received (curl/8.6, ::1)      ││
││ ○ llama.cpp        ││  12:01:22.283  first token                           ││
││   not detected     ││  12:01:24.020  streaming… 214 completion tokens      ││
│└────────────────────┘└──────────────────────────────────────────────────────┘│
│  ↑↓ select  ⏎ detail  p providers  f filter  c clear  w web ui  q quit       │
└──────────────────────────────────────────────────────────────────────────────┘
```

- **Left pane** — provider cards (reusing `sidebar_card`): status dot
  (green up / gray down, brand color accent), port, model count; expandable model list
  for the selected provider. Data: `ProviderStatus` events.
- **Content window** — top: scrolling request table (newest first, auto-follow until the
  user selects a row — same auto-expand-latest behavior as the web `App.tsx`); bottom:
  detail panel for the selected request (model, stream, TTFT, live token counters,
  event log — rendered from `PaymentFlow.events` + `InferenceInfo`).
- **Controls bar** — reuses `controls_bar`; `w` opens the web UI via `webbrowser`
  (dep already present).

### 6.3 App state & event loop

```rust
struct InferenceApp {
  providers: Vec<ProviderSummary>,
  flows: VecDeque<PaymentFlow>,        // ring, mirrors PDB's 200 cap
  selected_pane: Pane,                 // Providers | Requests
  selected_provider: usize,
  selected_flow: Option<String>,       // flow id; None => follow latest
  filter: Filter,                      // All | Errors | PerProvider(slug)
  scroll: u16,
}
```

Event loop = the proven topup shape (`tui.rs:717-871`): 50 ms `event::poll` tick;
drain `std mpsc` of bridged `SseMessage`s (`Snapshot`/`FlowCreated`/`FlowUpdated`/
`ProviderStatus`) into `InferenceApp`; render; handle keys. No async in the render loop.

### 6.4 Threading

Verified against pingora-core 0.5 source: `Server::run()` has **no main-thread
affinity** — it spawns its own tokio runtimes and blocks the calling thread; signal
handling is `tokio::signal::unix` (process-wide, thread-agnostic); the fork warning only
applies to daemon mode, which we don't use.

Better still, we bypass signals entirely. `run_forever()` is `run(RunArgs::default())` +
`std::process::exit(0)` — the exit would skip terminal restore, so we don't use it.
`run(RunArgs)` accepts a custom `shutdown_signal: Box<dyn ShutdownSignalWatch>`
(async `recv() -> ShutdownSignal`). Arrangement:

- Add `pay_proxy::run_with_shutdown(state, bind, control_plane, threads, shutdown_rx)`
  alongside `run()` (`proxy/src/lib.rs`), calling `server.run(RunArgs { shutdown_signal })`
  with a watcher that `select!`s a `tokio::sync::watch` channel **and** SIGTERM
  (so non-interactive `kill` still works).
- Main thread: TUI. Spawned `std::thread`: `run_with_shutdown` (returns after shutdown
  instead of exiting the process).
- Crossterm raw mode delivers Ctrl-C as a key event (no SIGINT), so the TUI owns quit:
  send `ShutdownSignal::FastShutdown` (avoids `GracefulTerminate`'s blocking
  `thread::sleep(grace_period)`), join the proxy thread, restore terminal, exit.
- Call `server.bootstrap()` **before** entering the alternate screen — it can
  `std::process::exit(1)` on fd-load failure, which must not leave the terminal raw.

In `--no-tui` mode, keep today's arrangement (Pingora on main thread via `run()`,
log lines via `tracing`).

---

## 7. Web UI (`pdb/` → `web-ui/`)

### 7.1 Rename

Directory `pdb/` → `web-ui/`. Touchpoints (all found in the audit):

| what | file | change |
|---|---|---|
| dist path in embed build | `rust/crates/pdb/build.rs` | `pay/pdb/dist` → `pay/web-ui/dist` (keep `PAY_PDB_DIST` env override working) |
| package name | `web-ui/package.json` | `payment-debugger` → `pay-web-ui` |
| HTML title | `web-ui/index.html` | static title → set at runtime from config (`title` field; default "Pay Debugger") |
| README/docs | `web-ui/README.md`, root `README.md`, `CONTRIBUTING.md` | path + name refs |
| CI / Justfile | any `pnpm --dir pdb` refs | update path |
| vercel | `web-ui/vercel.json` | unchanged functionally; domain rename is a separate infra task, not this PR |

Keep: crate name `pay-pdb`, mount path `/__402/pdb`, and the `--debugger` flag — they're
wire/API surface; renaming them buys nothing and breaks embedding. The *app* is `web-ui`;
"PDB" remains the debugger *mode* of that app.

### 7.2 "Pay Inference" mode

`useConfig` already fetches `/__402/pdb/api/config`. Add `mode: "inference" | "debugger"`
(+ `title`, `providers`) to it, then:

- **Header**: title from config ("Pay Inference"), keep theme toggle.
- **Sidebar**: in inference mode, replace endpoint/protocol listing with the provider
  list (status dot, port, models, brand color) — driven by `ProviderStatus` SSE messages
  (extend `useFlows` reducer with a `providers` slice; message shape mirrors Rust).
- **FlowRow**: when `flow.inference` present, badge shows provider brand color + slug
  instead of protocol, and the row gains model + tok/s columns.
- **FlowDetail**: new `InferencePanel` in the middle slot (where `PaymentSplits`/
  `SessionChannel` go today): model, stream flag, TTFT, prompt/completion tokens,
  live tok/s while `InProgress`. Sequence diagram simplifies to
  request → (first token) → completed for un-metered flows; the 402 diagram appears
  automatically once monetization is on since correlation logic is unchanged.
- **StatusIndicator**: add `in-progress` (pulsing) for the new `FlowStatus::InProgress`.
- Types: mirror `InferenceInfo`, new `SseMessage` variants, and `FlowStatus` addition in
  `web-ui/api/types.ts`; keep the TS correlation engine (`api/correlation.ts`) compiling
  but inference mode is Rust-embedded only (the Vercel demo stays debugger-mode).

---

## 8. File-level change map

```
rust/crates/cli/
  src/commands/mod.rs                     # alias "serve" on Server variant
  src/commands/server/mod.rs              # + Inference variant, dispatch
  src/commands/server/common.rs           # NEW: extracted AppState + control-plane builders (from start.rs)
  src/commands/server/start.rs            # consume common.rs (mechanical shrink)
  src/commands/server/inference/mod.rs    # NEW: InferenceCommand
  src/commands/server/inference/discovery.rs  # NEW
  src/commands/server/inference/providers.yml # NEW: embedded registry
  src/commands/server/inference/spec.rs   # NEW: DiscoveredProvider -> ApiSpec
  src/tui.rs → src/tui/{mod,term,theme,widgets,topup,session,inference}.rs
  Cargo.toml                              # (maybe) arc-swap

rust/crates/proxy/
  src/lib.rs                              # run_with_shutdown() variant (custom ShutdownSignalWatch)
  src/http402.rs                          # record_request_start hook; SSE stream observer

rust/crates/core/
  src/server/gate.rs or state trait home  # PaymentState::record_request_start (default no-op)

rust/crates/pdb/
  src/types.rs                            # InferenceInfo, FlowStatus::InProgress, SseMessage::ProviderStatus
  src/correlation.rs                      # CorrelationMode::AllExchanges, in-flight flows
  src/lib.rs                              # PdbState mode plumbing
  build.rs                                # dist path pdb/ -> web-ui/

pdb/ → web-ui/                            # git mv; package.json, index.html, README
web-ui/src/…                              # config mode, Sidebar providers, FlowRow/FlowDetail/InferencePanel,
                                          # useFlows ProviderStatus handling, StatusIndicator in-progress
web-ui/api/types.ts                       # TS mirrors

docs/serve-inference.md                   # this plan
```

---

## 9. Phasing (PR sequence)

Each phase lands green and independently revertable:

1. **PR-1 — TUI module split.** Pure refactor of `tui.rs`; public fns unchanged;
   snapshot the topup TUI manually before/after. No feature code.
2. **PR-2 — PDB generalization + rename.** `git mv pdb web-ui`, build.rs path,
   `CorrelationMode`, `FlowStatus::InProgress`, `InferenceInfo`, `ProviderStatus`
   message, config `mode/title`. Web app compiles in both modes; debugger mode is
   pixel-identical (guarded by existing behavior: mode defaults to `debugger`).
3. **PR-3 — discovery + headless gateway.** `pay serve inference --no-tui` end-to-end:
   registry, probes, watch task, spec synthesis, `common.rs` extraction, dynamic
   `apis` swap, free passthrough proxying with `record_exchange` flowing to PDB.
   Web UI already renders it via PR-2.
4. **PR-4 — inference TUI.** `tui/inference.rs`, broadcast→mpsc bridge, Pingora
   off-main-thread arrangement, provider pane + request table + detail.
5. **PR-5 — streaming telemetry.** `upstream_response_body_filter` observer:
   TTFT, token usage (OpenAI-compat + Ollama-native), throttled in-flight
   `FlowUpdated`, tok/s in TUI + web.
6. **PR-6 (later) — monetization.** `--monetize` + operator flags; synthesis emits
   `unit: tokens` metering (priced via flag or per-provider config); everything
   downstream (challenges, MPP/x402 verify, splits, receipts, PDB payment flows)
   already works. Token counts for **billing** must come from the §4.3 observer as the
   authoritative meter, not client-reported usage.

## 10. Testing

- **Discovery**: unit tests against `axum` stub servers impersonating each provider's
  identify endpoint (incl. the port-8080 ambiguity case and the false-positive
  "generic 200" case). Registry YAML parse tests.
- **Spec synthesis**: golden test `DiscoveredProvider` → `ApiSpec` YAML.
- **Proxy passthrough**: integration test (pattern exists in `crates/integration`):
  stub upstream, spec with empty endpoints, assert body/status/headers pass through and
  `record_exchange` fired; SSE stub asserting chunks arrive unbuffered + observer
  extracts usage from a canned OpenAI-compat stream and an Ollama-native stream.
- **Correlation `AllExchanges`**: LogEntry sequences → flow snapshots (existing test
  style in `correlation.rs`); in-flight → delivered transitions.
- **TUI**: keep it thin; state-struct unit tests (key event → `InferenceApp` mutation),
  `ratatui::backend::TestBackend` snapshot for the layout skeleton.
- **Web**: `pnpm build` in CI (already required by pdb build.rs release gate);
  type-check of the mirrored types.
- **Manual matrix**: Ollama + LM Studio live on this machine; llama.cpp via
  `llama-server -m …`; vLLM/exo best-effort (record probe transcripts as fixtures).

## 11. Implementation notes (deviations from the plan above)

Decisions made during implementation — the sections above describe the
original plan; where they differ, this section wins:

- **No `common.rs` extraction (§4.1/§8).** `start.rs` carries unrelated
  in-flight work on this branch; instead of extracting `AppState`, the
  inference command got its own minimal `InferenceState` (`PaymentState` with
  every payment backend `None` + the PDB `record_exchange` hook) and its own
  ~20-line control-plane router. Extraction can still happen later when
  `start.rs` is quiet.
- **Routes are static at startup (§4.2).** No `ArcSwap`: `PaymentState::apis()`
  returns `&[ApiSpec]`, which can't hand out a guard from a swap. The `--watch`
  loop re-probes and broadcasts provider up/down/model changes (a provider
  restarting on the same port resumes seamlessly since routing is by
  subdomain → fixed base_url), but a brand-new provider logs a
  restart-to-route hint instead of hot-registering.
- **Flag surface (§2.1).** `--watch-interval <SECS>` (0 disables) instead of
  `--watch`; no `--public-url` (nothing to rewrite — inference serves no
  OpenAPI in v1); `--spec` is repeatable rather than comma-joined.
- **AllExchanges merges 402 challenges with their paid retries** (Phase-6
  unification, landed 2026-07-05): a metered exchange that 402s parks as
  `payment-required`; the retry — spotted by its `Payment` Authorization
  header, threaded through `RequestStart.payment` — attaches to the same
  flow, so one logical request is one row with the 4-step payment diagram.
  Plain upstream 402s still fail; full PaymentFlows-style cross-protocol
  correlation stays debugger-mode-only.
- **Built-in providers moved from the embedded providers.yml into code**
  (2026-07-05): `InferenceProvider` trait with one impl + test module per
  provider (`inference/providers/`); the user override file
  `~/.config/pay/inference-providers.yml` keeps its schema via
  `CustomProvider` and still shadows built-ins by slug.
- **Payment Debugger correlation is otherwise untouched**; debugger mode
  stays pixel-identical in the web UI (verified by leaving mode unset).

## 12. Risks & open questions

- ~~**Pingora off the main thread**~~ — resolved, see §6.4: `Server::run()` is
  thread-agnostic and takes a pluggable `ShutdownSignalWatch`; we add
  `run_with_shutdown` to pay-proxy and never call `run_forever` in TUI mode.
- **`*.localhost` subdomains** work in browsers/curl on macOS+Linux, but some tools
  resolve strictly via `/etc/hosts`. Escape hatch: bare-host single-provider fallback
  already covers the 1-provider case; consider `--port-per-provider` later if it bites.
- **8080 collisions** beyond the registry (any dev server answers 200): mitigated by
  provider-specific identify probes; keep them strict (JSON key match, not status-only).
- **exo probe details** (port/dashboard endpoints) verified against exo `main` at
  implementation time — registry data is trivially updatable, and the user-override
  file de-risks drift for all five.
- **Body capture cost**: request-body model extraction requires peeking POST bodies in
  the gate; bodies stream unbuffered today. Peek only up to 16 KiB of
  `application/json` request bodies on inference specs (model field is near the top);
  never buffer streams. Measure before/after with the existing bench harness
  (`rust/bench/`), since we've been benchmarking session/charge forks there.
