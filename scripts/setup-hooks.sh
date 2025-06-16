#!/bin/bash

# Setup git hooks for the project

HOOKS_DIR=".git/hooks"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "Setting up git hooks for profile_aggregator..."

# Create hooks directory if it doesn't exist
mkdir -p "$PROJECT_ROOT/$HOOKS_DIR"

# Create pre-commit hook
cat > "$PROJECT_ROOT/$HOOKS_DIR/pre-commit" << 'EOF'
#!/bin/bash

echo "Running pre-commit checks..."

# Check if we're in the right directory
if [ ! -f "Cargo.toml" ]; then
    echo "Error: Not in a Rust project directory"
    exit 1
fi

# Run cargo fmt check
echo "Checking code formatting..."
if ! cargo fmt -- --check; then
    echo "❌ Code formatting check failed!"
    echo "Run 'cargo fmt' to fix formatting issues"
    exit 1
fi
echo "✓ Formatting check passed"

# Run clippy
echo "Running clippy linter..."
if ! cargo clippy --all-targets -- -D warnings; then
    echo "❌ Clippy check failed!"
    echo "Fix the clippy warnings before committing"
    exit 1
fi
echo "✓ Clippy check passed"

# Run tests (optional, can be slow)
# Uncomment if you want to run tests before every commit
# echo "Running tests..."
# if ! cargo test; then
#     echo "❌ Tests failed!"
#     exit 1
# fi
# echo "✓ Tests passed"

echo "✅ All pre-commit checks passed!"
EOF

# Make the hook executable
chmod +x "$PROJECT_ROOT/$HOOKS_DIR/pre-commit"

echo "✅ Git hooks installed successfully!"
echo ""
echo "The pre-commit hook will now run:"
echo "  • cargo fmt --check"
echo "  • cargo clippy -- -D warnings"
echo ""
echo "To skip the pre-commit hook (not recommended), use:"
echo "  git commit --no-verify"