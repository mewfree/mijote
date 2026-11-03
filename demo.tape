Output demo.gif

Set Shell "bash"
Set FontSize 14
Set Width 1200
Set Height 700
Set Theme "Dracula"
Set TypingSpeed 60ms

# --- hidden setup (run from the repo root: `make demo`) ---
Hide
Type "source demo-setup.sh && clear"
Enter
Sleep 1500ms
Show

Type "mijote"
Enter
Sleep 1500ms

# New isolated session (own git worktree) -> pick Claude Code
Type "n"
Sleep 1000ms
Enter
Sleep 1500ms

# Prompt, vim style
Type "add retry with backoff to fetch"
Sleep 600ms
Enter
Sleep 4500ms

# Permission modal: allow the edit
Type "1"
Sleep 6000ms

# Second agent in parallel, then jump back with the switcher
Escape
Type " n"
Sleep 800ms
Type "j"
Sleep 600ms
Enter
Sleep 1500ms
Type "summarize the repo"
Sleep 500ms
Enter
Sleep 4500ms
Type "1"
Sleep 3500ms
Escape
Tab
Sleep 1200ms
Type "k"
Sleep 800ms
Enter
Sleep 2000ms

# Quit
Escape
Type " q"
Sleep 1000ms
