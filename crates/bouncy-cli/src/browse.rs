//! `bouncy browse` — drive a stateful browse session from the CLI.
//!
//! Two modes share the same parsed-`Operation` core:
//!
//!   - **Scripted chain** — `--do "fill #u maziar" --do "submit form"`
//!     runs each step in order, prints the result of each (or just the
//!     final snapshot when `--json` is set), then exits.
//!   - **REPL** — no `--do`. Reads commands one per line from stdin
//!     and runs them against the held-open session. `exit` / EOF quits.
//!     Designed to be portable: pure stdin/stdout, no `rustyline`
//!     readline dep, so it stays usable from non-tty contexts (CI,
//!     `bouncy browse <url> < script.txt`).
//!
//! Command grammar is deliberately tiny so it's predictable from a
//! shell script:
//!
//!     click <selector|@INDEX>
//!     fill  <selector|@INDEX> <value...>     (value is rest-of-line)
//!     submit <selector|@INDEX>
//!     goto  <url>
//!     read  <selector|@INDEX> [text|html|attr:NAME]
//!     eval  <expr...>
//!     snapshot
//!     click_text <text...>
//!     select <selector|@INDEX> <value...>
//!     key <selector|@INDEX> <key>            (Enter / Tab / Escape / single chars)
//!     wait <ms>
//!     wait_for <selector>
//!     wait_for_text <text...>
//!     back
//!     forward
//!     help
//!     exit / quit                            (REPL only)
//!
//! `@INDEX` references the integer `index` field of an entry in the
//! current snapshot's `interactive` list (a feature adopted from
//! browser-use to skip selector construction).

use std::io::{BufRead, IsTerminal, Write};

use bouncy_browse::{BrowseOpts, BrowseSession, PageSnapshot, ReadMode, Target};

pub struct Args {
    pub url: String,
    pub do_steps: Vec<String>,
    pub json: bool,
    pub user_agent: Option<String>,
    pub stealth: bool,
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    let opts = BrowseOpts {
        user_agent: args.user_agent,
        stealth: args.stealth,
        ..BrowseOpts::default()
    };
    let (session, initial) = BrowseSession::open(&args.url, opts).await?;

    if args.do_steps.is_empty() {
        run_repl(session, initial, args.json).await
    } else {
        run_chain(session, initial, &args.do_steps, args.json).await
    }
}

/// Scripted-chain mode: each `--do "…"` is parsed and executed in
/// order. With `--json`, only the final snapshot is emitted (one
/// JSON document on stdout). Without, each step prints a one-line
/// summary so the user can follow along.
async fn run_chain(
    session: BrowseSession,
    initial: PageSnapshot,
    steps: &[String],
    json: bool,
) -> anyhow::Result<()> {
    if !json {
        println!("opened {} — {}", initial.url, initial.title);
    }
    let mut last_snapshot = initial;
    for raw in steps {
        let op = Operation::parse(raw)?;
        if !json {
            println!("> {raw}");
        }
        match op.execute(&session).await? {
            StepOutput::Snapshot(s) => {
                if !json {
                    println!("  ↳ {}", summarize(&s));
                }
                last_snapshot = s;
            }
            StepOutput::Reads(matches) => {
                if json {
                    // Read steps in JSON mode print their matches as
                    // their own JSON document on a separate line.
                    let v = serde_json::to_string(&matches)?;
                    println!("{v}");
                } else {
                    for line in &matches {
                        println!("  ↳ {line}");
                    }
                }
            }
            StepOutput::EvalResult(result, snap) => {
                if json {
                    let v = serde_json::json!({ "eval": result });
                    println!("{}", serde_json::to_string(&v)?);
                } else {
                    println!("  ↳ {result}");
                }
                last_snapshot = snap;
            }
        }
    }
    if json {
        // Emit the final snapshot last (after any per-step JSON lines)
        // so callers can pick the last line as the canonical result.
        println!("{}", serde_json::to_string_pretty(&last_snapshot)?);
    } else {
        println!("done.");
    }
    Ok(())
}

/// REPL mode: read commands from stdin one per line, run each, print
/// result. Exits on `exit` / `quit` / EOF / blank-line-on-EOF.
async fn run_repl(session: BrowseSession, initial: PageSnapshot, json: bool) -> anyhow::Result<()> {
    let interactive = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    if interactive {
        eprintln!("bouncy browse — opened {}", initial.url);
        eprintln!("  title: {}", initial.title);
        eprintln!("  {}", summarize(&initial));
        eprintln!("  type `help` for commands, `exit` to quit.");
    }
    print_prompt(interactive);

    let stdin = std::io::stdin();
    let lines = stdin.lock().lines();
    for line in lines {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            print_prompt(interactive);
            continue;
        }
        if matches!(trimmed, "exit" | "quit") {
            break;
        }
        if matches!(trimmed, "help" | "?") {
            print_help();
            print_prompt(interactive);
            continue;
        }
        match Operation::parse(trimmed) {
            Ok(op) => match op.execute(&session).await {
                Ok(StepOutput::Snapshot(s)) => {
                    if json {
                        println!("{}", serde_json::to_string(&s)?);
                    } else {
                        println!("  ↳ {}", summarize(&s));
                    }
                }
                Ok(StepOutput::Reads(matches)) => {
                    if json {
                        println!("{}", serde_json::to_string(&matches)?);
                    } else {
                        for m in matches {
                            println!("  ↳ {m}");
                        }
                    }
                }
                Ok(StepOutput::EvalResult(result, _)) => {
                    if json {
                        let v = serde_json::json!({ "eval": result });
                        println!("{}", serde_json::to_string(&v)?);
                    } else {
                        println!("  ↳ {result}");
                    }
                }
                Err(e) => {
                    eprintln!("  ✗ {e}");
                }
            },
            Err(e) => eprintln!("  ✗ {e}"),
        }
        print_prompt(interactive);
    }
    Ok(())
}

fn print_prompt(interactive: bool) {
    if interactive {
        eprint!("> ");
        let _ = std::io::stderr().flush();
    }
}

fn print_help() {
    eprintln!(
        "commands:
  click <selector|@N>             fire synthetic click on matched element
  fill  <selector|@N> <value>     set input value (fires input + change events)
  submit <selector|@N>            submit form (or form containing the matched button)
  goto  <url>                     navigate this session to a new URL
  read  <selector|@N> [mode]      mode: text (default) | html | attr:NAME
  eval  <js>                      evaluate JS in the page's V8 context
  click_text <text>               click first link/button whose text matches
  select <selector|@N> <value>    pick a <select>'s option (by value or text)
  key <selector|@N> <key>         dispatch keydown/keyup (Enter / Tab / etc.)
  wait <ms>                       sleep <ms> milliseconds
  wait_for <selector>             block until selector matches (5s timeout)
  wait_for_text <text>            block until visible text appears
  back                            re-navigate to the previous URL
  forward                         re-navigate forward
  snapshot                        re-print the current page snapshot
  help                            this message
  exit                            quit

@N references the `index` of an element from the latest snapshot's `interactive` list.
"
    );
}

/// One-line human summary of a snapshot for the chain / REPL output.
fn summarize(s: &PageSnapshot) -> String {
    format!(
        "snapshot @ {} — title={:?}, {} forms, {} links, {} buttons, {} inputs, {} headings",
        s.url,
        s.title,
        s.forms.len(),
        s.links.len(),
        s.buttons.len(),
        s.inputs.len(),
        s.headings.len(),
    )
}

// --- Parsed operation + execution -------------------------------------------

#[derive(Debug)]
enum Operation {
    Click(Target),
    Fill { target: Target, value: String },
    Submit(Target),
    Goto(String),
    Read { target: Target, mode: ReadMode },
    Eval(String),
    Snapshot,
    ClickText(String),
    Select { target: Target, value: String },
    Key { target: Target, key: String },
    Wait { ms: u64 },
    WaitFor(String),
    WaitForText(String),
    Back,
    Forward,
}

enum StepOutput {
    Snapshot(PageSnapshot),
    Reads(Vec<String>),
    EvalResult(String, PageSnapshot),
}

impl Operation {
    fn parse(line: &str) -> anyhow::Result<Self> {
        let line = line.trim();
        let (verb, rest) = split_first_word(line);
        let rest = rest.trim();
        match verb {
            "click" => {
                anyhow::ensure!(!rest.is_empty(), "click requires a selector or @INDEX");
                Ok(Operation::Click(parse_target(rest)?))
            }
            "fill" => {
                let (target_s, value) = split_first_word(rest);
                anyhow::ensure!(!target_s.is_empty(), "fill requires a selector or @INDEX");
                Ok(Operation::Fill {
                    target: parse_target(target_s)?,
                    // Value is rest-of-line, trimmed once but otherwise
                    // verbatim so spaces inside the value survive.
                    value: value.trim_start().to_string(),
                })
            }
            "submit" => {
                anyhow::ensure!(!rest.is_empty(), "submit requires a selector or @INDEX");
                Ok(Operation::Submit(parse_target(rest)?))
            }
            "goto" => {
                anyhow::ensure!(!rest.is_empty(), "goto requires a URL");
                Ok(Operation::Goto(rest.to_string()))
            }
            "read" => {
                let (target_s, mode_str) = split_first_word(rest);
                anyhow::ensure!(!target_s.is_empty(), "read requires a selector or @INDEX");
                let mode = parse_read_mode(mode_str.trim())?;
                Ok(Operation::Read {
                    target: parse_target(target_s)?,
                    mode,
                })
            }
            "eval" => {
                anyhow::ensure!(!rest.is_empty(), "eval requires an expression");
                Ok(Operation::Eval(rest.to_string()))
            }
            "snapshot" => Ok(Operation::Snapshot),
            "click_text" => {
                anyhow::ensure!(!rest.is_empty(), "click_text requires a text argument");
                Ok(Operation::ClickText(rest.to_string()))
            }
            "select" => {
                let (target_s, value) = split_first_word(rest);
                anyhow::ensure!(!target_s.is_empty(), "select requires a selector or @INDEX");
                let value = value.trim_start();
                anyhow::ensure!(!value.is_empty(), "select requires a value");
                Ok(Operation::Select {
                    target: parse_target(target_s)?,
                    value: value.to_string(),
                })
            }
            "key" => {
                let (target_s, key) = split_first_word(rest);
                anyhow::ensure!(!target_s.is_empty(), "key requires a selector or @INDEX");
                let key = key.trim();
                anyhow::ensure!(!key.is_empty(), "key requires a key name");
                Ok(Operation::Key {
                    target: parse_target(target_s)?,
                    key: key.to_string(),
                })
            }
            "wait" => {
                anyhow::ensure!(!rest.is_empty(), "wait requires a duration in ms");
                let ms: u64 = rest
                    .parse()
                    .map_err(|_| anyhow::anyhow!("wait expects an integer ms, got {rest:?}"))?;
                Ok(Operation::Wait { ms })
            }
            "wait_for" => {
                anyhow::ensure!(!rest.is_empty(), "wait_for requires a selector");
                Ok(Operation::WaitFor(rest.to_string()))
            }
            "wait_for_text" => {
                anyhow::ensure!(!rest.is_empty(), "wait_for_text requires a substring");
                Ok(Operation::WaitForText(rest.to_string()))
            }
            "back" => Ok(Operation::Back),
            "forward" => Ok(Operation::Forward),
            "" => anyhow::bail!("empty command"),
            other => anyhow::bail!(
                "unknown command {other:?} — try: click | fill | submit | goto | read | eval | snapshot | click_text | select | key | wait | wait_for | wait_for_text | back | forward | help"
            ),
        }
    }

    async fn execute(&self, session: &BrowseSession) -> anyhow::Result<StepOutput> {
        const CLI_WAIT_TIMEOUT_MS: u64 = 5_000;
        Ok(match self {
            Operation::Click(t) => StepOutput::Snapshot(session.click_target(t.clone()).await?),
            Operation::Fill { target, value } => {
                StepOutput::Snapshot(session.fill_target(target.clone(), value).await?)
            }
            Operation::Submit(t) => StepOutput::Snapshot(session.submit_target(t.clone()).await?),
            Operation::Goto(url) => StepOutput::Snapshot(session.goto(url).await?),
            Operation::Read { target, mode } => {
                StepOutput::Reads(session.read_target(target.clone(), mode.clone()).await?)
            }
            Operation::Eval(expr) => {
                let r = session.eval(expr).await?;
                StepOutput::EvalResult(r.result, r.snapshot)
            }
            Operation::Snapshot => StepOutput::Snapshot(session.snapshot().await?),
            Operation::ClickText(text) => StepOutput::Snapshot(session.click_text(text).await?),
            Operation::Select { target, value } => {
                StepOutput::Snapshot(session.select_option(target.clone(), value).await?)
            }
            Operation::Key { target, key } => {
                StepOutput::Snapshot(session.press_key(target.clone(), key).await?)
            }
            Operation::Wait { ms } => StepOutput::Snapshot(session.wait_ms(*ms).await?),
            Operation::WaitFor(selector) => {
                StepOutput::Snapshot(session.wait_for(selector, CLI_WAIT_TIMEOUT_MS).await?)
            }
            Operation::WaitForText(needle) => {
                StepOutput::Snapshot(session.wait_for_text(needle, CLI_WAIT_TIMEOUT_MS).await?)
            }
            Operation::Back => StepOutput::Snapshot(session.back().await?),
            Operation::Forward => StepOutput::Snapshot(session.forward().await?),
        })
    }
}

/// Parse a selector-or-index token. `@N` (or just a bare digit string
/// prefixed by `@`) becomes [`Target::Index`]; anything else becomes
/// [`Target::Selector`].
fn parse_target(s: &str) -> anyhow::Result<Target> {
    if let Some(rest) = s.strip_prefix('@') {
        let n: u32 = rest
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid index {s:?} — expected @<number>"))?;
        Ok(Target::Index(n))
    } else {
        Ok(Target::selector(s))
    }
}

fn split_first_word(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    }
}

fn parse_read_mode(s: &str) -> anyhow::Result<ReadMode> {
    match s {
        "" | "text" => Ok(ReadMode::Text),
        "html" => Ok(ReadMode::Html),
        other if other.starts_with("attr:") => Ok(ReadMode::Attr(other[5..].to_string())),
        other => anyhow::bail!("unknown read mode {other:?} — expected: text | html | attr:NAME"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_selector(t: &Target, expected: &str) {
        match t {
            Target::Selector(s) => assert_eq!(s, expected),
            Target::Index(_) => panic!("expected Selector, got Index"),
        }
    }

    fn assert_index(t: &Target, expected: u32) {
        match t {
            Target::Index(n) => assert_eq!(*n, expected),
            Target::Selector(_) => panic!("expected Index, got Selector"),
        }
    }

    #[test]
    fn parse_click_takes_selector() {
        match Operation::parse("click h1.title").unwrap() {
            Operation::Click(t) => assert_selector(&t, "h1.title"),
            _ => panic!("expected Click"),
        }
    }

    #[test]
    fn parse_click_accepts_at_index() {
        match Operation::parse("click @5").unwrap() {
            Operation::Click(t) => assert_index(&t, 5),
            _ => panic!("expected Click"),
        }
    }

    #[test]
    fn parse_fill_keeps_value_with_spaces_intact() {
        match Operation::parse("fill input[name=msg] hello world").unwrap() {
            Operation::Fill { target, value } => {
                assert_selector(&target, "input[name=msg]");
                assert_eq!(value, "hello world");
            }
            _ => panic!("expected Fill"),
        }
    }

    #[test]
    fn parse_fill_accepts_at_index() {
        match Operation::parse("fill @2 alice").unwrap() {
            Operation::Fill { target, value } => {
                assert_index(&target, 2);
                assert_eq!(value, "alice");
            }
            _ => panic!("expected Fill"),
        }
    }

    #[test]
    fn parse_read_defaults_to_text_mode() {
        match Operation::parse("read h1").unwrap() {
            Operation::Read { target, mode } => {
                assert_selector(&target, "h1");
                assert!(matches!(mode, ReadMode::Text));
            }
            _ => panic!("expected Read"),
        }
    }

    #[test]
    fn parse_read_attr_mode() {
        match Operation::parse("read a attr:href").unwrap() {
            Operation::Read { target, mode } => {
                assert_selector(&target, "a");
                match mode {
                    ReadMode::Attr(n) => assert_eq!(n, "href"),
                    _ => panic!("expected Attr"),
                }
            }
            _ => panic!("expected Read"),
        }
    }

    #[test]
    fn parse_click_text_takes_rest_of_line() {
        match Operation::parse("click_text Sign in").unwrap() {
            Operation::ClickText(s) => assert_eq!(s, "Sign in"),
            _ => panic!("expected ClickText"),
        }
    }

    #[test]
    fn parse_select_splits_target_and_value() {
        match Operation::parse("select #topic Bananas").unwrap() {
            Operation::Select { target, value } => {
                assert_selector(&target, "#topic");
                assert_eq!(value, "Bananas");
            }
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn parse_key_with_named_key() {
        match Operation::parse("key #search Enter").unwrap() {
            Operation::Key { target, key } => {
                assert_selector(&target, "#search");
                assert_eq!(key, "Enter");
            }
            _ => panic!("expected Key"),
        }
    }

    #[test]
    fn parse_wait_requires_integer() {
        match Operation::parse("wait 250").unwrap() {
            Operation::Wait { ms } => assert_eq!(ms, 250),
            _ => panic!("expected Wait"),
        }
        assert!(Operation::parse("wait abc").is_err());
    }

    #[test]
    fn parse_wait_for_takes_selector() {
        match Operation::parse("wait_for #ready").unwrap() {
            Operation::WaitFor(s) => assert_eq!(s, "#ready"),
            _ => panic!("expected WaitFor"),
        }
    }

    #[test]
    fn parse_back_and_forward_take_no_args() {
        assert!(matches!(Operation::parse("back").unwrap(), Operation::Back));
        assert!(matches!(
            Operation::parse("forward").unwrap(),
            Operation::Forward
        ));
    }

    #[test]
    fn parse_target_accepts_selector_and_index() {
        assert!(matches!(parse_target("h1").unwrap(), Target::Selector(_)));
        assert!(matches!(parse_target("@7").unwrap(), Target::Index(7)));
        assert!(parse_target("@abc").is_err());
    }

    #[test]
    fn parse_eval_keeps_expr_with_dots_and_parens() {
        match Operation::parse("eval document.querySelector('h1').textContent").unwrap() {
            Operation::Eval(e) => assert_eq!(e, "document.querySelector('h1').textContent"),
            _ => panic!("expected Eval"),
        }
    }

    #[test]
    fn parse_snapshot() {
        assert!(matches!(
            Operation::parse("snapshot").unwrap(),
            Operation::Snapshot
        ));
    }

    #[test]
    fn parse_unknown_command_errors_with_helpful_message() {
        let err = Operation::parse("scroll down").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown command"), "got: {msg}");
        assert!(msg.contains("scroll"), "got: {msg}");
        // The new commands should appear in the suggestion list.
        assert!(msg.contains("click_text"), "got: {msg}");
    }

    #[test]
    fn parse_missing_args_errors() {
        assert!(Operation::parse("click").is_err());
        assert!(Operation::parse("fill").is_err());
        assert!(Operation::parse("submit").is_err());
        assert!(Operation::parse("goto").is_err());
        assert!(Operation::parse("read").is_err());
        assert!(Operation::parse("eval").is_err());
    }

    #[test]
    fn parse_read_mode_unknown_errors() {
        assert!(parse_read_mode("foo").is_err());
    }

    #[test]
    fn parse_read_mode_html_and_text() {
        assert!(matches!(parse_read_mode("text").unwrap(), ReadMode::Text));
        assert!(matches!(parse_read_mode("html").unwrap(), ReadMode::Html));
        assert!(matches!(parse_read_mode("").unwrap(), ReadMode::Text));
    }

    #[test]
    fn split_first_word_basic() {
        assert_eq!(split_first_word("hello world"), ("hello", " world"));
        assert_eq!(split_first_word("alone"), ("alone", ""));
        assert_eq!(split_first_word(""), ("", ""));
    }
}
