mod rs 'rust/Justfile'
mod ts 'typescript/Justfile'

default:
    @just --list --unsorted

# Install a target: `just install pay`, `just install deps`
install target='deps':
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{target}}" in
        pay)
            cd pdb && pnpm install --frozen-lockfile && pnpm build
            cd ../rust && cargo cli-install
            ;;
        deps)
            cd typescript && pnpm install
            cd ../rust && cargo fetch
            ;;
        *)
            echo "Unknown target: {{target}}"
            echo "Usage: just install pay | just install deps"
            exit 1
            ;;
    esac

# Lint everything
lint:
    cd typescript && pnpm --filter @solana/pay lint
    cd rust && cargo clippy --workspace --all-targets -- -D warnings

# Test everything
test:
    cd typescript && pnpm --filter @solana/pay test
    cd rust && cargo test --workspace

# Build everything
build:
    cd typescript && pnpm --filter @solana/pay build
    cd rust && cargo build --release

# Format everything
fmt:
    cd typescript && pnpm --filter @solana/pay fmt
    cd rust && cargo fmt --check

# Full CI — lint, typecheck, test, build
ci: lint test build
    cd typescript && pnpm --filter @solana/pay typecheck
