mod rs 'rust/Justfile'
mod ts 'typescript/Justfile'

default:
    @just --list --unsorted

# Install a target: `just install pay`, `just install pay <cargo install args...>`, `just install deps`
[positional-arguments]
install *args:
    #!/usr/bin/env bash
    set -euo pipefail

    target='deps'
    if [ "$#" -gt 0 ]; then
        target="$1"
        shift
    fi

    build_pdb() {
        if [ -n "${PAY_PDB_DIST:-}" ]; then
            echo "Using prebuilt PDB assets from PAY_PDB_DIST=${PAY_PDB_DIST}"
            return
        fi
        cd pdb
        pnpm install --frozen-lockfile
        pnpm build
        cd ..
    }

    case "${target}" in
        pay)
            build_pdb
            if [ "$#" -gt 0 ]; then
                doppler run -- cargo install "$@"
            else
                cd rust && doppler run -- cargo install --path crates/cli --locked --force
            fi
            ;;
        deps)
            if [ "$#" -gt 0 ]; then
                echo "install deps does not accept extra arguments"
                exit 1
            fi
            cd typescript && pnpm install
            cd ../rust && cargo fetch
            ;;
        *)
            echo "Unknown target: ${target}"
            echo "Usage: just install pay [cargo install args...] | just install deps"
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

# Build a target: `just build`, `just build pay`, `just build pdb`
build target='all':
    #!/usr/bin/env bash
    set -euo pipefail

    build_pdb() {
        if [ -n "${PAY_PDB_DIST:-}" ]; then
            echo "Using prebuilt PDB assets from PAY_PDB_DIST=${PAY_PDB_DIST}"
            return
        fi
        cd pdb
        pnpm install --frozen-lockfile
        pnpm build
        cd ..
    }

    case "{{ target }}" in
        all)
            cd typescript && pnpm --filter @solana/pay build && cd ..
            build_pdb
            cd rust && just build
            ;;
        pay)
            build_pdb
            cd rust && just build
            ;;
        pdb)
            build_pdb
            ;;
        rust)
            cd rust && just build
            ;;
        typescript|ts)
            cd typescript && pnpm --filter @solana/pay build
            ;;
        *)
            echo "Unknown build target: {{ target }}"
            echo "Usage: just build [all|pay|pdb|rust|typescript]"
            exit 1
            ;;
    esac

# Format everything
fmt:
    cd typescript && pnpm --filter @solana/pay fmt
    cd rust && cargo fmt --check

# Full CI — lint, typecheck, test, build
ci: lint test build
    cd typescript && pnpm --filter @solana/pay typecheck
