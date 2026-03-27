# pay

**The missing [HTTP 402](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Status/402) client.**

Wrap `curl` or `wget` — when an API returns 402, `pay` detects the payment protocol, signs a Solana transaction, and retries. The response lands on stdout as if nothing happened.

[Install](#installation) · [Quick Start](#quick-start) · [Docs](https://docs.solanapay.com)

</div>

---

```sh
# Without pay — you get a 402
curl https://402-demo-api.vercel.app/

# With pay — it handles the 402 and you get the response
pay --dev curl https://402-demo-api.vercel.app/
```

## Key Features

### Transparent 402 Handling

Wrap `curl` or `wget` — when an API returns 402, `pay` detects the payment protocol, signs the transaction, and retries. You get the response body. That's it.

Supports both live payment standards on Solana:
- **[MPP](https://mpp.dev/)** — Machine Payments Protocol
- **[x402](https://x402.org/)** — x402 Payment Protocol

SOL and SPL tokens (USDC, USDT, etc.) are supported out of the box.

### Touch ID, GNOME Keyring & 1Password Key Storage

Your keys never touch disk in plaintext. `pay` stores keypairs in:

- **macOS Keychain** with Touch ID biometric protection
- **GNOME Keyring** with password/fingerprint prompt on every use (Linux)
- **1Password** vault integration (cross-platform)
- **File-based** fallback for CI and scripting

```sh
pay setup    # Touch ID on macOS, GNOME Keyring on Linux, or choose 1Password
```

> **Linux note:** GNOME Keyring auth uses polkit, which requires a one-time setup step:
> ```sh
> sudo cp rust/config/polkit/sh.pay.unlock-keypair.policy /usr/share/polkit-1/actions/
> ```
> This grants `pay` the right to prompt for your password or fingerprint before
> accessing the keypair. Without it, `pay topup` and `pay curl` will error.

### Session Budgets via TUI

Set a spending cap and expiration before making requests. The interactive TUI lets you control exactly how much you're willing to spend per session — no surprise charges.

### AI-Native with MCP

`pay` ships with a built-in [MCP](https://modelcontextprotocol.io/) server, giving AI assistants the ability to make paid API calls on your behalf.

```sh
# Run Claude Code or Codex with pay injected automatically
pay --dev claude
pay --dev codex
```

### Dev Mode

Get started instantly with an ephemeral keypair auto-funded via [Surfpool](https://github.com/txtx/surfpool). No setup, no mainnet tokens needed.

```sh
# Uses public devnet (402.surfnet.dev) by default
pay --dev curl https://402-demo-api.vercel.app/mpp/quote/SOL

# Or use a local Surfpool instance
pay --dev --local curl http://localhost:3402/mpp/quote/SOL
```

## Installation

### From Source

```sh
git clone https://github.com/solana-foundation/pay.git
cd pay/rust
cargo install --path crates/cli
```

**Linux only** — install the polkit action to enable keypair auth:

```sh
sudo cp rust/config/polkit/sh.pay.unlock-keypair.policy /usr/share/polkit-1/actions/
```

### Verify

```sh
pay --version
```

## Quick Start

```sh
# 1. Generate a keypair (Touch ID protected on macOS)
pay setup

# 2. Make a paid API call (--dev uses an ephemeral funded keypair)
pay --dev curl https://402-demo-api.vercel.app/

# 3. Or let your AI agent handle it
pay --dev claude
```

## Contributing

```sh
cd rust
just build   # release binary
just test    # all tests
just lint    # clippy (warnings = errors)
```

We welcome contributions — check [open issues](https://github.com/solana-foundation/pay/issues) to get started.

## License

Apache-2.0 — see [LICENSE](./LICENSE).
