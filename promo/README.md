# Show HN launch kit

Files:

- **`show-hn.md`** — the post itself: title, body, comments-to-expect prep, and submission tactics.
- **`demo.tape`** — a [VHS](https://github.com/charmbracelet/vhs) tape that records a deterministic 30-second demo gif. `vhs demo.tape` produces `assets/demo.gif`.
- **`demo.sh`** — same demo as a plain shell script. Use this if you'd rather record with [asciinema](https://asciinema.org) or capture by hand.
- **`comments-prep.md`** — likely top comments + drafted responses, so you're not improvising under load.

## Recording the demo

Pick **one**:

```bash
# Option A — VHS (recommended; deterministic, repeatable)
brew install vhs   # or: go install github.com/charmbracelet/vhs@latest
vhs promo/demo.tape
# → produces assets/demo.gif

# Option B — asciinema (interactive, slightly less polished)
brew install asciinema agg
asciinema rec demo.cast --command "bash promo/demo.sh"
agg demo.cast assets/demo.gif

# Option C — record by hand
# Run `bash promo/demo.sh` in a clean terminal; record with QuickTime/OBS.
```

Submit the GIF as the post's main image (HN strips `<img>`, but the gif lives in the README and shows up in any preview).

## Posting

1. Land PR #39 on `main` first so `cargo install bouncy-cli` reflects everything you're showing.
2. Tag a release (release-please is wired) so the demo's `cargo install` line points at a clean version, not `main`.
3. Submit at <https://news.ycombinator.com/submit>. Title and URL go to the `bouncy` GitHub repo. Paste the body of `show-hn.md` into the "text" field if you want context inline; otherwise leave blank and the comments will start there.
4. Cross-post in **descending** value order:
   - **Lobste.rs** — Rust audience, expect deeper questions on the V8 integration.
   - **r/rust** — same audience, less critical tone.
   - **r/LocalLLaMA** — angle on "local agent loop without Chromium tax."
   - **MCP server lists** — [punkpeye/awesome-mcp-servers](https://github.com/punkpeye/awesome-mcp-servers) and similar; add via PR.

Cap yourself at 4 places in the first 24h — beyond that the cross-posting feels spammy and HN downweights things that look amplified.
