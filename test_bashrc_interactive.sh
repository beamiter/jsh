#!/bin/bash

# Test .bashrc compatibility improvements - INTERACTIVE MODE
# This tests that aliases and env vars are loaded when jsh starts

TEST_DIR=$(mktemp -d)
TEST_BASHRC="$TEST_DIR/.bashrc"

cat > "$TEST_BASHRC" << 'EOF'
# Environment variable
export TEST_VAR="hello_world"
export MY_PATH="/custom/path"

# Aliases
alias ll='ls -la'
alias grep='grep --color=auto'
alias mytest='echo "test result: "'

# Shell options
shopt -s extglob
shopt -s dotglob

# Function (won't be fully imported yet, but shouldn't break)
function my_func() {
    echo "Hello from function: $1"
}
EOF

echo "=== Setup ==="
echo "Test .bashrc at: $TEST_BASHRC"

JSH_BIN="./target/debug/jsh"

# Helper function to test with jsh in interactive mode via echo
test_jsh_interactive() {
    local cmd="$1"
    local desc="$2"
    echo "Test: $desc"
    echo "  Command: $cmd"

    # Use echo to pipe command to jsh in interactive mode
    # This simulates user input in interactive shell
    HOME="$TEST_DIR" echo "$cmd" | $JSH_BIN 2>/dev/null | grep -v "^jsh>" || echo "  Result: (no output or failed)"
    echo ""
}

echo "=== Testing jsh interactive mode with custom .bashrc ==="
echo ""

# Test 1: Environment variables
test_jsh_interactive "echo \$TEST_VAR" "Environment variable TEST_VAR"
test_jsh_interactive "echo \$MY_PATH" "Environment variable MY_PATH"

# Test 2: Aliases
test_jsh_interactive "type ll" "Check if alias 'll' is registered"
test_jsh_interactive "alias" "List all aliases"

# Test 3: Shell options
test_jsh_interactive "shopt extglob" "Check extglob option"

# Cleanup
rm -rf "$TEST_DIR"

echo "=== Test Complete ==="
