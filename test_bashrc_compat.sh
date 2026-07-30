#!/bin/bash

# Test .bashrc compatibility improvements
# Create a temporary test bashrc with aliases, env vars, and shopts

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
shopt -s nullglob

# This shouldn't be imported but shouldn't break either
function my_func() {
    echo "Hello from function: $1"
    return 0
}
EOF

echo "=== Test Setup ==="
echo "Test bashrc created at: $TEST_BASHRC"
echo "Content:"
cat "$TEST_BASHRC"
echo ""

# Test with current jsh build
JSH_BIN="./target/debug/jsh"

echo "=== Testing jsh with custom .bashrc ==="
# We need to trick jsh into using our test bashrc
# This is done via HOME environment variable
export HOME="$TEST_DIR"

# Test 1: Check if environment variables are imported
echo "Test 1: Environment variables"
echo "Command: echo \$TEST_VAR"
$JSH_BIN -c 'echo $TEST_VAR' 2>/dev/null || echo "FAIL: Could not run jsh"

echo "Command: echo \$MY_PATH"
$JSH_BIN -c 'echo $MY_PATH' 2>/dev/null || echo "FAIL: Could not run jsh"

# Test 2: Check if aliases are imported
echo ""
echo "Test 2: Aliases"
echo "Command: ll"
# Create a test directory structure
mkdir -p "$TEST_DIR/testdir"
touch "$TEST_DIR/testdir/file1.txt"
touch "$TEST_DIR/testdir/.hidden"
cd "$TEST_DIR/testdir"
$JSH_BIN -c 'll' 2>/dev/null | head -3 || echo "FAIL: Alias not working"

echo ""
echo "Test 3: Alias with arguments"
echo "Command: mytest"
$JSH_BIN -c 'mytest' 2>/dev/null || echo "FAIL: Alias expansion failed"

echo ""
echo "Test 4: Alias expansion in pipelines"
echo "Command: echo hello | grep ."
$JSH_BIN -c 'echo hello | grep .' 2>/dev/null || echo "FAIL: Alias in pipeline failed"

# Test 5: Built-in commands still work
echo ""
echo "Test 5: Built-in commands still work"
echo "Command: pwd"
$JSH_BIN -c 'pwd' 2>/dev/null || echo "FAIL: pwd failed"

# Cleanup
rm -rf "$TEST_DIR"

echo ""
echo "=== Test Complete ==="
