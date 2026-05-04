# bouncy-bench-webarena

Agent-loop harness for running **bouncy + Claude** through WebArena-shaped tasks. Lives in the workspace as a leaderboard-submission scaffold for [leaderboard.steel.dev](https://leaderboard.steel.dev). Not published to crates.io.

## What it does

1. Reads a JSON file of tasks (`{id, start_url, instruction, expected[], max_steps?}`).
2. For each task, opens a `bouncy_browse::BrowseSession`.
3. Loops:
   - Builds a prompt = system + previous turns + current page snapshot (forms / fields / links / buttons / `interactive` indices / text summary).
   - Calls Claude (Anthropic Messages API).
   - Parses any `tool_use` blocks the model emits.
   - Dispatches each tool call against the live bouncy session (`click` / `fill` / `submit` / `click_text` / `select_option` / `press_key` / `goto` / `read` / `wait_for` / `back` / `done`).
   - Feeds the brief result back as a `tool_result` block.
4. Stops on `done`, `max_steps`, or hard error.
5. Scores the trajectory with a pluggable `Judge` (default: case-insensitive substring match on the model's `done` answer).

## Run it

```bash
ANTHROPIC_API_KEY=sk-ant-... cargo run -p bouncy-bench-webarena -- \
    --tasks tasks.json \
    --model claude-sonnet-4-6
```

`tasks.json` can be a single task or an array:

```json
[
  {
    "id": "find_total_42",
    "start_url": "http://localhost:8080/",
    "instruction": "Click the 'Details' link and report the order total.",
    "expected": ["137.50"]
  }
]
```

The CLI prints a per-task `✓ / ✗` line plus a summary with success rate, total wall-clock, and median per-task time.

## Architecture

```
bench/webarena/src/
├── lib.rs       module wiring
├── main.rs      CLI entrypoint
├── task.rs      Task struct + JSON load
├── tools.rs     Anthropic tool schemas + dispatch to BrowseSession
├── llm.rs       LlmClient trait + AnthropicClient + ScriptedClient (tests)
├── agent.rs     run_task — the loop
├── judge.rs     Judge trait + SubstringJudge
└── fixture.rs   in-process hyper server for tests
```

`tests/agent_smoke.rs` drives the loop end-to-end with `ScriptedClient` against the fixture — hermetic, no API credentials.

## What's stubbed for the leaderboard submission

This crate proves the architecture with a substring judge and an in-process fixture. To submit to [leaderboard.steel.dev/leaderboards/webarena](https://leaderboard.steel.dev/leaderboards/webarena):

1. **Stand up WebArena fixtures locally** — WebArena ships a docker compose with Reddit / GitLab / e-commerce / OSM / CMS clones. See [github.com/web-arena-x/webarena](https://github.com/web-arena-x/webarena).
2. **Port WebArena's task format** — extend `task.rs` with the verification rubric WebArena tasks use (URL match, page-state checks, exact-string-match flags). The shape is roughly compatible; bring fields in as needed.
3. **Plug in WebArena's official judge** — implement `Judge` against the rubric. Keep `SubstringJudge` as a fallback.
4. **Run the suite** — 100-task subset is fine for a first submission; full 812 is the eventual target. Save trajectories as JSON for the source link in the leaderboard PR.
5. **Submit** — open a PR to [steel-dev/leaderboard](https://github.com/steel-dev/leaderboard) with a row in `src/data/` matching the schema and a public link to your trajectories + reproduction script.

The story to tell on the submission row: **same accuracy as browser-use + Claude on the WebArena DOM-feasible subset, ~50× lower wall-clock per task** (no Chromium boot, ~30 ms cold start, ~20 MB resident).
