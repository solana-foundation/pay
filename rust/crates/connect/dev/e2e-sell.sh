#!/usr/bin/env bash
# End-to-end `sell_inference` on a local Surfpool fork of mainnet:
#
#   surfpool ── pay-connect (sell operator) ── pay sell create ── pay sell serve
#                                                  ▲
#                     pay --local curl (buyer pays: mpp, then x402, then a stream)
#
#   rust/crates/connect/dev/e2e-sell.sh                 # echo harness, no LLM
#   rust/crates/connect/dev/e2e-sell.sh --harness claude # a real agent via claude-agent-acp
#   rust/crates/connect/dev/e2e-sell.sh --keep           # leave everything running
#
# Needs: surfpool, solana-keygen, jq, openssl, and `cargo build -p pay -p pay-connect`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"
BIN="$ROOT/rust/target/debug"
RPC="${RPC:-http://127.0.0.1:8899}"
PORT="${PORT:-8402}"
CONNECT="http://127.0.0.1:$PORT"
HARNESS="echo"
KEEP=0
PRICE="0.02"
# Session channels close (and pay the seller) after this idle time; short so
# the balance check below does not wait ten minutes.
IDLE_CLOSE_SECS=5

while [ $# -gt 0 ]; do
  case "$1" in
    --harness) HARNESS="$2"; shift 2 ;;
    --price) PRICE="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

TMP="$(mktemp -d -t pay-sell-e2e)"
PIDS=()
cleanup() {
  if [ "$KEEP" = 1 ]; then
    echo; echo "Left running (logs in $TMP): pids ${PIDS[*]:-}"; return
  fi
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done
}
trap cleanup EXIT INT TERM

step() { printf '\n\033[1m› %s\033[0m\n' "$*"; }
fail() { printf '\033[31m✗ %s\033[0m\n' "$*" >&2; exit 1; }
ok() { printf '\033[32m✓ %s\033[0m\n' "$*"; }

rpc() { curl -sS "$RPC" -H 'content-type: application/json' -d "$1"; }
wait_for() { # url, tries
  for _ in $(seq 1 "${2:-60}"); do curl -fsS "$1" >/dev/null 2>&1 && return 0; sleep 1; done
  return 1
}

for tool in surfpool solana-keygen jq openssl curl; do
  command -v "$tool" >/dev/null || fail "$tool not found"
done
[ -x "$BIN/pay" ] && [ -x "$BIN/pay-connect" ] || fail "build first: (cd rust && cargo build -p pay -p pay-connect)"

# ── 1. Surfpool: a local fork of mainnet, so mainnet USDC and the payment programs exist.
step "Surfpool at $RPC"
if rpc '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' 2>/dev/null | grep -q '"ok"'; then
  ok "already running"
else
  (cd "$TMP" && surfpool start --port "${RPC##*:}" --network mainnet --no-deploy --no-tui --no-studio </dev/null >"$TMP/surfpool.log" 2>&1) &
  PIDS+=($!)
  for _ in $(seq 1 90); do
    rpc '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' 2>/dev/null | grep -q '"ok"' && break
    sleep 1
  done
  rpc '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' | grep -q '"ok"' || { tail -20 "$TMP/surfpool.log"; fail "surfpool did not come up"; }
  ok "started (log: $TMP/surfpool.log)"
fi

# ── 2. Keys: the operator sponsors fees and signs settlements; the seller only receives.
step "Operator and seller keys"
solana-keygen new --no-bip39-passphrase --silent --force -o "$TMP/operator.json" >/dev/null
solana-keygen new --no-bip39-passphrase --silent --force -o "$TMP/seller.json" >/dev/null
OPERATOR="$(solana-keygen pubkey "$TMP/operator.json")"
SELLER="$(solana-keygen pubkey "$TMP/seller.json")"
USDC="EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
rpc "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"surfnet_setAccount\",\"params\":[\"$OPERATOR\",{\"lamports\":100000000000,\"data\":\"\",\"executable\":false,\"owner\":\"11111111111111111111111111111111\"}]}" | grep -q '"result"' || fail "could not fund the operator"
rpc "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"surfnet_setTokenAccount\",\"params\":[\"$SELLER\",\"$USDC\",{\"amount\":0},\"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA\"]}" | grep -q '"result"' || fail "could not create the seller's USDC account"
ok "operator $OPERATOR (100 SOL), seller $SELLER (empty USDC account)"

seller_usdc() {
  rpc "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getTokenAccountsByOwner\",\"params\":[\"$SELLER\",{\"mint\":\"$USDC\"},{\"encoding\":\"jsonParsed\"}]}" \
    | jq -r '[.result.value[].account.data.parsed.info.tokenAmount.uiAmount] | add // 0'
}

# ── 3. pay-connect with the sell operator and a static creator token.
step "pay-connect on $CONNECT"
CREATOR="pay_e2e_$(openssl rand -hex 8)"
PAY_CONNECT_MCP=1 \
PAY_CONNECT_MCP_TOKENS="$CREATOR" \
PAY_CONNECT_SELL_KEYPAIR="$(cat "$TMP/operator.json")" \
PAY_CONNECT_SELL_RPC_URL="$RPC" \
PAY_CONNECT_SELL_SECRET="$(openssl rand -hex 32)" \
RUST_LOG="${RUST_LOG:-info,pay_connect=debug}" \
  "$BIN/pay-connect" --port "$PORT" >"$TMP/connect.log" 2>&1 &
PIDS+=($!)
wait_for "$CONNECT/health" 30 || { tail -30 "$TMP/connect.log"; fail "pay-connect did not come up"; }
grep -q "sell_inference enabled" "$TMP/connect.log" || { tail -30 "$TMP/connect.log"; fail "sell_inference is not enabled"; }
ok "up (log: $TMP/connect.log)"

# ── 4. The seller creates an endpoint priced per request.
step "pay sell create (\$$PRICE per request, paid to the seller)"
VIEW="$("$BIN/pay" sell create --connect-url "$CONNECT" --token "$CREATOR" \
  --model e2e-agent --price "$PRICE" --recipient "$SELLER" --network localnet \
  --session-idle-close-secs "$IDLE_CLOSE_SECS" --earn-cap 2.00 --json)"
ID="$(jq -r .id <<<"$VIEW")"
OWNER_TOKEN="$(jq -r .owner_token <<<"$VIEW")"
CHAT_URL="$(jq -r .chat_completions_url <<<"$VIEW")"
[ -n "$ID" ] && [ "$ID" != null ] || fail "no endpoint id in: $VIEW"
ok "endpoint $ID"
echo "  schemes: $(jq -r '.schemes | join(", ")' <<<"$VIEW")"
echo "  $CHAT_URL"

# ── 5. The worker, driving the chosen harness from this directory.
step "pay sell serve --harness $HARNESS"
PAY_SELL_OWNER_TOKEN="$OWNER_TOKEN" \
  "$BIN/pay" sell serve "$ID" --harness "$HARNESS" --connect-url "$CONNECT" --cwd "$TMP" >"$TMP/serve.log" 2>&1 &
PIDS+=($!)
sleep 1
grep -q "Serving" "$TMP/serve.log" || { cat "$TMP/serve.log"; fail "the worker did not start"; }
ok "worker up (log: $TMP/serve.log)"

# ── 6. Buyers. `pay --local` pays from the localnet wallet in accounts.yml. The
# client only self-funds a wallet it just created, so fund it here: a fresh
# Surfpool has no token account for it, and the channel program rejects an
# uninitialized payer token account.
step "Buyer wallet"
BUYER="$("$BIN/pay" --local whoami 2>&1 | grep -o 'localnet [1-9A-HJ-NP-Za-km-z]*' | awk '{print $2}' | head -1)"
[ -n "$BUYER" ] || fail "could not read the localnet wallet from \`pay --local whoami\`"
rpc "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"surfnet_setAccount\",\"params\":[\"$BUYER\",{\"lamports\":100000000000,\"data\":\"\",\"executable\":false,\"owner\":\"11111111111111111111111111111111\"}]}" | grep -q '"result"' || fail "could not fund the buyer with SOL"
rpc "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"surfnet_setTokenAccount\",\"params\":[\"$BUYER\",\"$USDC\",{\"amount\":1000000000},\"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA\"]}" | grep -q '"result"' || fail "could not fund the buyer with USDC"
ok "buyer $BUYER (100 SOL, 1000 USDC)"

ASK="hello from the sell e2e"
BODY="{\"model\":\"e2e-agent\",\"messages\":[{\"role\":\"user\",\"content\":\"$ASK\"}]}"
buy() { # protocol-flag, extra-body-json
  "$BIN/pay" --local "$1" curl -sS -X POST "$CHAT_URL" -H 'content-type: application/json' -d "$2"
}

step "Unpaid request is challenged"
CODE="$(curl -sS -o "$TMP/402.json" -w '%{http_code}' -X POST "$CHAT_URL" -H 'content-type: application/json' -d "$BODY")"
[ "$CODE" = 402 ] || { cat "$TMP/402.json"; fail "expected 402, got $CODE"; }
ok "402 with $(curl -sS -o /dev/null -D - -X POST "$CHAT_URL" -H 'content-type: application/json' -d "$BODY" | grep -ci '^www-authenticate\|^payment-required') payment headers"

step "Buyer pays with MPP"
OUT="$(buy --mpp "$BODY")" || { echo "$OUT"; tail -20 "$TMP/serve.log"; grep -i "warn\|error" "$TMP/connect.log" | tail -5; fail "mpp purchase failed"; }
echo "$OUT" | jq -e --arg ask "$ASK" '.choices[0].message.content | contains($ask)' >/dev/null \
  || { echo "$OUT"; fail "the answer does not echo the question"; }
ok "answer: $(jq -r '.choices[0].message.content' <<<"$OUT") (usage: $(jq -c .usage <<<"$OUT"))"

step "Buyer pays with x402"
OUT="$(buy --x402 "$BODY")" || { echo "$OUT"; tail -20 "$TMP/serve.log"; fail "x402 purchase failed"; }
echo "$OUT" | jq -e --arg ask "$ASK" '.choices[0].message.content | contains($ask)' >/dev/null \
  || { echo "$OUT"; fail "the answer does not echo the question"; }
ok "answer: $(jq -r '.choices[0].message.content' <<<"$OUT")"

step "Buyer streams"
STREAM_BODY="{\"model\":\"e2e-agent\",\"stream\":true,\"messages\":[{\"role\":\"user\",\"content\":\"$ASK\"}]}"
OUT="$(buy --mpp "$STREAM_BODY")" || { echo "$OUT"; tail -20 "$TMP/serve.log"; fail "streamed purchase failed"; }
grep -q '^data: \[DONE\]' <<<"$OUT" || { echo "$OUT"; fail "the stream did not end with [DONE]"; }
CHUNKS="$(grep -c '^data: {' <<<"$OUT")"
TEXT="$(grep '^data: {' <<<"$OUT" | sed 's/^data: //' | jq -rj '.choices[0].delta.content // empty')"
[ "$TEXT" = "$ASK" ] || { echo "$OUT"; fail "streamed text was '$TEXT'"; }
ok "$CHUNKS SSE chunks, text reassembles to the answer, usage $(grep '^data: {' <<<"$OUT" | sed 's/^data: //' | jq -c 'select(.usage) | .usage')"

# ── 7. The seller got paid: charge and upto settle at once, session channels
# after the idle close above, batch escrow at its own cadence. Wait for the
# seller's USDC to cover the three requests.
step "Seller balance"
EXPECTED="$(awk -v p="$PRICE" 'BEGIN{print p*3}')"
for i in $(seq 1 75); do
  BALANCE="$(seller_usdc)"
  if awk -v b="$BALANCE" -v e="$EXPECTED" 'BEGIN { exit !(b + 0 >= e - 1e-9) }'; then break; fi
  [ $((i % 5)) = 0 ] && echo "  seller USDC so far: $BALANCE (waiting for channel closes)"
  sleep 2
done
echo "  seller USDC: $BALANCE"
awk -v b="$BALANCE" -v e="$EXPECTED" 'BEGIN { exit !(b + 0 >= e - 1e-9) }' \
  || { grep -i "settle\|close\|distribut" "$TMP/connect.log" | tail -8 | cut -c1-300; fail "expected at least $EXPECTED USDC settled to the seller"; }
ok "three paid requests settled to the seller"

grep -c answered "$TMP/serve.log" | xargs -I{} echo "  worker answered {} requests"

# ── 8. The same through the MCP tool: `pay mcp` creates an endpoint, spawns
# its own worker, a buyer pays, then status and stop. A seller may hold one
# open endpoint at a time, so the CLI one is stopped first.
step "One open endpoint per seller"
CODE="$(curl -sS -o "$TMP/409.json" -w '%{http_code}' -X POST "$CONNECT/v1/endpoints" \
  -H "Authorization: Bearer $CREATOR" -H 'content-type: application/json' \
  -d "{\"pricing\":{\"per_request_usd\":0.02},\"model\":\"x\",\"recipient\":\"$SELLER\",\"network\":\"localnet\",\"earn_cap_usd\":1}")"
[ "$CODE" = 409 ] || { cat "$TMP/409.json"; fail "expected 409 for a second open endpoint, got $CODE"; }
curl -sS -f -X DELETE "$CONNECT/v1/endpoints/$ID" -H "Authorization: Bearer $OWNER_TOKEN" >/dev/null || fail "could not stop the CLI endpoint"
ok "second endpoint refused with 409; CLI endpoint $ID stopped"

step "sell_inference over MCP"
export PAY_CONNECT_URL="$CONNECT" PAY_CONNECT_TOKEN="$CREATOR" PAY_SELL_DIR="$TMP/sell" \
  SELL_RECIPIENT="$SELLER" MCP_LOG="$TMP/mcp.log"
MCP_ID="$(python3 "$ROOT/rust/crates/connect/dev/e2e-sell-mcp.py" "$BIN/pay" create 2>"$TMP/mcp-create.txt")" \
  || { cat "$TMP/mcp-create.txt"; tail -20 "$TMP/mcp.log"; fail "sell_inference create failed"; }
grep -q "counterpart\|Selling inference" "$TMP/mcp-create.txt" || { cat "$TMP/mcp-create.txt"; fail "unexpected tool output"; }
[ -f "$PAY_SELL_DIR/$MCP_ID.json" ] || fail "no sell record was written"
ok "endpoint $MCP_ID created and worker spawned by the tool"
sleep 2
MCP_URL="$CONNECT/endpoints/$MCP_ID/v1/chat/completions"
OUT="$("$BIN/pay" --local --mpp curl -sS -X POST "$MCP_URL" -H 'content-type: application/json' \
  -d '{"model":"mcp-agent","messages":[{"role":"user","content":"paid through the mcp endpoint"}]}')" \
  || { echo "$OUT"; cat "$PAY_SELL_DIR/$MCP_ID.log"; fail "purchase on the MCP endpoint failed"; }
echo "$OUT" | jq -e '.choices[0].message.content == "paid through the mcp endpoint"' >/dev/null \
  || { echo "$OUT"; fail "the MCP endpoint did not answer"; }
ok "buyer paid and got: $(jq -r '.choices[0].message.content' <<<"$OUT")"
STATUS="$(python3 "$ROOT/rust/crates/connect/dev/e2e-sell-mcp.py" "$BIN/pay" status "$MCP_ID")" || fail "status failed"
grep -q "worker: running" <<<"$STATUS" || { echo "$STATUS"; fail "status does not show a running worker"; }
grep -q 'earned: \$0.02 of \$0.03' <<<"$STATUS" || { echo "$STATUS"; fail "status does not show earnings against the cap"; }
ok "status: $(grep '^earned' <<<"$STATUS"), $(grep '^worker' <<<"$STATUS")"

# The second answered request crosses the $0.03 cap: the endpoint closes
# itself, buyers get 410 without being charged, and the worker exits.
OUT="$("$BIN/pay" --local --mpp curl -sS -X POST "$MCP_URL" -H 'content-type: application/json' \
  -d '{"model":"mcp-agent","messages":[{"role":"user","content":"second"}]}')" || fail "second purchase failed"
echo "$OUT" | jq -e '.choices[0].message.content == "second"' >/dev/null || { echo "$OUT"; fail "second answer wrong"; }
CODE="$(curl -sS -o "$TMP/410.json" -w '%{http_code}' -X POST "$MCP_URL" -H 'content-type: application/json' -d '{"model":"mcp-agent","messages":[]}')"
[ "$CODE" = 410 ] || { cat "$TMP/410.json"; fail "expected 410 after the cap, got $CODE"; }
for _ in $(seq 1 20); do grep -q "Done." "$PAY_SELL_DIR/$MCP_ID.log" && break; sleep 1; done
grep -q "Done." "$PAY_SELL_DIR/$MCP_ID.log" || { tail -5 "$PAY_SELL_DIR/$MCP_ID.log"; fail "the worker did not exit after the cap"; }
STATUS="$(python3 "$ROOT/rust/crates/connect/dev/e2e-sell-mcp.py" "$BIN/pay" status "$MCP_ID")" || fail "status failed"
grep -q "closed, the cap was reached" <<<"$STATUS" || { echo "$STATUS"; fail "status does not report the closed endpoint"; }
ok "cap reached: buyers get 410, worker exited, $(grep '^earned' <<<"$STATUS")"

STOP="$(python3 "$ROOT/rust/crates/connect/dev/e2e-sell-mcp.py" "$BIN/pay" stop "$MCP_ID")" || fail "stop failed"
grep -q "endpoint deleted" <<<"$STOP" || { echo "$STOP"; fail "stop did not delete the endpoint"; }
CODE="$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$MCP_URL" -H 'content-type: application/json' -d '{}')"
[ "$CODE" = 404 ] || fail "endpoint still answers ($CODE) after stop"
[ ! -f "$PAY_SELL_DIR/$MCP_ID.json" ] || fail "the sell record was not removed"
ok "stopped: worker killed, endpoint gone, record removed"
unset PAY_CONNECT_URL PAY_CONNECT_TOKEN PAY_SELL_DIR SELL_RECIPIENT MCP_LOG

echo
ok "sell_inference e2e passed"
