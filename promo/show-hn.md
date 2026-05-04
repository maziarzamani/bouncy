# Show HN: bouncy

## Title

```
Show HN: Bouncy – tiny Rust headless browser, MCP-native, no Chromium
```

(67 chars — under HN's 80-char limit. Avoids exclamation marks and emoji per HN guidelines.)

## URL

```
https://github.com/maziarzamani/bouncy
```

## Text body (paste into the "text" field)

```
Bouncy started as a web scraper and grew into a tiny full-on browser. One ~40 MB
Rust binary, no Node, no Chrome, no Python. It does three things:

  - Scrape — `bouncy fetch <url>` returns HTML / visible text / links / CSS-
    selector matches. Static pages: 3-6 ms cold. JS pages: 30-80 ms (lazy V8 —
    boots only when the page actually needs it).

  - Browse — `bouncy browse <url>` opens a stateful session that holds V8 +
    cookies + DOM across click / fill / submit / goto / read steps. Scriptable
    as a one-liner --do chain or interactive as a REPL.

  - Drive autonomously — `bouncy-mcp` exposes the same browse primitives as
    MCP tools. Claude Desktop / Claude Code / Cursor can open a page, find a
    form, fill it, submit, read the result without any glue code.

Same shape as browser-use, without the Chromium dependency. ~30 ms cold start
vs Playwright's ~1.5 s. ~20 MB resident vs 200+ MB.

Ergonomics adopted from browser-use after a recent refactor: indexed
interactive elements (every form field / link / button gets a stable integer
the model addresses by `index` instead of guessing CSS selectors), click_text
(find by visible text), select_option, send_keys, wait_for, history,
sensitive-data masking on fill, and a `chain` primitive that batches N
actions into one round trip.

Honest about what it doesn't do: no real layout, no paint, no canvas / WebGL,
no screenshots. Pure DOM + V8. Right tool when the page renders correctly
*enough* without a compositor — most form-driven flows, login walls,
table extraction, multi-step navigation. Wrong tool when the model needs to
*see* the page (use Playwright + browser-use for that).

Install:

  cargo install bouncy-cli bouncy-mcp

Or grab a binary tarball from the releases page (Linux x86_64, macOS Apple
Silicon, Windows x86_64).

Repo: https://github.com/maziarzamani/bouncy
```

## Why this framing works

- **Lead with the artifact.** "Bouncy started as a scraper and grew into a
  tiny full-on browser" — concrete, no buzzwords, sets context in one
  sentence.
- **Three modes, named.** Scrape / browse / drive. The reader can see
  whether their use case fits without reading 500 words.
- **Numbers up front.** 30 ms vs 1.5 s, 20 MB vs 200 MB. HN responds to
  benchmarks more than to feature lists.
- **Pre-empt the "it can't do screenshots" comment.** Acknowledging the
  tradeoff in the post body kills the top critical comment before it
  posts.
- **Don't oversell.** No "the future of agents," no "10× engineer hack,"
  no emoji. HN deflates anything that smells like marketing.

## Don't include in the post

- Roadmap items.
- Anything about leaderboards (we don't have a row yet).
- Comparisons to anything you haven't actually benched against.
- An "ask" of any kind (job, funding, contributors). HN posts that ask
  for things land worse than ones that just show.

## What goes in the assets

- **Logo** — already at `assets/logo-light.png` / `assets/logo-dark.png`.
  README renders fine for HN's preview.
- **Demo gif** — `assets/demo.gif`, produced by `vhs promo/demo.tape`. ~30
  seconds, shows scrape + browse + chain.
- **TUI demo** — `assets/tui-demo.gif` already exists, secondary asset.
