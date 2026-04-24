mod rs 'rust/Justfile'
mod ts 'typescript/Justfile'

default:
    @just help

# Show available commands
help:
    @echo ""
    @echo "  pay"
    @echo "  ─────────────────────────────────────"
    @echo ""
    @echo "  Top-level"
    @echo "    just install           Install all deps (pnpm + cargo)"
    @echo "    just lint              Lint everything"
    @echo "    just test              Test everything"
    @echo "    just build             Build everything"
    @echo "    just fmt               Format everything"
    @echo "    just ci                Full CI — lint, typecheck, test, build"
    @echo ""
    @echo "  Rust CLI (just rs <cmd>)"
    @echo "    just rs build          Build release binary"
    @echo "    just rs test           Run all tests"
    @echo "    just rs unit-test      Run unit tests only"
    @echo "    just rs integration-test  Run integration tests only"
    @echo "    just rs lint           Clippy (warnings = errors)"
    @echo "    just rs fmt            Format check"
    @echo "    just rs run            Run CLI (pass args after --)"
    @echo ""
    @echo "  TypeScript SDK (just ts <cmd>)"
    @echo "    just ts install        Install pnpm dependencies"
    @echo "    just ts nuke           Nuke node_modules and reinstall"
    @echo "    just ts build          Build the core package"
    @echo "    just ts watch          Rebuild on change"
    @echo "    just ts test           Run tests"
    @echo "    just ts test-watch     Run tests in watch mode"
    @echo "    just ts lint           Check lint + formatting"
    @echo "    just ts fmt            Auto-fix formatting + lint"
    @echo "    just ts typecheck      Typecheck"
    @echo "    just ts clean          Clean build artifacts"
    @echo "    just ts release        Release build (clean + build)"
    @echo ""

# ── Top-level ──────────────────────────────────

# Install all dependencies
install:
    cd typescript && pnpm install
    cd rust && cargo fetch

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

# Install the pay CLI binary
install-pay:
    cd pdb && pnpm install --frozen-lockfile && pnpm build
    cd rust && cargo cli-install

# Full CI — lint, typecheck, test, build
ci: lint test build
    cd typescript && pnpm --filter @solana/pay typecheck
