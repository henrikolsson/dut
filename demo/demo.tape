# Records the README demo; run demo/record.sh rather than this directly.
Output demo/demo.gif

Require bash
Set Shell bash
Set FontSize 15
Set Width 1400
Set Height 640
Set Padding 18
Set Theme "Catppuccin Mocha"
Set TypingSpeed 70ms
Set CursorBlink false

Hide
Type `export PATH="$PWD/target/release:$PATH" DUT_CACHE_DIR="$HOME/demo-cache" PS1='$ '; cd ~; clear`
Enter
Sleep 300ms
Show

Type "dut -x demo"
Sleep 400ms
Enter
Sleep 1.5s

# Tree: open Projects and one of its children. Changes since the last
# snapshot show in the +/- column.
Type "g"
Down 3
Sleep 300ms
Right
Sleep 500ms
Down
Sleep 300ms
Right
Sleep 1.2s
Screenshot demo/tree.png
Sleep 1.5s

# Treemap: move around, zoom in and back out
Type "2"
Sleep 1.2s
Screenshot demo/treemap.png
Sleep 1.5s
Right
Sleep 600ms
Down
Sleep 600ms
Enter
Sleep 1.5s
Backspace
Sleep 800ms

# Largest files and file types
Type "3"
Sleep 1.8s
Type "4"
Sleep 1.8s

# What changed since yesterday's snapshot
Type "5"
Sleep 1.2s
Screenshot demo/changes.png
Sleep 1.5s

# Size over two weeks of snapshots
Type "6"
Sleep 1.2s
Screenshot demo/history.png
Sleep 1.5s

# Deleting asks first
Type "1"
Sleep 600ms
Type "d"
Sleep 1.8s
Type "n"
Sleep 800ms
Type "q"
Sleep 500ms
