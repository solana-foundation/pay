---
workspace: pay

# =============================================================================
# Skill directories
# =============================================================================
# Search paths for skill files (first match wins for duplicate names)
# Supports: ./relative, ~/home, /absolute paths

skills:
  - path: ./skills
  - path: ~/.config/axel/skills

# =============================================================================
# Layouts
# =============================================================================

layouts:
  # ---------------------------------------------------------------------------
  # Pane definitions
  # ---------------------------------------------------------------------------
  panes:
    # Claude Code - AI coding assistant
    - type: claude
      color: gray
      skills:
        - "*"

    # Codex - OpenAI coding assistant
    - type: codex
      color: green
      skills:
        - "*"

    # Free shell for ad-hoc commands
    - type: custom
      name: shell
      color: yellow
      notes:
        - "$ just --list           # show available commands"
        - "$ just run pay whoami   # run a pay subcommand"
        - "$ pay server demo       # gateway + debugger + sample endpoints"

    # Dev server / build watcher
    - type: custom
      name: dev_server
      color: orange
      notes:
        - "$ cd rust && cargo watch -x 'build -p pay'"
        - "$ just test             # run all tests"
        - "$ just lint             # clippy + pnpm lint"

  # ---------------------------------------------------------------------------
  # Grid layouts
  # ---------------------------------------------------------------------------
  grids:
    # Default - Claude on the left, shell + dev_server stacked on the right
    default:
      type: tmux
      claude:
        col: 0
        row: 0
        width: 50
      shell:
        col: 1
        row: 0
        height: 40
      dev_server:
        col: 1
        row: 1
        height: 60

    # Dual-agent layout - Claude and Codex side by side, shell underneath
    # dual:
    #   type: tmux
    #   claude:
    #     col: 0
    #     row: 0
    #     width: 40
    #   codex:
    #     col: 1
    #     row: 0
    #     width: 40
    #   shell:
    #     col: 2
    #     row: 0
    #     width: 20
---

# pay

The missing payment layer for HTTP. `pay` handles x402 and MPP payment
challenges with user-authorized stablecoin signing — wrapping CLIs like
`curl`, `claude`, and `codex` so that a 402 response triggers a local,
biometric-gated stablecoin payment and a transparent retry.

## Getting Started

```sh
just install deps        # fetch Rust + pnpm dependencies
just install pay         # build and install the `pay` binary
pay setup                # provision a local wallet (Touch ID on macOS)
pay --sandbox curl https://debugger.pay.sh/mpp/quote/AAPL
```

## Architecture

- `rust/` — Cargo workspace; the `pay` CLI, gateway, MCP server, and core
  payment-protocol crates (x402 + MPP) live here. Built with `cargo` /
  `just build pay`.
- `typescript/` — pnpm workspace publishing `@solana/pay` and supporting
  packages.
- `pdb/` — Payment Debugger web UI bundled into the gateway.
- `gateway/` — gateway server configs and demo specs.
- `skills/` — Axel/Claude skills surfaced in agent panes.
- `docs/` — user-facing docs (mirrored at https://docs.solanapay.com).

## Key Files

- `Justfile` — top-level entry point: `just build`, `just test`, `just lint`,
  `just ci`, `just run <bin> ...`.
- `rust/Cargo.toml` — workspace manifest for the Rust crates.
- `typescript/pnpm-workspace.yaml` — TS package layout.
- `pay-demo.yaml` — sample API spec used by `pay server demo`.
- `README.md` — install + quick-start instructions.
