# pay

**The missing payment layer for HTTP. `pay` auto-signs stablecoin transactions when APIs charge per request (x402, MPP).**

Wrap your CLI (`curl`, `claude`, `codex`, etc.) — when a stablecoin gated API returns 402, `pay` detects the payment protocol, signs a stablecoin transaction, and retries. The response lands on stdout as if nothing happened.

[Install](#installation) · [Quick Start](#quick-start) · [Docs](https://docs.solanapay.com)

</div>

---

```sh
# Without pay — you get a 402
curl https://payment-debugger.vercel.app/mpp/quote/AAPL

# With pay — it handles the 402 and you get the response
pay --sandbox curl https://payment-debugger.vercel.app/mpp/quote/AAPL
```

## Key Features

### 💵 Transparent 402 Handling

Wrap your CLI (`curl`, `claude`, `codex`, etc.) — when an API returns 402, `pay` detects the payment protocol, signs a stablecoin transaction, and retries. You get the response body. That's it.

Supports both live payment standards on Solana:
- **[MPP](https://paymentauth.org/draft-solana-charge-00.html/)** — Machine Payments Protocol
- **[x402](https://x402.org/)** — x402 Payment Protocol

Stablecoins deployed to Solana are supported out of the box.

### 🗺️ Skills — Discover Paid APIs

Browse, search, and install catalogs of paid API providers directly from the CLI.

```sh
pay skills search "gemini"          # find providers by keyword
pay skills endpoints stableenrich   # list all endpoints for a service
pay skills add org/catalog          # add a provider source (GitHub or URL)
pay skills update                   # refresh the local cache
```

### 🤖 AI-Native with MCP

`pay` ships with a built-in [MCP](https://modelcontextprotocol.io/) server, giving AI assistants the ability to make paid API calls on your behalf.

```sh
# Run Claude Code or Codex with pay injected automatically
pay --sandbox claude
pay --sandbox codex
```

### 🛠️ Payment debugging and simulations

`pay` ships with an embedded Payment Debugger — a local web UI that visualizes every 402 challenge-response cycle as a sequence diagram. See exactly which headers were sent, which protocol was used (MPP or x402), and where things went wrong.

Everything runs locally — no data leaves your machine.

```sh
# Start a gateway with the debugger on any API spec
pay server start --debugger spec.yml

# Or run the bundled demo (sandbox + debugger + sample endpoints)
pay server demo
```

A [public debugger](https://payment-debugger.vercel.app) is also available.

### 🔐 Secure Key Storage

Your keys never touch disk in plaintext. `pay` stores keypairs in:

- **macOS Keychain** with optional Touch ID biometric prompt (macOS)
- **Windows Credential Manager** with optional Windows Hello prompt (Windows)
- **GNOME Keyring** via Secret Service / polkit prompt (Linux)
- **1Password** vault via `op` CLI — auth handled by 1Password itself (cross-platform)
- **File-based** keypair for CI and scripting

The biometric/password prompt is controlled per-account by the `auth_required` setting — defaults to `true` on mainnet, `false` elsewhere.

```sh
pay setup    # Touch ID on macOS, Windows Hello on Windows, GNOME Keyring on Linux, or choose 1Password
```

## Installation

### Prebuilt Binaries

```sh
brew install pay
```

### From Source

```sh
git clone https://github.com/solana-foundation/pay.git
cd pay
just install-pay
```

### Verify

```sh
pay --version
```

## Quick Start

```sh
# 1. Generate a keypair (Touch ID protected on macOS)
pay setup

# 2. Make a paid API call (--sandbox uses an ephemeral funded keypair)
pay --sandbox curl https://payment-debugger.vercel.app/mpp/quote/AAPL

# 3. Or let your AI agent handle it
pay --sandbox claude
```

## Contributing

```sh
cd rust
just build   # release binary
just test    # all tests
just lint    # clippy (warnings = errors)
```

We welcome contributions — check [open issues](https://github.com/solana-foundation/pay/issues) to get started.

## Troubleshooting

### Linux: `pay topup` or `pay curl` errors with "auth failed"

GNOME Keyring auth uses polkit, which requires a one-time setup step:

```sh
sudo cp rust/config/polkit/sh.pay.unlock-keypair.policy /usr/share/polkit-1/actions/
```

This grants `pay` the right to prompt for your password or fingerprint before accessing the keypair.

## License

Apache-2.0 — see [LICENSE](./LICENSE).
