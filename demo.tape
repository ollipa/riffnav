# Reproducible demo recording for riffnav, using vhs (https://github.com/charmbracelet/vhs).
#
#   cargo install --path .          # make sure `riffnav` is on PATH
#   vhs demo.tape                   # writes demo.gif
#
# Run this from a git repository that has uncommitted changes across a few files,
# so `git diff` produces something interesting to navigate.

Output demo.gif

Set Shell "bash"
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

# Walk the tree.
Type@400ms "jjj"
Sleep 800ms

# Fuzzy-find a file.
Type "/"
Sleep 500ms
Type@150ms "parser"
Sleep 800ms
Enter
Sleep 1s

# Scroll the diff, flip to side-by-side, back.
Tab
Type@120ms "dddd"
Sleep 800ms
Type "s"
Sleep 1.5s
Type "s"
Sleep 800ms

# Help overlay, then quit.
Type "?"
Sleep 1.5s
Type "q"
Sleep 500ms
