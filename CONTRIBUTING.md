# Contributing

Pay is developed in public and we appreciate contributions.

## Important: Branch Targeting

The `main` branch is the integration branch. All feature work and bug fixes should target `main`.

### Prerequisites

- [Just](https://github.com/casey/just) (command runner)
- Rust 1.86+
- Solana CLI 2.2+
- Node.js 20+ and pnpm (for SDK)

## Getting Started

Install all dependencies:

```shell
just install
```

## Rust CLI

```shell
just rs build              # Build release binary
just rs lint               # Clippy (warnings = errors)
just rs fmt                # Format check
just rs test               # Run all tests
just rs unit-test          # Unit tests only
just rs integration-test   # Integration tests only
just rs run -- --help      # Run the CLI
```

## TypeScript SDK

```shell
just ts install            # Install pnpm dependencies
just ts build              # Build the core package
just ts lint               # Check lint + formatting
just ts fmt                # Auto-fix formatting + lint
just ts typecheck          # Typecheck
just ts test               # Run tests
just ts test-watch         # Run tests in watch mode
```

## Before Submitting

- Run `just ci` (full lint, typecheck, test, build for both Rust and TypeScript)
- Use [conventional commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `chore:`, etc.)
