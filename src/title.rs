//! A Claude session's title: what it's working on, in a few words. Claude
//! Code shows a session's title as its terminal's (Konsole's tab and window,
//! Windows Terminal's tab, a console window's title bar), so with one, a
//! glance says which window is which.
//!
//! Given through Claude's hooks, which every PC's settings already run
//! (`claude-accounts hook prompt` and `hook session-start`), by their
//! `sessionTitle` answer:
//! - the first prompt names the session (or, for one started before this,
//!   what its transcript says it's about: Claude's own title for it, the
//!   /goal it was given, its first prompt);
//! - a /goal names it again, from what the goal is;
//! - a session resumed (`claude --resume`) gets its title back at once.
//!
//! A name you give (`/rename`, `--name`) stands: it's never replaced. Only a
//! session someone's at is named (not `claude -p`). The name is asked of a
//! small model (`claude -p --model haiku`: no tools, no hooks, nothing
//! saved, a couple of seconds), else made from the request's own words.
//! What was given is kept in `titles/<session>.json` beside the accounts
//! tool's cache.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// In the environment of the `claude -p` that names a session: its own
/// hooks never name anything (`--setting-sources ""` leaves them out anyway).
const CHILD: &str = "CLAUDE_ACCOUNTS_TITLE_CHILD";
/// How long the model may take before the request's own words stand in.
const ASK_FOR: Duration = Duration::from_secs(12);
/// A title is at most this many characters, and this many words.
const MAX_CHARS: usize = 60;
const MAX_WORDS: usize = 8;
/// Kept titles of sessions untouched this long are tidied away.
const KEEP_DAYS: u64 = 60;

/// What the model is asked (the request follows as the prompt).
const ASK: &str = "You name terminal tabs. Reply with only a short title, 2 to 6 words, naming the subject being worked on in the request: the project, tool or problem, specific enough to tell it apart from other work at a glance. Title Case. No quotes, no trailing period, no filler such as Please, Ensure, Work On or Request.";

/// What a request's words are about, for a title.
#[derive(Debug, PartialEq)]
pub struct Subject {
    pub text: String,
    /// A /goal's: it says what the session is for from now on.
    pub goal: bool,
}

/// The subject of a prompt as Claude's hooks (or its transcript) give it:
/// a /goal's condition, or a request of a few words or more. None: a
/// command (/clear, /accounts), a goal cleared, a word or two ("yes",
/// "carry on"), or something Claude itself put in (`<task-notification>`).
pub fn subject_of(prompt: &str) -> Option<Subject> {
    let p = prompt.trim();
    // As the transcript keeps a command: its name and arguments in tags.
    if p.contains("<command-name>") {
        if !p.contains("<command-name>/goal</command-name>") {
            return None;
        }
        let args = p.split_once("<command-args>").and_then(|(_, r)| r.split_once("</command-args>")).map(|(a, _)| a.trim())?;
        return goal(args);
    }
    if let Some(rest) = p.strip_prefix("/goal") {
        if rest.starts_with(char::is_whitespace) || rest.is_empty() {
            return goal(rest.trim());
        }
        return None;
    }
    if p.starts_with('/') || p.starts_with('<') || p.split_whitespace().count() < 4 {
        return None;
    }
    Some(Subject { text: p.to_string(), goal: false })
}

fn goal(args: &str) -> Option<Subject> {
    let quiet = ["", "clear", "off", "stop", "done", "none"];
    (!quiet.contains(&args.to_lowercase().as_str())).then(|| Subject { text: args.to_string(), goal: true })
}

/// What a transcript says a session is about: Claude's own title for it
/// (`ai-title`, the newest), else the newest /goal, else its first request.
/// (Some(title), _): Claude's title, used as it is.
pub fn transcript_subject(r: impl BufRead) -> (Option<String>, Option<Subject>) {
    let (mut ai, mut goal, mut first) = (None, None, None);
    for line in r.lines().map_while(Result::ok) {
        if line.contains("\"ai-title\"") {
            if let Some(t) = serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("aiTitle").and_then(Value::as_str).map(String::from)) {
                ai = Some(t);
            }
            continue;
        }
        if !line.starts_with("{\"") || !line.contains("\"type\":\"user\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("user") || v.get("isMeta") == Some(&json!(true)) {
            continue;
        }
        let Some(text) = v.pointer("/message/content").and_then(Value::as_str) else { continue };
        match subject_of(text) {
            Some(s) if s.goal => goal = Some(s),
            Some(s) if first.is_none() => first = Some(s),
            _ => {}
        }
    }
    (ai.and_then(|t| clean(&t)), goal.or(first))
}

/// A title as shown: one line, no quotes or markdown around it, no
/// trailing period, at most `MAX_WORDS` words and `MAX_CHARS` characters.
/// None: nothing left.
pub fn clean(s: &str) -> Option<String> {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.trim_start_matches(|c: char| matches!(c, '#' | '*' | '-' | '>') || c.is_whitespace());
    let line = line.strip_prefix("Title:").or_else(|| line.strip_prefix("title:")).unwrap_or(line);
    let t = line.trim().trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '“' | '”' | '.' | ':') || c.is_whitespace());
    let words: Vec<&str> = t.split_whitespace().filter(|w| !w.chars().any(char::is_control)).take(MAX_WORDS).collect();
    let mut out = words.join(" ");
    if out.chars().count() > MAX_CHARS {
        out = out.chars().take(MAX_CHARS - 1).collect::<String>().trim_end().to_string() + "…";
    }
    (!out.is_empty()).then_some(out)
}

/// A title from the request's own words, when no model answers: what
/// follows the asking ("please work to", "ensure that", "can you"), up to
/// the first sentence's end or list, capitalised.
pub fn words_title(text: &str) -> Option<String> {
    let lead = [
        "please", "pls", "can you", "could you", "would you", "will you", "i want you to", "i want to", "i'd like to", "i would like to", "i need you to", "i need to", "help me", "work to", "go ahead and", "let's", "lets", "ensure that", "ensure", "make sure that", "make sure", "also", "now", "ok", "okay", "so", "hey", "and", "then", "to",
    ];
    let mut t = text.trim().replace('\n', " ");
    // Compared on the text itself, ASCII case aside: a lowercase copy's
    // lengths can differ from the text's (the Kelvin sign's is 'k').
    let starts = |t: &str, l: &str| t.get(..l.len()).is_some_and(|h| h.eq_ignore_ascii_case(l));
    while let Some(l) = lead.iter().find(|l| starts(&t, l) && (t.len() == l.len() || t[l.len()..].starts_with([' ', ',', ':']))) {
        t = t[l.len()..].trim_start_matches([' ', ',', ':']).to_string();
    }
    let cut = [". ", "? ", "! ", "; ", ", ", " 1)", " 1.", " (1)", " - "].iter().filter_map(|c| t.find(c)).min().unwrap_or(t.len());
    let mut t = t[..cut].trim().to_string();
    // A word left hanging by the cut ("... tool to 1) ensure").
    let hanging = ["to", "and", "or", "the", "a", "an", "of", "for", "with", "on", "in", "by", "so", "that"];
    while let Some((head, last)) = t.rsplit_once(' ') {
        if !hanging.contains(&last.to_lowercase().as_str()) {
            break;
        }
        t = head.trim_end().to_string();
    }
    for p in ["my ", "the ", "a "] {
        if starts(&t, p) {
            t = t[p.len()..].to_string();
        }
    }
    let mut c = t.chars();
    let t = {
        let f = c.next()?;
        f.to_uppercase().collect::<String>() + c.as_str()
    };
    clean(&t)
}

/// A title for `text`: the model's, else its own words.
fn name(text: &str) -> Option<String> {
    ask_model(text).or_else(|| words_title(text))
}

/// The model's title for `text` (`claude -p --model haiku`), within `ASK_FOR`.
fn ask_model(text: &str) -> Option<String> {
    let exe = find_claude()?;
    let text: String = text.chars().take(4000).collect();
    let mut cmd = Command::new(exe);
    // Not the session's: a hook runs with its Claude's variables, and this
    // Claude is one of its own. Its Claude folder (CLAUDE_CONFIG_DIR) stays,
    // so it's signed in as the session is. First, so what's set below stays.
    for (k, _) in std::env::vars_os() {
        if k.to_str().is_some_and(|k| k.starts_with("CLAUDE") && k != "CLAUDE_CONFIG_DIR") {
            cmd.env_remove(&k);
        }
    }
    // The request goes on stdin: on a command line any user's `ps` could read
    // it, and one starting with "-" would read as an option.
    cmd.args(["-p", "--no-session-persistence", "--model", "haiku", "--tools", "", "--disable-slash-commands", "--strict-mcp-config", "--setting-sources", "", "--system-prompt", ASK])
        .env(CHILD, "1")
        // Only the request: no folder, branch, CLAUDE.md or memory of the
        // session's around it (a title once came out naming the project the
        // session's folder held, not what the request was about).
        .current_dir(std::env::temp_dir())
        .env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1")
        .env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::util::no_window(&mut cmd);
    let mut child = cmd.spawn().ok()?;
    if let Some(mut i) = child.stdin.take() {
        use std::io::Write;
        let _ = i.write_all(text.as_bytes());
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if start.elapsed() > ASK_FOR => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    clean(&out)
}

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// Whether the session the hook is for is one someone's at: not
/// `claude -p` (its entry point `sdk-cli`, unattended), nor the namer itself.
fn attended() -> bool {
    let (entry, at) = (var("CLAUDE_CODE_ENTRYPOINT"), var("CLAUDE_CODE_SESSION_ATTENDED"));
    var(CHILD).is_none() && entry.is_none_or(|e| e == "cli") && at.is_none_or(|a| a != "0")
}

/// Claude Code's program: its own installer's place (`~/.local/bin`), else
/// PATH (on Windows its `claude.cmd` too), else Homebrew's on macOS.
fn find_claude() -> Option<PathBuf> {
    let exe = format!("claude{}", std::env::consts::EXE_SUFFIX);
    let mut tries: Vec<PathBuf> = Vec::new();
    if let Some(h) = crate::util::home() {
        tries.push(h.join(".local").join("bin").join(&exe));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&path) {
            tries.push(d.join(&exe));
            if cfg!(target_os = "windows") {
                tries.push(d.join("claude.cmd"));
            }
        }
    }
    if cfg!(target_os = "macos") {
        tries.push(PathBuf::from("/opt/homebrew/bin/claude"));
        tries.push(PathBuf::from("/usr/local/bin/claude"));
    }
    tries.into_iter().find(|p| p.is_file())
}

// ------------------------------------------------------------------ deciding

/// What a hook does about the session's title.
#[derive(Debug, PartialEq)]
pub enum Plan {
    /// Leave it.
    Keep,
    /// Give it this title (one known already).
    Give(String),
    /// Name it from these words (`name`).
    Name(String),
}

/// What's kept of a session's title (`titles/<session>.json`).
#[derive(Debug, Default, PartialEq)]
pub struct Kept {
    pub title: Option<String>,
    /// Named by you since (`/rename`): never touched again.
    pub yours: bool,
}

/// A prompt (`prompt`) in a session whose title Claude says is `current`
/// (none until it's named) and whose kept record is `kept`; `history`: what
/// its transcript says it's about, looked at only when needed.
pub fn plan_prompt(kept: &Kept, current: Option<&str>, prompt: &str, history: impl FnOnce() -> (Option<String>, Option<Subject>)) -> Plan {
    if kept.yours {
        return Plan::Keep;
    }
    let current = current.map(str::trim).filter(|c| !c.is_empty());
    let asked = subject_of(prompt);
    match (&kept.title, current) {
        // Named, by you, since it was given (or before anything was).
        (Some(t), Some(c)) if c != t => Plan::Keep,
        (None, Some(_)) => Plan::Keep,
        (Some(t), c) => match asked {
            Some(s) if s.goal => Plan::Name(s.text),
            // Lost on the way (a resume that didn't bring it back).
            _ if c.is_none() => Plan::Give(t.clone()),
            _ => Plan::Keep,
        },
        (None, None) => match asked {
            Some(s) if s.goal => Plan::Name(s.text),
            asked => match history() {
                (Some(ai), _) => Plan::Give(ai),
                (None, Some(s)) => Plan::Name(s.text),
                (None, None) => asked.map_or(Plan::Keep, |s| Plan::Name(s.text)),
            },
        },
    }
}

/// A session starting (`source`: startup, resume, clear, compact) with the
/// kept record `kept`: a resumed one gets its title back, or one from what
/// its transcript says it's about.
pub fn plan_start(kept: &Kept, source: &str, history: impl FnOnce() -> (Option<String>, Option<Subject>)) -> Plan {
    if kept.yours || source != "resume" {
        return Plan::Keep;
    }
    if let Some(t) = &kept.title {
        return Plan::Give(t.clone());
    }
    match history() {
        (Some(ai), _) => Plan::Give(ai),
        (None, Some(s)) => Plan::Name(s.text),
        (None, None) => Plan::Keep,
    }
}

// ------------------------------------------------------------------ the hooks

fn kept_file(dir: &Path, session: &str) -> Option<PathBuf> {
    let ok = !session.is_empty() && session.len() <= 80 && session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then(|| dir.join(format!("{session}.json")))
}

fn read_kept(p: &Path) -> Kept {
    let v: Value = std::fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    Kept { title: v.get("title").and_then(Value::as_str).map(String::from), yours: v.get("yours") == Some(&json!(true)) }
}

fn write_kept(p: &Path, k: &Kept) {
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, serde_json::to_vec(&json!({"title": k.title, "yours": k.yours})).unwrap_or_default());
}

/// Kept titles of sessions untouched for `KEEP_DAYS`, away (once a day at most).
fn tidy(dir: &Path) {
    let mark = dir.join(".tidied");
    let day = Duration::from_secs(86400);
    if std::fs::metadata(&mark).and_then(|m| m.modified()).ok().and_then(|m| m.elapsed().ok()).is_some_and(|a| a < day) {
        return;
    }
    let _ = std::fs::write(&mark, b"");
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|m| m.elapsed().ok()).is_some_and(|a| a > day * KEEP_DAYS as u32);
        if old && e.path().extension().is_some_and(|x| x == "json") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn history(h: &Value) -> (Option<String>, Option<Subject>) {
    match h.get("transcript_path").and_then(Value::as_str).and_then(|p| std::fs::File::open(p).ok()) {
        Some(f) => transcript_subject(BufReader::new(f)),
        None => (None, None),
    }
}

/// Carries out a plan; the title to answer with, if any.
fn carry_out(p: &Path, kept: Kept, plan: Plan) -> Option<String> {
    let title = match plan {
        Plan::Keep => return None,
        Plan::Give(t) => t,
        Plan::Name(text) => name(&text)?,
    };
    if kept.title.as_deref() != Some(title.as_str()) {
        write_kept(p, &Kept { title: Some(title.clone()), yours: false });
    }
    Some(title)
}

/// UserPromptSubmit (`hook prompt`, its input `h`, from `dir`: where titles
/// are kept): the title the session should have now, if it should change.
pub fn on_prompt(dir: &Path, h: &Value) -> Option<String> {
    if !attended() {
        return None;
    }
    let p = kept_file(dir, h.get("session_id").and_then(Value::as_str)?)?;
    tidy(dir);
    let kept = read_kept(&p);
    let current = h.get("session_title").and_then(Value::as_str);
    let prompt = h.get("prompt").and_then(Value::as_str).unwrap_or("");
    let plan = plan_prompt(&kept, current, prompt, || history(h));
    // A name of yours since ours: noted, so a resume doesn't put ours back.
    if plan == Plan::Keep && !kept.yours && current.is_some_and(|c| !c.trim().is_empty() && Some(c.trim()) != kept.title.as_deref()) {
        write_kept(&p, &Kept { title: kept.title.clone(), yours: true });
        return None;
    }
    carry_out(&p, kept, plan)
}

/// SessionStart (`hook session-start`): a resumed session's title.
pub fn on_start(dir: &Path, h: &Value) -> Option<String> {
    if !attended() {
        return None;
    }
    let p = kept_file(dir, h.get("session_id").and_then(Value::as_str)?)?;
    let kept = read_kept(&p);
    let source = h.get("source").and_then(Value::as_str).unwrap_or("");
    let plan = plan_start(&kept, source, || history(h));
    carry_out(&p, kept, plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_requests_subject_is_its_goal_or_its_words() {
        let goal = |t: &str| Some(Subject { text: t.into(), goal: true });
        assert_eq!(subject_of("/goal please work to update my claude bottom UI tool"), goal("please work to update my claude bottom UI tool"));
        assert_eq!(subject_of("<command-name>/goal</command-name>\n<command-message>goal</command-message>\n<command-args>fix the VPN</command-args>"), goal("fix the VPN"));
        assert_eq!(subject_of("/goal clear"), None);
        assert_eq!(subject_of("/goal"), None);
        assert_eq!(subject_of("/goals list"), None);
        assert_eq!(subject_of("<command-name>/accounts</command-name>"), None);
        assert_eq!(subject_of("/clear"), None);
        assert_eq!(subject_of("yes do it"), None, "a word or two");
        assert_eq!(subject_of("<task-notification>done</task-notification>"), None);
        assert_eq!(subject_of("why does the PC switch off at 95 C?"), Some(Subject { text: "why does the PC switch off at 95 C?".into(), goal: false }));
    }

    #[test]
    fn a_title_is_one_short_clean_line() {
        assert_eq!(clean("\"Claude Accounts: Soonest Week First\".\n"), Some("Claude Accounts: Soonest Week First".into()));
        assert_eq!(clean("**Title:** PDF Viewer Fit"), Some("PDF Viewer Fit".into()));
        assert_eq!(clean("\n\n# VPN Self-Heal\nmore"), Some("VPN Self-Heal".into()));
        assert_eq!(clean("one two three four five six seven eight nine ten"), Some("one two three four five six seven eight".into()));
        let long = clean(&"Supercalifragilistic ".repeat(5)).unwrap();
        assert!(long.chars().count() <= MAX_CHARS && long.ends_with('…'), "{long}");
        assert_eq!(clean("  \n \"\" "), None);
    }

    #[test]
    fn without_a_model_the_requests_own_words() {
        assert_eq!(words_title("please work to update my claude bottom UI tool to 1) ensure its on the same version as the server").as_deref(), Some("Update my claude bottom UI tool"));
        assert_eq!(words_title("how to improve my nightly build workflow/ ci setup, ensure a linux build box is present").as_deref(), Some("How to improve my nightly build workflow/ ci"));
        assert_eq!(words_title("Also work to ensure that my claude sessions have a descriptive top; IE at a glance").as_deref(), Some("Claude sessions have a descriptive top"));
        assert_eq!(words_title("can you fix the VPN. It drops at boot").as_deref(), Some("Fix the VPN"));
        assert_eq!(words_title("please"), None);
        // A letter whose lowercase is shorter (the Kelvin sign's is 'k') never splits.
        assert_eq!(words_title("O\u{212A} fix the build cache keys now").as_deref(), Some("O\u{212A} fix the build cache keys now"));
        assert_eq!(words_title("ok, fix the build cache keys now").as_deref(), Some("Fix the build cache keys now"));
    }

    #[test]
    fn a_transcript_says_what_its_session_is_about() {
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"Caveat: the messages below were generated by the user while running local commands"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"look at why the status line says HTTP 429"}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#,
            r#"{"type":"assistant","message":{"content":"/goal not this"}}"#,
        ];
        let (ai, s) = transcript_subject(lines.join("\n").as_bytes());
        assert_eq!((ai, s.map(|s| s.text)), (None, Some("look at why the status line says HTTP 429".into())));
        let with_goal = format!("{}\n{}", lines.join("\n"), r#"{"type":"user","message":{"role":"user","content":"<command-name>/goal</command-name><command-args>usage polling never shows 429</command-args>"}}"#);
        assert_eq!(transcript_subject(with_goal.as_bytes()).1, Some(Subject { text: "usage polling never shows 429".into(), goal: true }));
        let with_ai = format!("{with_goal}\n{}\n{}", r#"{"type":"ai-title","aiTitle":"Status line 429s","sessionId":"x"}"#, r#"{"type":"ai-title","aiTitle":"Usage endpoint rate limits","sessionId":"x"}"#);
        assert_eq!(transcript_subject(with_ai.as_bytes()).0.as_deref(), Some("Usage endpoint rate limits"), "the newest of Claude's own");
    }

    #[test]
    fn the_first_prompt_names_a_session_a_goal_renames_it_and_yours_stands() {
        let none = || (None, None);
        let fresh = Kept::default();
        let ours = Kept { title: Some("VPN Self-Heal".into()), yours: false };
        let ask = "why does the tunnel show connected with no traffic";
        // First prompt, nothing known: named from it.
        assert_eq!(plan_prompt(&fresh, None, ask, none), Plan::Name(ask.into()));
        // ...unless it's a word or two: wait for one that says something.
        assert_eq!(plan_prompt(&fresh, None, "continue", none), Plan::Keep);
        // A session from before: what its transcript says, Claude's own title first.
        assert_eq!(plan_prompt(&fresh, None, "continue", || (Some("Tunnel Health".into()), None)), Plan::Give("Tunnel Health".into()));
        assert_eq!(plan_prompt(&fresh, None, "Also make the tabs say what they're for", || (None, Some(Subject { text: "fix the VPN".into(), goal: true }))), Plan::Name("fix the VPN".into()));
        // Named already: later prompts leave it.
        assert_eq!(plan_prompt(&ours, Some("VPN Self-Heal"), "also check the DNS while you're there", none), Plan::Keep);
        // A /goal says what it's for now.
        assert_eq!(plan_prompt(&ours, Some("VPN Self-Heal"), "/goal make Resume reopen Firefox tabs", none), Plan::Name("make Resume reopen Firefox tabs".into()));
        assert_eq!(plan_prompt(&fresh, None, "/goal make Resume reopen Firefox tabs", || (Some("Old".into()), None)), Plan::Name("make Resume reopen Firefox tabs".into()));
        // Lost on the way: given back.
        assert_eq!(plan_prompt(&ours, None, "next", none), Plan::Give("VPN Self-Heal".into()));
        // Yours (/rename, --name) stands, even for a goal.
        assert_eq!(plan_prompt(&ours, Some("my VPN thing"), "/goal something else entirely", none), Plan::Keep);
        assert_eq!(plan_prompt(&fresh, Some("named at start"), ask, none), Plan::Keep);
        assert_eq!(plan_prompt(&Kept { title: Some("x".into()), yours: true }, None, "/goal y z", none), Plan::Keep);
    }

    #[test]
    fn a_resumed_session_gets_its_title_back() {
        let none = || (None, None);
        let ours = Kept { title: Some("PDF Viewer Fit".into()), yours: false };
        assert_eq!(plan_start(&ours, "resume", none), Plan::Give("PDF Viewer Fit".into()));
        assert_eq!(plan_start(&ours, "startup", none), Plan::Keep);
        assert_eq!(plan_start(&Kept::default(), "resume", || (Some("Viewer recents".into()), None)), Plan::Give("Viewer recents".into()));
        assert_eq!(plan_start(&Kept::default(), "resume", || (None, Some(Subject { text: "size the viewer to the PDF".into(), goal: false }))), Plan::Name("size the viewer to the PDF".into()));
        assert_eq!(plan_start(&Kept::default(), "resume", none), Plan::Keep);
        assert_eq!(plan_start(&Kept { title: Some("t".into()), yours: true }, "resume", none), Plan::Keep);
    }

    #[test]
    fn kept_titles_are_only_ever_file_names() {
        let d = Path::new("/k");
        assert_eq!(kept_file(d, "c6077b17-fdb6-4695-909b-ac8d4657c012"), Some(PathBuf::from("/k/c6077b17-fdb6-4695-909b-ac8d4657c012.json")));
        assert_eq!(kept_file(d, "../x"), None);
        assert_eq!(kept_file(d, ""), None);
    }
}
