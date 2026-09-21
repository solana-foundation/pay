#!/usr/bin/env bash
# Build and run pay-cloud locally with everything switched on: the
# onboarding pages, the Coinflow funding page, the MCP connector and its
# OAuth server. By default a mock Openfort runs beside it so the wallet
# flows work with no Openfort account.
#
#   rust/crates/cloud/dev/run.sh                 # mock Openfort, http://127.0.0.1:8402
#   rust/crates/cloud/dev/run.sh --real-openfort # use dashboard.openfort.io
#   rust/crates/cloud/dev/run.sh --public-url https://xyz.trycloudflare.com
#   rust/crates/cloud/dev/run.sh --static-token <token>   # header auth + a mock wallet for it
#   rust/crates/cloud/dev/run.sh --anonymous              # DEV ONLY: no-auth hosts act as that wallet
#   rust/crates/cloud/dev/run.sh --tunnel --anonymous     # Grok demo: quick tunnel + no-auth mock wallet
#   rust/crates/cloud/dev/run.sh --funnel --anonymous     # same, on a stable Tailscale Funnel hostname
#   rust/crates/cloud/dev/run.sh --funnel --funnel-port 8443 --port 8403 --mock-port 8498
#                                                 # a second instance beside the first (Funnel also
#                                                 # serves 8443 and 10000), e.g. Privy for Claude.ai
#
# Reads the repo-root .env (Coinflow sandbox settings) when present.
# Ctrl-C stops everything.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"
PORT=8402
MOCK_PORT=8499
PUBLIC_URL=""
REAL_OPENFORT=0
SKIP_BUILD=0
STATIC_TOKEN=""
ANONYMOUS=0
TUNNEL=0
FUNNEL=0
FUNNEL_PORT=443
FUNNEL_ACTIVE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --port) PORT="$2"; shift 2 ;;
    --public-url) PUBLIC_URL="$2"; shift 2 ;;
    --real-openfort) REAL_OPENFORT=1; shift ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --static-token) STATIC_TOKEN="$2"; shift 2 ;;
    --anonymous) ANONYMOUS=1; shift ;;
    --tunnel) TUNNEL=1; shift ;;
    --funnel) FUNNEL=1; shift ;;
    --funnel-port) FUNNEL_PORT="$2"; shift 2 ;;
    --mock-port) MOCK_PORT="$2"; shift 2 ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

step() { printf '\n\033[1m› %s\033[0m\n' "$*"; }

PIDS=()
cleanup() {
  if [ "$FUNNEL_ACTIVE" = 1 ]; then
    tailscale funnel --https="$FUNNEL_PORT" off >/dev/null 2>&1 || true
  fi
  for p in "${PIDS[@]:-}"; do
    [ -n "$p" ] && kill "$p" 2>/dev/null || true
  done
}
trap cleanup EXIT INT TERM

if [ "$TUNNEL" = 1 ]; then
  # A Cloudflare quick tunnel: public HTTPS with no account, a new hostname
  # each start, and it can vanish without notice, so the runner owns it and
  # pins pay-cloud to whatever hostname it got.
  command -v cloudflared >/dev/null || { echo "cloudflared not found: brew install cloudflared" >&2; exit 1; }
  step "Starting a Cloudflare quick tunnel to 127.0.0.1:$PORT"
  TUNNEL_LOG="$(mktemp -t pay-cloud-tunnel)"
  cloudflared tunnel --url "http://127.0.0.1:$PORT" > "$TUNNEL_LOG" 2>&1 &
  PIDS+=($!)
  for _ in $(seq 1 40); do
    PUBLIC_URL="$(grep -oE 'https://[a-z0-9-]+\.trycloudflare\.com' "$TUNNEL_LOG" | head -1 || true)"
    [ -n "$PUBLIC_URL" ] && break
    sleep 1
  done
  [ -n "$PUBLIC_URL" ] || { echo "the tunnel did not report a hostname (see $TUNNEL_LOG)" >&2; exit 1; }
fi

if [ "$FUNNEL" = 1 ]; then
  # Tailscale Funnel: a stable HTTPS hostname on the tailnet's domain that
  # survives restarts, unlike a quick tunnel. Needs `tailscale up` and
  # Funnel enabled for the tailnet (the command says how if it is not).
  command -v tailscale >/dev/null || { echo "tailscale not found" >&2; exit 1; }
  step "Exposing 127.0.0.1:$PORT through Tailscale Funnel on :$FUNNEL_PORT"
  tailscale funnel --bg --https="$FUNNEL_PORT" "$PORT" >/dev/null
  FUNNEL_ACTIVE=1
  HOST="$(tailscale status --json | python3 -c 'import sys,json; print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))')"
  PUBLIC_URL="https://$HOST"
  [ "$FUNNEL_PORT" != 443 ] && PUBLIC_URL="$PUBLIC_URL:$FUNNEL_PORT"
fi

PUBLIC_URL="${PUBLIC_URL:-http://127.0.0.1:$PORT}"

if [ "$SKIP_BUILD" = 0 ]; then
  step "Building the payment debugger web bundle"
  (cd "$ROOT/web-ui" && pnpm install --frozen-lockfile --silent && pnpm -s build >/dev/null)
  step "Building pay-cloud and the pay CLI"
  (cd "$ROOT/rust" && cargo build -q -p pay-cloud -p pay)
fi

if [ -f "$ROOT/.env" ]; then
  set -a; . "$ROOT/.env"; set +a
  step "Loaded $ROOT/.env (Coinflow: ${COINFLOW_ENV:-unset}; Privy: ${PRIVY_APP_ID:-off})"
fi

if [ "$REAL_OPENFORT" = 0 ]; then
  step "Starting mock Openfort on http://127.0.0.1:$MOCK_PORT"
  python3 "$ROOT/rust/crates/cloud/dev/mock_openfort.py" "$MOCK_PORT" &
  PIDS+=($!)
  # pay-cloud may provision dev wallets against the mock at startup.
  for _ in $(seq 1 50); do
    curl -fsS -o /dev/null "http://127.0.0.1:$MOCK_PORT/v2/accounts" 2>/dev/null && break
    sleep 0.2
  done
  export OPENFORT_BASE_URL="http://127.0.0.1:$MOCK_PORT"
  export OPENFORT_AUTH_PAGE_URL="http://127.0.0.1:$MOCK_PORT"
else
  unset OPENFORT_BASE_URL OPENFORT_AUTH_PAGE_URL
fi

export PAY_CLOUD_MCP=1
export RUST_LOG="${RUST_LOG:-info,pay_cloud=debug}"
if [ "$ANONYMOUS" = 1 ] && [ -z "$STATIC_TOKEN" ]; then
  STATIC_TOKEN="pay_dev_$(openssl rand -hex 16)"
fi
if [ -n "$STATIC_TOKEN" ]; then
  # A header-authenticated host; with the mock, the token gets a wallet too.
  export PAY_CLOUD_MCP_TOKENS="$STATIC_TOKEN"
  [ "$REAL_OPENFORT" = 0 ] && export PAY_CLOUD_DEV_MOCK_TENANTS=1
fi
if [ "$ANONYMOUS" = 1 ]; then
  # DEV ONLY: hosts that send no header and cannot finish OAuth act as the
  # static token's tenant. Anyone with the URL can use that mock wallet.
  export PAY_CLOUD_DEV_ANONYMOUS_TOKEN="$STATIC_TOKEN"
fi

step "Starting pay-cloud on http://127.0.0.1:$PORT (public URL $PUBLIC_URL)"
"$ROOT/rust/target/debug/pay-cloud" --port "$PORT" --public-url "$PUBLIC_URL" &
PIDS+=($!)
for _ in $(seq 1 50); do
  curl -fsS -o /dev/null "http://127.0.0.1:$PORT/health" 2>/dev/null && break
  sleep 0.2
done
curl -fsS -o /dev/null "http://127.0.0.1:$PORT/health" 2>/dev/null || { echo "pay-cloud did not start; its output is above" >&2; exit 1; }

cat <<EOF

────────────────────────────────────────────────────────────────────────
  pay-cloud is up.  $PUBLIC_URL
────────────────────────────────────────────────────────────────────────

$( [ -n "$STATIC_TOKEN" ] && printf '  Header-authenticated host (Grok custom connector, "headers" field):\n    Authorization: Bearer %s\n\n' "$STATIC_TOKEN" )  MCP connector (what Grok would use), with Claude Code as the host:
    claude mcp add --transport http paycloud $PUBLIC_URL/mcp
    then in Claude Code:  /mcp  → paycloud → Authenticate
    The browser lands on the consent page, creates a wallet$( [ "$REAL_OPENFORT" = 0 ] && printf ' (mock Openfort)' ), and returns.

  CLI, remote wallet setup (with the mock, use a scratch HOME so mock
  credentials never land in your real keychain):
    HOME=\$(mktemp -d) PAY_CLOUD_LOCAL=1 $ROOT/rust/target/debug/pay setup --backend cloud

  CLI, buy USDC with a card (Coinflow sandbox, test card 4242 4242 4242 4242):
    PAY_ONRAMP=coinflow PAY_CLOUD_LOCAL=1 $ROOT/rust/target/debug/pay topup

  Pages:  ${PAY_CLOUD_PAGES_URL:-https://pay.sh}/connect   ${PAY_CLOUD_PAGES_URL:-https://pay.sh}/onramp
  OAuth:  $PUBLIC_URL/.well-known/oauth-authorization-server
$( if [ -n "${PRIVY_APP_ID:-}" ]; then printf '  Privy:  consent page signs users in with app %s; wallets get signer %s\n' "$PRIVY_APP_ID" "${PRIVY_SIGNER_ID:-?}"; else printf '  Privy:  off (set PRIVY_APP_ID, PRIVY_APP_SECRET, PRIVY_VERIFICATION_KEY,\n          PRIVY_AUTHORIZATION_PRIVATE_KEY, PRIVY_SIGNER_ID in .env)\n'; fi )

$( if [ "$FUNNEL" = 1 ]; then printf '  Grok custom connector URL:  %s/mcp   (stable: Tailscale Funnel)\n' "$PUBLIC_URL"; elif [ "$TUNNEL" = 1 ]; then printf '  Grok custom connector URL:  %s/mcp\n  (quick tunnels get a new hostname each start; re-add the connector after a restart)\n' "$PUBLIC_URL"; else printf '  For Grok itself you need a public HTTPS URL: rerun with --funnel (Tailscale) or --tunnel (cloudflared),\n  or pass --public-url https://<your-host> behind your own proxy.\n'; fi )
  Ctrl-C stops everything.
EOF

wait
