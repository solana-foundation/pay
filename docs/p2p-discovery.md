# P2P provider discovery — Design & Implementation Plan

Status: **draft / planning** (researched 2026-07-05)
Branch: follows `feat/serve-inference` (see docs/serve-inference.md)
Owner: @ludo

The goal from the serve-inference roadmap: devs running `pay serve inference`
register their gateway so any `pay` client (claude/codex/qoder/curl) can
discover, select, and pay them — no central operator required. This doc answers
three questions asked up front — *is a DHT state of the art? do we need TCP
hole punching? what can we reuse from the Solana stack?* — and then plans the
`p2p` module.

---

## 1. TL;DR — the three answers

**Is a Kademlia DHT state of the art?** No. The field bifurcated (~2023-2026):

- For *endpoint* discovery (key → current address), the modern pattern is
  **iroh-style dial-by-key**: QUIC + hole punching + stateless relays, with
  signed DNS/pkarr records instead of a bespoke DHT. iroh 1.0 (June 2026)
  deliberately ships **no DHT** — its opt-in fallback publishes signed records
  to the existing BitTorrent Mainline DHT via pkarr rather than running one.
- For a *service registry with attribute queries* ("all providers of model X
  under price P"), **no production system uses a raw DHT** — DHTs only do
  exact-key lookup, records are ~1000 bytes with hours-long TTLs, and there is
  zero sybil resistance. Even IPFS fronted its DHT with an HTTP indexer (IPNI).
  Every decentralized *inference* marketplace (Bittensor, Nosana, Kuzco)
  converged on the same shape: **registry control plane + direct P2P data
  plane**. Bittensor literally puts miner endpoints on-chain and has clients
  filter the metagraph locally.

**Do we need TCP hole punching?** Nobody ships TCP hole punching in 2026 (a
2025 study of 4.4M attempts found TCP ≈ QUIC success *when* RTT-synchronized —
but the ecosystem standardizes on QUIC because relays/multiplexing/fallback are
cleaner). The practice is **QUIC hole punching + always-available encrypted
relay fallback**: iroh reports ~90% direct connections in production (libp2p
DCUtR measures ~70% on its more hostile population), with self-hostable
stateless relays (`iroh-relay`: a binary on any VPS, ACME TLS built in).
Two big caveats that shape our design:

- Hole punching needs cooperation from **both** ends. A vanilla HTTPS client
  (plain `curl`) can't punch. Our advantage: **our clients are our CLI** — the
  payer proxy that `pay claude` already runs can embed an iroh endpoint, so
  the whole fleet is p2p-capable without users noticing.
- IPv6 is at ~50% of Google traffic (Mar 2026) but doesn't obviate traversal:
  both ends need it and home routers still drop unsolicited inbound. Treat v6
  as one more candidate path, as iroh does.

**What can we reuse from Solana?**

- **`solana-gossip`: no.** It's `agave-unstable-api`-gated (breaks without
  notice), 0% documented, and — the killer — CRDS is a *closed enum* of
  validator record types (ContactInfo/Vote/…): there is no extension point for
  "inference gateway at URL, models M, price P". Reusing it means forking
  Agave and running a private gossip cluster. Even DoubleZero, the most
  gossip-adjacent Solana project, registered its devices **on-chain** and
  built its own control plane instead.
- **The on-chain registry pattern: yes — it's what the whole Solana DePIN
  ecosystem does** (Nosana GPU markets: stake-gated node queues on-chain, jobs
  off-chain; Helium: compressed-NFT entities on-chain, radio off-chain).
  Registration costs rent (~0.0015 SOL ≈ $0.25 today for ~300 bytes,
  *refundable*, dropping ~10× via SIMD-0437) — which doubles as sybil
  resistance. At 10k providers this is ~15 SOL locked, total; ZK compression
  is unnecessary at our scale.
- **Strongest concrete reuse candidate:** the **Solana Agent Registry /
  ERC-8004-on-Solana** work — `8004-solana` is live and audited on mainnet
  (identity/reputation/validation registries; registration files carry MCP/A2A
  endpoints and wallets), and the SATI sRFC (#7) standardizes the same with
  Token-2022 non-transferable NFTs. An MPP inference gateway is exactly an
  "agent with paid endpoints" — evaluate riding this before writing our own
  program.
- **One idea worth stealing, not the crate:** agave's QUIC stack authenticates
  peers by putting the **Ed25519 pubkey in the TLS cert**. Implement that
  directly on quinn/rustls if we ever need it — iroh already gives us
  key-authenticated QUIC anyway.

---

## 2. Architecture

Three separable problems, three layers:

```
┌────────────────────────────────────────────────────────────────────┐
│ REGISTRY (control plane, Solana)                                   │
│   PDA per gateway: solana key, iroh EndpointID, https url?,        │
│   network, models URI/hash, pricing hint, MPP realm, attestations  │
│   rent-refundable (+ optional stake) ⇒ sybil resistance            │
│   query: getProgramAccounts / indexer → filter locally             │
└──────────────┬─────────────────────────────────────────────────────┘
               │ resolve → filter by model/price
┌──────────────▼─────────────────────────────────────────────────────┐
│ CONNECTIVITY (data plane)                                          │
│   tier A: public HTTPS endpoint (today's Pingora — zero change)    │
│   tier B: iroh dial-by-EndpointID (QUIC hole punch, ~90% direct)   │
│   tier C: stateless relay fallback (self-hosted iroh-relay fleet)  │
└──────────────┬─────────────────────────────────────────────────────┘
               │ dial → verify → 402 → pay (MPP, unchanged)
┌──────────────▼─────────────────────────────────────────────────────┐
│ TRUST (what protects the payer)                                    │
│   registry record valid iff signed by the on-chain-registered key  │
│   price truth = the 402 challenge itself (never pay unverified)    │
│   liveness = dial-time probing, not on-chain heartbeats            │
│   reputation/attestations later (8004 reputation / SAS)            │
└────────────────────────────────────────────────────────────────────┘
```

Design principles:

1. **The 402 flow is already the trust anchor.** Clients never pay without a
   verified challenge, and the challenge *is* the authoritative price — the
   registry's pricing field is a search hint, not a contract. Worst-case
   registry spam costs the client one wasted dial. This is why a
   light-consistency registry is fine.
2. **Identity = the Solana wallet the provider already has.** The iroh
   EndpointID (Ed25519) is bound to it with a signature both ways inside the
   registry record. Payments, registry writes, and transport auth all chain to
   one identity.
3. **Liveness is client-side.** No on-chain heartbeats. pkarr/DNS keeps the
   *address* fresh (iroh republishes automatically); a dead provider fails the
   dial and is skipped. The picker's filter/search UI (already shipped) is the
   consumption surface.
4. **Local discovery stays.** Port probing (the `InferenceProvider` trait) and
   the registry are just two sources feeding the same picker.

### 2.1 What we intentionally do NOT build

- **No bespoke DHT** and no solana-gossip fork — registry control plane +
  pkarr-backed addressing covers both needs with maintained infrastructure.
- **No gossip mesh in v1.** iroh-gossip (+ distributed-topic-tracker for
  serverless bootstrap) is the upgrade path if registry polling ever becomes a
  bottleneck — providers would broadcast signed availability adverts per model
  topic. Not needed at 10²-10³ providers.
- **No TCP hole punching, no WebRTC/ICE stack** — QUIC via iroh, full stop.
- **No reverse-tunnel product** (rathole/Cloudflare/Funnel). Vanilla-HTTPS
  reachability for non-pay clients stays the provider's own choice (tier A);
  our CLI fleet uses tier B.

---

## 3. The `p2p` module

New lib crate `rust/crates/p2p` (`pay-p2p`) — same lib-only pattern as
`pay-proxy`; the heavy iroh tree stays out of core/kit dependents.

```
crates/p2p/
  src/lib.rs
  src/identity.rs    # SolanaKey ⟷ iroh SecretKey/EndpointID binding + proofs
  src/registry/
    mod.rs           # ProviderRecord {identity, endpoint_id, https_url?,
                     #   network, models_uri, pricing_hint, realm, sig}
                     # trait ProviderRegistry { publish/refresh/deregister/list }
    onchain.rs       # Solana program client (or 8004-solana adapter — §5 P1)
    http.rs          # indexer/cached mirror client (read path at scale)
    mock.rs          # in-memory impl for tests
  src/transport/
    mod.rs
    acceptor.rs      # gateway side: iroh Endpoint accepting streams, bridged
                     #   into the local Pingora bind (127.0.0.1:1402) — HTTP/1.1
                     #   over iroh bidi streams; ALPN "pay-mpp/0"
    dialer.rs        # client side: dial-by-EndpointID → hyper-compatible
                     #   connector (payer proxy / pay curl plug in here)
  src/probe.rs       # dial-time verification: identify, model list, TTFB
```

Integration seams (all already exist):

- **`InferenceProvider` trait** (cli): a `RemoteProvider` impl backed by a
  `ProviderRecord` — `identify()` = dial + probe, `list_models()` from the
  record's models URI verified against the live `/v1/models`,
  `paid_endpoints()` from the record. The picker, spec synthesis, and TUI all
  work unchanged.
- **`pay serve inference --announce`**: after startup, bind identity, publish
  the record, start the iroh acceptor alongside Pingora. `--sandbox` refuses
  to announce to mainnet registries (the guard extends naturally).
- **Payer proxy** (`claude/payer.rs`): gains the iroh connector for
  `EndpointID` upstreams — everything else (402 handling, streaming) is
  transport-agnostic already.
- **PDB/TUI**: a remote provider is just a `ProviderSummary` with a base_url
  of `iroh://<endpoint-id>`; connection aggregation unchanged.

Dependencies added (workspace): `iroh = "1"` (wire-stable), optionally
`pkarr = "6"` later for registry-less address fallback. Nothing else.

---

## 4. Wire details worth pinning early

- **Record (on-chain, ~300 bytes)**: `{version, solana_pubkey, endpoint_id,
  https_url: Option<64B>, network_slug, models_uri_hash, realm, pricing_hint
  (µUSD flat), flags, iroh_sig, solana_sig}`. Fat metadata (full model list,
  descriptions) lives in an off-chain registration file at `models_uri`
  (ERC-8004 style), hash-pinned by the record.
- **Transport ALPN**: `pay-mpp/0` — plain HTTP/1.1 (incl. SSE) over an iroh
  bidirectional stream per request, exactly what the payer proxy already
  speaks; h3 is a later optimization.
- **Relays**: start on n0's public relays (free tier, rate-limited) for the
  spike; run 2-3 `iroh-relay` instances (cheap VPSes, ACME built in) before
  any public beta. Relay URLs ship in the record so providers can pin their
  own.
- **Sandbox/mainnet**: registry program deploys per network; `--sandbox`
  binds to the devnet/localnet registry only. Mainnet announcing stays behind
  the same deliberate gate as mainnet monetization (both unwired today).

---

## 5. Phases

- **P0 — transport spike (no registry).** `crates/p2p` with
  identity+transport only; `pay serve inference --announce=print` prints the
  EndpointID; `pay curl iroh://<id>/v1/models` works through NAT via the
  payer-proxy connector. Proves the bridge + hole punching on real networks.
  Unit tests: identity binding proofs, acceptor↔dialer loopback, SSE through
  the bridge.
- **P1 — registry decision.** Time-boxed evaluation of `8004-solana` (live,
  audited, reputation included) and the SATI sRFC against the record in §4;
  fall back to a ~200-line Anchor program of our own if the fit is poor.
  Decision doc + devnet deployment + `registry::onchain` client with mock
  parity tests.
- **P2 — announce.** `pay serve inference --announce` publishes/refreshes on
  startup, deregisters (rent refund) on clean shutdown; record includes the
  live model list URI served from the gateway's control plane.
- **P3 — discover.** `pay claude` (and friends) merge registry results into
  the picker — the filterable table was built for exactly this; remote
  entries show latency from the dial-time probe. `pay serve inference` gets
  `--registry` to list peers in the web UI too.
- **P4 — operate & trust.** Self-hosted relay fleet, registry indexer
  (read-path cache), 8004 reputation / SAS attestations surfaced as badges in
  the picker, gossip adverts only if polling shows strain.

## 6. Open questions

- 8004-solana vs own program: does their registration-file schema carry MPP
  pricing/realm cleanly, and is their reputation module worth the coupling?
- Model catalog freshness: hash-pinned URI (re-announce on model change) vs
  live-probe-only (registry lists nothing but identity+endpoint)?
- Mainnet prerequisite: announcing is pointless while monetization is
  sandbox-only — P2/P3 land against devnet; mainnet gates on the payments
  hardening track.
- Privacy: an on-chain registry is public by construction; do we want an
  unlisted mode (pkarr-only, share-the-key discovery)?

## 7. Key sources

iroh 1.0 (2026-06-15, wire-stable; ~90% direct, 200M endpoints/30d):
iroh.computer/blog/v1 · discovery = DNS+pkarr, no DHT:
docs.iroh.computer/concepts/discovery · hole-punching study (4.4M attempts,
~70% ± 7, TCP≈QUIC): arxiv.org/abs/2510.27500 · libp2p stewardship strain +
Polkadot's litep2p exit: ipshipyard.com/blog/2025-libp2p-maintenance-update,
forum.polkadot.network litep2p-network-backend-updates · IPFS fronts its DHT
with an indexer: specs.ipfs.tech/routing/http-routing-v1 · Bittensor metagraph
(endpoints on-chain): docs.learnbittensor.org/subnets/metagraph · Nosana
stake-gated node queues: github.com/nosana-ci/nosana-programs ·
agave-unstable-api (gossip): anza.xyz/blog/agave-4.0-patch-notes ·
8004-solana (mainnet): github.com/QuantuLabs/8004-solana · SATI sRFC #7:
github.com/solana-foundation/SRFCs/discussions/7 · rent halving SIMD-0436/0437.
