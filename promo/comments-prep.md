# Show HN — comments prep

Top-of-thread comments on a Show HN post tend to fall into a small number of buckets. Drafting your responses before posting means you reply within 5–10 minutes instead of an hour, which dramatically affects how the thread plays out (HN's ranking heavily weights early engagement).

Don't paste these verbatim — read them, then reply in your own words once the actual comment lands.

---

## Bucket 1 — "Why not just use Playwright + lightpanda / chrome-devtools-protocol?"

**Likely framing:** "lightpanda already does headless-Chromium-without-Chromium. What does this add?"

**Honest answer:**

> lightpanda is great and shares the "DOM + V8, no compositor" philosophy. Two things bouncy adds: (1) MCP-native — `bouncy-mcp` ships first-class browse tools so Claude Desktop / Cursor / Claude Code can drive autonomous flows without a wrapper layer. (2) Indexed interactive elements + browser-use-style action vocabulary (click_text, select_option, send_keys, wait_for, chain) tuned for LLM consumption rather than human / Playwright API consumption. If you're driving from code, lightpanda might be the better fit; if you're driving from an LLM, bouncy.

**Don't:** disparage lightpanda. The audience overlap is high and they'll downvote tribal answers.

---

## Bucket 2 — "DOM-only browsers can't render <site they care about>"

**Likely framing:** "I tried something similar with [JSDOM / cheerio / etc.] and it broke on [shadow DOM / web components / canvas-heavy site]."

**Honest answer:**

> Yeah — bouncy targets the ~80% of sites that render correctly enough with a DOM + JS but no compositor. Shadow DOM works (we polyfill the basics in bouncy-js), web components work, canvas / WebGL / pixel-perfect layout do not. The README's "vs Playwright" section is explicit about the boundary. For canvas-heavy sites, Playwright is the right tool; we don't try to fight that.

**Don't:** promise to fix it. The DOM-only constraint is the whole positioning.

---

## Bucket 3 — "Why Rust?"

**Likely framing:** Either genuine ("does it matter?") or hostile ("Rust evangelism strikes again").

**Honest answer:**

> Two specific things: (1) The 40 MB single-binary distribution is hard to do from Node or Python without a packaging dance. (2) V8 isolate management in a multi-tenant scrape backend is much easier when the borrow checker won't let you share a `!Send` isolate across threads. Neither is "Rust is faster"; it's tooling fit.

**Don't:** generic "Rust = safe + fast" responses. They're correct but read as evangelism.

---

## Bucket 4 — "How does this compare to browser-use specifically?"

**Likely framing:** A direct comparison request (you mentioned browser-use, expect the question).

**Honest answer:**

> Same shape — open page, hand the model a structured snapshot, dispatch the next action. Differences: bouncy is one Rust binary (~40 MB) vs Python + Node + Chromium (~300 MB), 30 ms cold start vs ~1.5 s Playwright launch, and ~20 MB resident vs 200+ MB per page. browser-use does things bouncy can't: pixel-perfect layout, screenshots for vision-mode reasoning, canvas / WebGL. If you need those, browser-use; if you don't, bouncy. We adopted browser-use's clickable-element indexing, click_text / select_option / press_key / wait_for / chain primitives in a recent refactor — borrowed because they're correctly shaped for LLM consumption.

**Don't:** claim feature parity. Be specific about what each does better.

---

## Bucket 5 — "Show me benchmark numbers"

**Likely framing:** "30 ms vs 1.5 s on what hardware? What page? Reproducible?"

**Honest answer:**

> Cold-start numbers from my dev box (M3 Pro, 18 GB). Static page (`bouncy fetch https://example.com`): 3-6 ms wall-clock, no V8 boot. JS page (Hacker News, with the inline scripts run): 30-80 ms. Playwright launch numbers from their own docs (~800-1500 ms). The repo has a `bench/webarena/` agent-loop harness — bouncy + Claude over Bedrock or Anthropic-direct — but I haven't run a head-to-head against browser-use yet. That's the next milestone; happy to share when it's reproducible.

**Don't:** claim numbers you haven't reproduced. The HN crowd will check.

---

## Bucket 6 — "Stealth / fingerprinting?"

**Likely framing:** "Does it work against Cloudflare / Akamai / DataDome?"

**Honest answer:**

> Built-in stealth covers `navigator.webdriver`, canvas / audio / WebGPU / battery / WebGL fingerprint randomization per session, no Playwright tells. Holds up against Sannysoft Bot Test and CreepJS at moderate detection settings; **does not** beat Cloudflare Turnstile or DataDome's interactive challenges (those need real layout / canvas / pointer events). Honest about the ceiling.

**Don't:** claim "undetectable" or "bypasses Cloudflare." That's a magnet for hostile traffic and a guaranteed downvote.

---

## Bucket 7 — "Why no screenshots?"

**Likely framing:** "Can't an agent reason much better with screenshots than DOM?"

**Honest answer:**

> Often yes — for visually complex pages, vision is a real win. The tradeoff: screenshots require a real compositor, which means Chromium, which means 200+ MB / 1.5 s / Node-or-Python runtime. Bouncy's bet is that the form-driven 80% (login → fill → submit → read result) doesn't need vision, and people running those flows would rather have 50× lower latency. Mixed-mode (Playwright for visual tasks, bouncy for everything else) is a reasonable architecture too.

**Don't:** dunk on vision-based agents. They're great for what they're great for.

---

## Bucket 8 — Off-topic / shitpost / "this isn't novel"

**Likely framing:** "This already exists, see X" / "Yet another headless browser" / etc.

**Honest answer:**

> Mostly: don't engage. Reply once if there's a substantive point ("this exists in X" → "yes, X is great; the difference is Y"), then disengage. HN's algorithm punishes long defensive threads from the OP.

**Don't:** get drawn into argument trees. One reply, then back out.

---

## Posting hygiene

- **Don't ask for upvotes.** Anywhere. Not in the post, not in DMs, not on Twitter. HN downweights anything that looks brigaded.
- **Reply within 10 minutes** of posting, until the first hour is up. Visible engagement from the author keeps the thread on the front page.
- **Acknowledge mistakes fast.** "You're right, that benchmark is misleading — fixing the README" gets more credibility than defending wrong numbers.
- **Edit the post sparingly.** HN flags substantive edits. Typo fixes are fine; rewriting paragraphs is suspicious.
- **Best time to post:** Tuesday-Thursday, 7-9am Pacific. Avoid Mondays (catch-up volume) and Fridays (low engagement). HN is global but the front-page algorithm peaks during US morning.
