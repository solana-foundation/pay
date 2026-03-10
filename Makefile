.PHONY: install build test lint fmt typecheck clean watch dev

# Install all dependencies
install:
	pnpm install

# Build the core package
build:
	pnpm --filter @solana/pay build

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

# Watch mode (rebuild on change)
watch:
	pnpm --filter @solana/pay watch

# Clean build artifacts
clean:
	pnpm --filter @solana/pay clean

# Full CI check: lint + typecheck + test + build
ci: lint typecheck test build

# Nuke node_modules and reinstall
nuke:
	rm -rf node_modules core/node_modules docs/node_modules examples/*/node_modules
	pnpm install

# Release build (clean + build)
release:
	pnpm --filter @solana/pay release
