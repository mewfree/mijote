#!/usr/bin/env bash
# Sourced by demo.tape from the repo root. Builds a throwaway git repo and
# puts scripted fake agents (demo-agent.py) on PATH; nothing real is touched.
SRC=$PWD
DEMO=/tmp/mijote-demo

rm -rf "$DEMO"
mkdir -p "$DEMO/bin" "$DEMO/home" "$DEMO/repo/src"

printf '#!/bin/sh\nexec python3 %s "Claude Code"\n' "$SRC/demo-agent.py" > "$DEMO/bin/claude-agent-acp"
printf '#!/bin/sh\nexec python3 %s "Gemini CLI"\n' "$SRC/demo-agent.py" > "$DEMO/bin/gemini"
chmod +x "$DEMO/bin/claude-agent-acp" "$DEMO/bin/gemini"

export MIJOTE_HOME="$DEMO/home"
export PATH="$SRC/target/release:$DEMO/bin:$PATH"

cd "$DEMO/repo" || return 1
git init -q
git config user.email demo@example.com
git config user.name demo
cat > src/client.rs << 'EOF'
pub fn fetch(url: &str) -> Result<String, Error> {
    http::get(url)
}
EOF
git add -A && git commit -q -m "initial commit"
PS1='$ '
