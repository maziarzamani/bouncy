# bouncy — Show HN demo tape.
#
# A VHS tape: https://github.com/charmbracelet/vhs
# Run `vhs demo.tape` from the repo root. Produces `assets/demo.gif`.
#
# Goal: a 30-second loop demonstrating the three modes — scrape, browse,
# drive. Speed is the headline, so we lean on `time` and explicit timings.
#
# Pre-conditions (do this BEFORE recording — the build is silent and
# slow, you don't want it inside the gif):
#
#     # build the binaries from this checkout, install over whatever's
#     # in $PATH so the demo lines below pick up the latest features
#     # (click_text, --select, indexed snapshots).
#     cargo install --path crates/bouncy-cli --force
#     cargo install --path crates/bouncy-mcp --force
#
# Then: `vhs promo/demo.tape`.

Output assets/demo.gif

Set FontSize 16
Set Width 1200
Set Height 700
Set Theme "Catppuccin Macchiato"
Set TypingSpeed 35ms
Set PlaybackSpeed 1.0

# ─── 1. Headline (1s) ──────────────────────────────────────────────
Type "# bouncy — tiny Rust headless browser. no Chromium."
Sleep 1s
Enter

# ─── 2. Scrape: time + dump in one shot (5s) ───────────────────────
# `--dump text` works on every released version. The `time` output is
# the actual headline — milliseconds, no Chromium boot.
Type "# scrape — static page, no V8 boot"
Enter
Sleep 500ms
Type "time bouncy fetch https://example.com --dump text"
Enter
Sleep 3s

# ─── 3. Browse: stateful session with click_text (8s) ──────────────
# click_text is post-PR-#36; requires a fresh `cargo install --path …
# --force` if your $PATH binary is older than that.
Type "# browse — stateful session, click by visible text"
Enter
Sleep 500ms
Type 'bouncy browse https://example.com --do "click_text Learn more"'
Enter
Sleep 5s

# ─── 4. Indexed interactive elements (8s) ──────────────────────────
Type "# every interactive element gets a stable index — addressable from"
Enter
Type "# the LLM without guessing selectors. snapshot.interactive[N]:"
Enter
Sleep 500ms
Type "bouncy browse https://example.com --do snapshot --json | jq '.interactive'"
Enter
Sleep 4s

# ─── 5. MCP / autonomous drive (5s) ────────────────────────────────
Type "# bouncy-mcp serves the same primitives as MCP tools."
Enter
Type "# Claude Desktop / Code / Cursor drive the loop end-to-end:"
Enter
Sleep 500ms
Type "  bouncy_browse_open  → snapshot + session_id"
Enter
Sleep 300ms
Type "  bouncy_browse_chain → batch click/fill/submit/done in one round trip"
Enter
Sleep 2s

# ─── 6. Outro (1s) ─────────────────────────────────────────────────
Type "# 40 MB binary. 30 ms cold start. github.com/maziarzamani/bouncy"
Enter
Sleep 1500ms
