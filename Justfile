mod rs 'rust/Justfile'
mod ts 'typescript/Justfile'

default:
    @just --list --unsorted

# Run a workspace binary, forwarding subcommands.
# Examples:
#   just run pay whoami
#   just run pay accounts
#   just run pay account new ludo
run BIN *ARGS:
    cd rust && cargo run -p {{BIN}} -- {{ARGS}}

# Install a target: `just install pay`, `just install pay <cargo install args...>`, `just install deps`
[positional-arguments]
install *args:
    #!/usr/bin/env bash
    set -euo pipefail

    if [ "$#" -eq 0 ]; then
        target='deps'
    else
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
            {{ just_executable() }} _check-native-build-deps
            build_pdb
            if [ "$#" -gt 0 ]; then
                cargo install "$@"
            else
                cd rust && cargo cli-install
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

# Update a target: `just update pay`
update target:
    #!/usr/bin/env bash
    set -euo pipefail

    case "{{ target }}" in
        pay)
            git pull
            just install pay
            pay setup --update
            ;;
        *)
            echo "Unknown update target: {{ target }}"
            echo "Usage: just update pay"
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
            {{ just_executable() }} _check-native-build-deps
            build_pdb
            cd rust && cargo build --release
            ;;
        pay)
            {{ just_executable() }} _check-native-build-deps
            build_pdb
            cd rust && cargo build --release
            ;;
        pdb)
            build_pdb
            ;;
        rust)
            {{ just_executable() }} _check-native-build-deps
            cd rust && cargo build --release
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

_check-native-build-deps:
    #!/usr/bin/env bash
    set -euo pipefail

    missing=()
    for tool in cc make cmake; do
        if ! command -v "${tool}" >/dev/null 2>&1; then
            missing+=("${tool}")
        fi
    done

    if [ "${#missing[@]}" -eq 0 ]; then
        exit 0
    fi

    echo "Missing native build tool(s): ${missing[*]}"
    echo
    echo "pay currently builds native TLS/compression dependencies through Pingora/rustls."
    case "$(uname -s)" in
        Darwin)
            echo "Install Xcode Command Line Tools for cc/make:"
            echo "  xcode-select --install"
            echo "Install CMake with Homebrew:"
            echo "  brew install cmake"
            ;;
        Linux)
            echo "Debian/Ubuntu:"
            echo "  sudo apt-get install build-essential cmake pkg-config"
            echo "Fedora:"
            echo "  sudo dnf install gcc gcc-c++ make cmake pkgconf-pkg-config"
            ;;
        *)
            echo "Install a C compiler, make, and cmake for your platform."
            ;;
    esac
    exit 1

# Format everything
fmt:
    cd typescript && pnpm --filter @solana/pay fmt
    cd rust && cargo fmt --check

# Full CI — lint, typecheck, test, build
ci: lint test build
    cd typescript && pnpm --filter @solana/pay typecheck
