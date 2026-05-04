# bouncy — Show HN demo tape.
#
# A VHS tape: https://github.com/charmbracelet/vhs
# Run `vhs demo.tape` from the repo root. Produces `assets/demo.gif`.
#
# Goal: a 30-second loop demonstrating the three modes — scrape, browse,
# drive. Speed is the headline, so we lean on `time` and explicit timings.
#
# Pre-conditions:
#   - cargo install bouncy-cli bouncy-mcp  (or: cargo build --release in repo)
#   - example.com reachable
#   - VHS installed (brew install vhs)

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

# ─── 2. Scrape: fetch + extract in one shot (5s) ───────────────────
Type "# scrape — static page, no V8 boot"
Enter
Sleep 500ms
Type "time bouncy fetch https://example.com --select h1 --dump text"
Enter
Sleep 3s

# ─── 3. Browse: stateful session with click_text (8s) ──────────────
Type "# browse — stateful session, click by visible text"
Enter
Sleep 500ms
Type 'bouncy browse https://example.com --do "click_text More information"'
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
