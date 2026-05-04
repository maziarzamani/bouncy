#!/usr/bin/env bash
# bouncy — Show HN demo, runnable directly.
#
# Use cases:
#   - Sanity check before recording: `bash promo/demo.sh`
#   - Record with asciinema: `asciinema rec demo.cast -c "bash promo/demo.sh"`
#   - Manual screen capture: just run it in a clean terminal.
#
# Each step prints a comment line, sleeps briefly, then runs the command.
# Mirrors the structure of `promo/demo.tape` so the recording matches the
# tape's framing.

set -euo pipefail

# Colors — keep these gentle for a demo (no red bold etc.)
DIM='\033[2m'
RESET='\033[0m'
BLUE='\033[34m'

note() {
    printf "${DIM}# %s${RESET}\n" "$1"
}

cmd() {
    printf "${BLUE}\$${RESET} %s\n" "$1"
    sleep 0.3
    eval "$1"
    printf "\n"
}

clear

note "bouncy — tiny Rust headless browser. no Chromium."
sleep 1
echo

# 1. Scrape
# Use `--dump text` (works on every version) so an old `bouncy` in PATH
# doesn't error. If you've upgraded (`cargo install --path crates/bouncy-cli
# --force`), you can swap in `--select h1 --dump text` for a single-line
# h1 extraction.
note "scrape — static page, no V8 boot"
cmd 'time bouncy fetch https://example.com --dump text'
sleep 1

# 2. Browse — click by visible text. (example.com's link text changes
# every few years — currently "Learn more" as of mid-2026.)
note "browse — stateful session, click by visible text"
cmd 'bouncy browse https://example.com --do "click_text Learn more"'
sleep 1

# 3. Indexed interactive elements
note "every interactive element gets a stable index — addressable from"
note "the LLM without guessing selectors. snapshot.interactive[N]:"
cmd "bouncy browse https://example.com --do snapshot --json | jq '.interactive'"
sleep 1

# 4. MCP — describe (we can't easily demo the LLM in a terminal)
note "bouncy-mcp serves the same primitives as MCP tools."
note "Claude Desktop / Code / Cursor drive the loop end-to-end:"
note "  bouncy_browse_open  → snapshot + session_id"
note "  bouncy_browse_chain → batch click/fill/submit/done in one round trip"
sleep 2

# 5. Outro
note "40 MB binary. 30 ms cold start."
note "github.com/maziarzamani/bouncy"
sleep 1
