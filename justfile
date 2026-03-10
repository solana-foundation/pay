default:
    @just help

# Show available commands
help:
    @echo ""
    @echo "  @solana/pay"
    @echo "  ─────────────────────────────────────"
    @echo ""
    @echo "  Setup"
    @echo "    just install       Install dependencies"
    @echo "    just nuke          Nuke node_modules and reinstall"
    @echo ""
    @echo "  Dev"
    @echo "    just build         Build the core package"
    @echo "    just watch         Rebuild on change"
    @echo ""
    @echo "  Quality"
    @echo "    just test          Run tests"
    @echo "    just test-watch    Run tests in watch mode"
    @echo "    just lint          Check lint + formatting"
    @echo "    just fmt           Auto-fix formatting + lint"
    @echo "    just typecheck     Typecheck"
    @echo ""
    @echo "  CI / Release"
    @echo "    just ci            Full CI — lint, typecheck, test, build"
    @echo "    just clean         Clean build artifacts"
    @echo "    just release       Release build (clean + build)"
    @echo ""

# Install all dependencies
install:
    pnpm install

# Nuke node_modules and reinstall from scratch
nuke:
    rm -rf node_modules core/node_modules docs/node_modules examples/*/node_modules
    pnpm install

# Build the core package
build:
    pnpm --filter @solana/pay build

# Watch mode — rebuild on change
watch:
    pnpm --filter @solana/pay watch

# Run tests
test:
    pnpm --filter @solana/pay test

# Run tests in watch mode
test-watch:
    pnpm --filter @solana/pay test:watch

# Check lint + formatting
lint:
    pnpm --filter @solana/pay lint

# Auto-fix formatting + lint
fmt:
    pnpm --filter @solana/pay fmt

# Typecheck
typecheck:
    pnpm --filter @solana/pay typecheck

# Full CI check — lint, typecheck, test, build
ci: lint typecheck test build

# Clean build artifacts
clean:
    pnpm --filter @solana/pay clean

# Release build (clean + build)
release:
    pnpm --filter @solana/pay release
