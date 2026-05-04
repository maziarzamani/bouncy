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

### Direct Anthropic API (default)

```bash
ANTHROPIC_API_KEY=sk-ant-... cargo run -p bouncy-bench-webarena -- \
    --tasks tasks.json \
    --model claude-sonnet-4-6
```

### AWS Bedrock

Build with the `bedrock` feature, then point `--provider bedrock` at a
Bedrock model id (or inference profile ARN). Auth is the standard AWS
credential chain — env vars (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
`AWS_SESSION_TOKEN`), `~/.aws/credentials`, or an attached IAM role.

```bash
# Pre-flight: confirm the Anthropic Claude models are enabled in your
# Bedrock account for the region you're targeting (Console → Bedrock →
# Model access). Bedrock model ids look like
#   anthropic.claude-sonnet-4-5-20250929-v1:0
# Inference profile ARNs (us.anthropic.claude-sonnet-...) work too.

cargo run -p bouncy-bench-webarena --features bedrock -- \
    --tasks tasks.json \
    --provider bedrock \
    --model anthropic.claude-sonnet-4-5-20250929-v1:0 \
    --region us-east-1
```

The `bedrock` feature is opt-in because it pulls in the AWS SDK
(noticeable compile-time cost). Without it, the binary still
builds — `--provider bedrock` then errors with a clear "rebuild with
--features bedrock" message.

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

## Running real WebArena tasks

WebArena ships its task suite as JSON files under [`config_files/`](https://github.com/web-arena-x/webarena/tree/main/config_files) — one document per task with a starting URL templated against placeholder hosts (`__SHOPPING__`, `__REDDIT__`, etc.) plus an eval rubric. The harness loads that format directly via `--webarena-tasks` and scores trajectories with `WebArenaJudge` instead of the substring judge.

### Setup

```bash
# 1. Stand up WebArena's docker compose locally — Reddit / GitLab /
#    e-commerce / OSM / CMS clones. Follow the upstream repo:
#    https://github.com/web-arena-x/webarena
docker compose up -d   # in the webarena/ checkout

# 2. Export the same env vars WebArena's own scripts use, pointing
#    at the bound localhost ports. Skip ones you don't need —
#    tasks that use missing placeholders error at load time.
export SHOPPING="http://localhost:7770"
export SHOPPING_ADMIN="http://localhost:7780/admin"
export REDDIT="http://localhost:9999"
export GITLAB="http://localhost:8023"
export MAP="http://localhost:3000"
export WIKIPEDIA="http://localhost:8888"
export HOMEPAGE="http://localhost:4399"

# 3. Point the harness at WebArena's task config(s). Path can be a
#    single .json file or a directory of them.
ANTHROPIC_API_KEY=sk-ant-… cargo run -p bouncy-bench-webarena -- \
    --webarena-tasks /path/to/webarena/config_files \
    --model claude-sonnet-4-6
```

The `--webarena-tasks` and `--tasks` flags are mutually exclusive — pick the format you're feeding in. Run output looks the same as the simple format; the judge per-task is whichever the task's source format implies.

### Eval coverage

WebArena's verifier supports three eval types. The `WebArenaJudge` covers them as follows:

| eval type      | coverage                                                                 |
|----------------|--------------------------------------------------------------------------|
| `string_match` | `exact_match` (case-insensitive trim), `must_include`, `fuzzy_match` (substring approximation; upstream uses an LLM judge — gap documented) |
| `url_match`    | not yet — needs `Trajectory.final_url` plumbed through. Errors loudly with a typed message rather than silently passing. |
| `program_html` | not yet — needs a post-task DOM fetch + lxml-style locator port. Same loud-error treatment. |

Tasks whose eval type isn't covered fail with a clear "not yet supported" message, so you'll see exactly which tasks would need each gap closed.

## To get a row on leaderboard.steel.dev

1. **Fixture parity** — your WebArena docker stack needs the same fixture versions WebArena's leaderboard pins. See upstream's `Dockerfile` tags.
2. **Eval gap closure** — `url_match` and `program_html` cover a real share of the 812-task suite. If your task selection avoids them you can submit a (clearly labelled) DOM-only subset; otherwise close the gaps first.
3. **Run the suite** — full 812 tasks is the eventual target; a 100-task labelled subset is acceptable for a first row.
4. **Submit** — open a PR to [steel-dev/leaderboard](https://github.com/steel-dev/leaderboard) with a row in `src/data/` and a public link to your trajectories + reproduction script.

The story to tell on the submission row: **same accuracy as browser-use + Claude on the DOM-feasible subset, ~50× lower wall-clock per task** (no Chromium boot, ~30 ms cold start, ~20 MB resident).
