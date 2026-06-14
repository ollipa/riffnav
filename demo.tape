# Reproducible demo recording for riffnav, using vhs (https://github.com/charmbracelet/vhs).
#
#   cargo install --path .          # make sure `riffnav` is on PATH
#   vhs demo.tape                   # writes demo.gif
#
# Run this from a git repository that has uncommitted changes across a few files,
# so `git diff` produces something interesting to navigate.

Output demo.gif

Set Shell "bash"
# riffnav's default icons are Nerd Font glyphs, so the recording font must be a
# Nerd Font — otherwise they render as tofu boxes. Change this to whichever Nerd
# Font you have installed (or press `i` in the app / set icon_style = "unicode"
# to avoid Nerd glyphs entirely).
Set FontFamily "ZedMono Nerd Font Mono"
Set FontSize 16
Set Width 1100
Set Height 640
Set Theme "Catppuccin Mocha"
Set Padding 16

# Launch into the diff.
Hide
Type "git diff | riffnav"
Enter
Sleep 1.5s
Show

# Walk down the tree — the diff on the right updates as the selection moves.
Type@450ms "jjj"
Sleep 800ms

# Jump to the top folder and fold / unfold it.
Type@250ms "kkkkk"
Sleep 500ms
Enter
Sleep 1s
Enter
Sleep 800ms

# Fuzzy-find a file by name.
Type "/"
Sleep 500ms
Type@150ms "stats"
Sleep 800ms
Enter
Sleep 1s

# Focus the diff and scroll it.
Tab
Sleep 400ms
Ctrl+D
Sleep 700ms
Ctrl+D
Sleep 700ms
Ctrl+U
Sleep 700ms

# Flip to side-by-side and back, then cycle icon styles.
Type "s"
Sleep 1.5s
Type "s"
Sleep 800ms
Type "i"
Sleep 1s

# Show the help overlay, close it, and quit.
Type "?"
Sleep 1.5s
Escape
Sleep 400ms
Type "q"
Sleep 500ms
