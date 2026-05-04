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
//!     click <selector>
//!     fill  <selector> <value...>      (value is rest-of-line)
//!     submit <selector>
//!     goto  <url>
//!     read  <selector> [text|html|attr:NAME]   (default: text)
//!     eval  <expr...>                  (rest-of-line)
//!     snapshot                         (re-print current snapshot)
//!     help
//!     exit / quit                       (REPL only)

use std::io::{BufRead, IsTerminal, Write};

use bouncy_browse::{BrowseOpts, BrowseSession, PageSnapshot, ReadMode};

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
  click <selector>                fire synthetic click on matched element
  fill  <selector> <value>        set input value (fires input + change events)
  submit <selector>               submit form (or form containing the matched button)
  goto  <url>                     navigate this session to a new URL
  read  <selector> [mode]         mode: text (default) | html | attr:NAME
  eval  <js>                      evaluate JS in the page's V8 context
  snapshot                        re-print the current page snapshot
  help                            this message
  exit                            quit
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
    Click(String),
    Fill { selector: String, value: String },
    Submit(String),
    Goto(String),
    Read { selector: String, mode: ReadMode },
    Eval(String),
    Snapshot,
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
                anyhow::ensure!(!rest.is_empty(), "click requires a selector");
                Ok(Operation::Click(rest.to_string()))
            }
            "fill" => {
                let (selector, value) = split_first_word(rest);
                anyhow::ensure!(!selector.is_empty(), "fill requires a selector");
                Ok(Operation::Fill {
                    selector: selector.to_string(),
                    // Value is rest-of-line, trimmed once but otherwise
                    // verbatim so spaces inside the value survive.
                    value: value.trim_start().to_string(),
                })
            }
            "submit" => {
                anyhow::ensure!(!rest.is_empty(), "submit requires a selector");
                Ok(Operation::Submit(rest.to_string()))
            }
            "goto" => {
                anyhow::ensure!(!rest.is_empty(), "goto requires a URL");
                Ok(Operation::Goto(rest.to_string()))
            }
            "read" => {
                let (selector, mode_str) = split_first_word(rest);
                anyhow::ensure!(!selector.is_empty(), "read requires a selector");
                let mode = parse_read_mode(mode_str.trim())?;
                Ok(Operation::Read {
                    selector: selector.to_string(),
                    mode,
                })
            }
            "eval" => {
                anyhow::ensure!(!rest.is_empty(), "eval requires an expression");
                Ok(Operation::Eval(rest.to_string()))
            }
            "snapshot" => Ok(Operation::Snapshot),
            "" => anyhow::bail!("empty command"),
            other => anyhow::bail!(
                "unknown command {other:?} — try: click | fill | submit | goto | read | eval | snapshot | help"
            ),
        }
    }

    async fn execute(&self, session: &BrowseSession) -> anyhow::Result<StepOutput> {
        Ok(match self {
            Operation::Click(s) => StepOutput::Snapshot(session.click(s).await?),
            Operation::Fill { selector, value } => {
                StepOutput::Snapshot(session.fill(selector, value).await?)
            }
            Operation::Submit(s) => StepOutput::Snapshot(session.submit(s).await?),
            Operation::Goto(url) => StepOutput::Snapshot(session.goto(url).await?),
            Operation::Read { selector, mode } => {
                StepOutput::Reads(session.read(selector, mode.clone()).await?)
            }
            Operation::Eval(expr) => {
                let r = session.eval(expr).await?;
                StepOutput::EvalResult(r.result, r.snapshot)
            }
            Operation::Snapshot => StepOutput::Snapshot(session.snapshot().await?),
        })
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

    #[test]
    fn parse_click_takes_selector() {
        match Operation::parse("click h1.title").unwrap() {
            Operation::Click(s) => assert_eq!(s, "h1.title"),
            _ => panic!("expected Click"),
        }
    }

    #[test]
    fn parse_fill_keeps_value_with_spaces_intact() {
        match Operation::parse("fill input[name=msg] hello world").unwrap() {
            Operation::Fill { selector, value } => {
                assert_eq!(selector, "input[name=msg]");
                assert_eq!(value, "hello world");
            }
            _ => panic!("expected Fill"),
        }
    }

    #[test]
    fn parse_read_defaults_to_text_mode() {
        match Operation::parse("read h1").unwrap() {
            Operation::Read { selector, mode } => {
                assert_eq!(selector, "h1");
                assert!(matches!(mode, ReadMode::Text));
            }
            _ => panic!("expected Read"),
        }
    }

    #[test]
    fn parse_read_attr_mode() {
        match Operation::parse("read a attr:href").unwrap() {
            Operation::Read { selector, mode } => {
                assert_eq!(selector, "a");
                match mode {
                    ReadMode::Attr(n) => assert_eq!(n, "href"),
                    _ => panic!("expected Attr"),
                }
            }
            _ => panic!("expected Read"),
        }
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
