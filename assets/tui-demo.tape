# bouncy scrape --tui demo recording.
#
# Render with:  vhs assets/tui-demo.tape
# Requires:     vhs (https://github.com/charmbracelet/vhs)
#               bouncy in $PATH (cargo install bouncy-cli)
#
# Output:       assets/tui-demo.gif

Output assets/tui-demo.gif

Set FontSize 14
Set Width 1100
Set Height 600
Set Theme "Catppuccin Mocha"
Set Padding 18

# Local-dev convenience: if you're rendering this against a build
# from `target/release` instead of an installed `bouncy`, uncomment
# the next block to put the binary on PATH inside vhs's spawned
# shell (the lines stay hidden in the recording).
# Hide
# Type "export PATH=$PWD/target/release:$PATH" Enter
# Type "clear" Enter
# Show

Sleep 600ms

Type@25ms "bouncy scrape https://example.com https://example.org https://example.net https://www.rust-lang.org --tui"
Sleep 400ms
Enter

# Dashboard runs. With 4 URLs and --concurrency 8 (default), the
# scrape typically finishes in ~300 ms (release build). Pad to 5s
# so the final state — filled URL list, populated p50/p95/max,
# status histogram bars — is visible for several seconds.
Sleep 5s

Type "q"
Sleep 600ms
