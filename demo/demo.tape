# VHS tape for the dda demo GIF — https://github.com/charmbracelet/vhs
#
# Regenerate with `mise run demo`, which builds the release binary, generates
# the (git-ignored) fixture tree at demo/fixture, then renders demo/demo.gif.
# Requires the `vhs` and `ttyd` tools — both are pinned in mise.toml.

Output demo/demo.gif

Set Shell bash
Set FontSize 15
Set Width 1200
Set Height 640
Set Padding 20

# --- setup (not recorded) ---
# Isolate the TUI's remembered view prefs so the demo starts from a known
# state every render, put the freshly-built binary on PATH, then scan inside
# the fixture so the paths read as project-relative.
Hide
Type `export XDG_STATE_HOME="$PWD/demo/.demo-state"; rm -rf "$XDG_STATE_HOME"` Enter
Type `export PATH="$PWD/target/release:$PATH"; cd demo/fixture; clear` Enter
Show

# Zero-config: scan the current directory. Results stream in and re-sort live.
Type "dda --icons emoji"
Sleep 750ms
Enter
Sleep 3s

# Mark the two biggest directories for deletion — the footer totals the space
# that would be freed.
Down
Sleep 500ms
Space
Sleep 900ms
Down
Sleep 500ms
Space
Sleep 1500ms

# Ask to delete: the confirmation dialog lists what goes and how much it frees.
Type "d"
Sleep 3s

# It's only a demo — cancel, then quit.
Escape
Sleep 1s
Type "q"
Sleep 750ms
