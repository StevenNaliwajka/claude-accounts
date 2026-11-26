//! Several Claude subscriptions on one computer, the same on Linux, macOS
//! and Windows: every account you've signed Claude Code in with is kept, the
//! one Claude uses is swapped on request (or by itself when it nears its
//! limit, or another's week ends sooner: that one's used first, as what's
//! left of a week is lost at its reset), each one's usage is tracked, and
//! Claude's status line is drawn from it.
//!
//! ```text
//! claude-accounts [list [--fresh]]     usage on every account
//! claude-accounts add [name]           keep the account Claude is signed in with now
//! claude-accounts switch <name>        make <name> the one Claude uses
//! claude-accounts next                 the next one: the soonest-ending week with room
//! claude-accounts auto [on|off|<pct>]
//! claude-accounts rename <old> <new> | remove <name> | refresh
//! claude-accounts install              Claude's status line, hooks and /accounts set up
//! claude-accounts statusline           Claude Code's statusLine (JSON on stdin)
//! claude-accounts hook <event>         Claude Code's hooks (JSON on stdin): `prompt`
//!                                      moves off a full account first; `stop-failure`
//!                                      moves off the one a turn stopped on and wakes it
//! ```
//!
//! The store: `~/.config/claude-accounts/accounts/<name>.json` (owner-only),
//! its `settings.json`, and the usage cache in `~/.cache/claude-accounts`.
//!
//! Claude's own sign-in is where Claude keeps it: `.credentials.json` in its
//! folder (Linux, Windows), the login keychain on macOS (its item "Claude
//! Code-credentials", written the way Claude writes it: `security -i`, the
//! secret as hex on stdin, never on a command line). Each account's tokens
//! are refreshed with Claude Code's own OAuth client when they run out
//! (the active one is left to Claude), and usage is asked of the same
//! endpoint Claude's /usage asks. A token that can't ask it (one narrowed to
//! inference, such as `claude setup-token` makes) has its usage read from the
//! headers of the smallest request there is (`probe_usage`), as Claude Code's
//! own quota check reads them.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Whose a token is (what Claude Code asks after a sign-in).
#[cfg_attr(test, allow(dead_code))]
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const TOKEN_URLS: [&str; 2] = ["https://platform.claude.com/v1/oauth/token", "https://console.anthropic.com/v1/oauth/token"];
/// Claude Code's OAuth client (public: it's in every copy of Claude Code).
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Usage is asked again after this many seconds: the active account, the rest.
/// The endpoint answers 429 to much more than this, and the active account's
/// weekly and 5-hour use come live from Claude Code's own status-line input
/// (`rate_limits`) between asks anyway.
const TTL_ACTIVE: f64 = 180.;
const TTL_OTHER: f64 = 300.;
/// The same for an account whose usage is read by a request (`probe_usage`):
/// each one spends a token, and the active one's numbers come live anyway.
const TTL_PROBE_ACTIVE: f64 = 600.;
const TTL_PROBE_OTHER: f64 = 900.;
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
/// The probe's model: the smallest, as Claude Code's quota check uses.
const PROBE_MODEL: &str = "claude-haiku-4-5";
/// After a miss (429, 5xx, no network): this long before asking again,
/// doubling with each miss in a row up to the cap (or what Retry-After says).
const BACKOFF_MIN: f64 = 120.;
const BACKOFF_MAX: f64 = 1800.;
/// A miss shows on the status line only once the numbers are this old.
const STALE_AFTER: f64 = 1800.;
/// Auto-switch (on unless `auto off`) never lets the active account's
/// 5-hour or weekly use pass this (`auto <pct>` sets another): it's used up
/// to its limit, and Claude moves on a request or two before it'd be refused
/// (`switch_reason`). Claude's own numbers come after every answer.
const AUTO_AT: f64 = 100.;
/// Rises in Claude's live numbers between looks at most this far apart are
/// what a request or two costs (`observe`); a longer gap says nothing of that.
const STEP_GAP: f64 = 90.;
/// The last status line's numbers count for switching from a `refresh` while
/// they're this recent.
const LIVE_FRESH: f64 = 300.;
/// Claude writes its sign-in, then `.claude.json` naming whose it is: new
/// tokens are kept only once their file is this old (a login half written).
const SETTLE: f64 = 3.;
/// macOS: Claude's keychain item.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0., |d| d.as_secs_f64())
}

// ------------------------------------------------------------------ places

struct Places {
    home: PathBuf,
    claude: PathBuf,
    claude_json: PathBuf,
    conf: PathBuf,
    cache: PathBuf,
}

/// The Claude folder Claude was started with (`CLAUDE_CONFIG_DIR`), if any.
pub fn config_dir() -> Option<std::ffi::OsString> {
    std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty())
}

fn places() -> Option<Places> {
    places_in(crate::util::home()?, config_dir())
}

fn places_in(home: PathBuf, config_dir: Option<std::ffi::OsString>) -> Option<Places> {
    let custom = config_dir.filter(|v| !v.is_empty()).map(PathBuf::from);
    let claude = custom.clone().unwrap_or_else(|| home.join(".claude"));
    let claude_json = match &custom {
        Some(d) => d.join(".claude.json"),
        None => home.join(".claude.json"),
    };
    let conf = home.join(".config").join("claude-accounts");
    Some(Places { cache: home.join(".cache").join("claude-accounts"), conf, home, claude, claude_json })
}

impl Places {
    fn accounts_dir(&self) -> PathBuf {
        self.conf.join("accounts")
    }
    fn settings(&self) -> PathBuf {
        self.conf.join("settings.json")
    }
    fn usage(&self) -> PathBuf {
        self.cache.join("usage.json")
    }
    fn creds_file(&self) -> PathBuf {
        self.claude.join(".credentials.json")
    }
    fn log(&self, msg: &str) {
        let _ = std::fs::create_dir_all(&self.cache);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.cache.join("log")) {
            let _ = writeln!(f, "{} {}", crate::util::stamp(), msg);
        }
    }
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// Write JSON whole, owner-only (`private`), beside then renamed in.
fn write_json(p: &Path, v: &Value, private: bool) -> Result<(), String> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let tmp = p.with_extension(format!("tmp.{}", std::process::id()));
    let text = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    // A temp file of a process gone (same id): out of the way, so this one's
    // made fresh, with its own permissions.
    let _ = std::fs::remove_file(&tmp);
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(if private { 0o600 } else { 0o644 });
        }
        #[cfg(not(unix))]
        let _ = private;
        let mut f = o.open(&tmp).map_err(|e| format!("{}: {e}", p.display()))?;
        f.write_all(text.as_bytes()).map_err(|e| format!("{}: {e}", p.display()))?;
        // On disk before it's renamed in: a power cut never leaves it empty.
        f.sync_all().map_err(|e| format!("{}: {e}", p.display()))?;
    }
    std::fs::rename(&tmp, p).map_err(|e| format!("{}: {e}", p.display()))
}

// ------------------------------------------------------------------ Claude's own sign-in

/// The JSON Claude keeps its sign-in in (`{"claudeAiOauth": {...}}`), for
/// reading: macOS's keychain (else the file, where Claude falls back to it),
/// elsewhere the file. None: none, or none this tool can read.
fn live_store(pl: &Places) -> Option<Value> {
    live_store_state(pl).ok().flatten()
}

/// The same for changing it: Ok(None) none there yet; Err: one is there but
/// isn't a JSON object (a form of Claude's this tool doesn't know, or a
/// keychain it can't read now), which is never written over.
fn live_store_state(pl: &Places) -> Result<Option<Value>, String> {
    #[cfg(target_os = "macos")]
    match keychain_get(None) {
        Keychain::Json(v) => return Ok(Some(v)),
        Keychain::Unknown(why) => return Err(format!("Claude's keychain item: {why}; nothing changed")),
        Keychain::Absent => {}
    }
    let f = pl.creds_file();
    match std::fs::read_to_string(&f) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", f.display())),
        Ok(t) => match serde_json::from_str::<Value>(&t) {
            Ok(v) if v.is_object() => Ok(Some(v)),
            _ => Err(format!("{} isn't a JSON object; left as it is", f.display())),
        },
    }
}

fn set_live_store(pl: &Places, v: &Value) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if matches!(keychain_get(None), Keychain::Json(_)) || !pl.creds_file().is_file() {
        return keychain_write_in(v, None);
    }
    write_json(&pl.creds_file(), v, true)
}

/// Claude's keychain item on macOS, as found.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum Keychain {
    Absent,
    Json(Value),
    /// There, but not a JSON object (or not readable now: a locked keychain).
    Unknown(String),
}

/// The account Claude files its keychain item under, as Claude Code picks it:
/// `$USER` (else `$LOGNAME`), or `claude-code-user` for none or one with
/// characters other than letters, digits, `.`, `_`, `-`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn keychain_user_from(user: Option<String>) -> String {
    match user.filter(|u| !u.is_empty()) {
        Some(u) if u.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) => u,
        _ => "claude-code-user".into(),
    }
}

#[cfg(target_os = "macos")]
fn keychain_user() -> String {
    keychain_user_from(std::env::var("USER").ok().filter(|u| !u.is_empty()).or_else(|| std::env::var("LOGNAME").ok()))
}

/// The keychain item's name, as Claude Code gives it: "Claude Code-credentials",
/// and for a Claude folder of its own (`CLAUDE_CONFIG_DIR`, or
/// `CLAUDE_SECURESTORAGE_CONFIG_DIR` when set) a dash and the first 8 hex
/// digits of that path's SHA-256 after it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn keychain_service_for(secure_dir: Option<&std::ffi::OsStr>, config_dir: Option<&std::ffi::OsStr>) -> String {
    let dir = match secure_dir {
        Some(s) => Some(s).filter(|s| !s.is_empty()),
        None => config_dir.filter(|c| !c.is_empty()),
    };
    match dir {
        None => KEYCHAIN_SERVICE.to_string(),
        Some(d) => {
            use sha2::Digest;
            let sum = sha2::Sha256::digest(d.to_string_lossy().as_bytes());
            let hex: String = sum.iter().take(4).map(|b| format!("{b:02x}")).collect();
            format!("{KEYCHAIN_SERVICE}-{hex}")
        }
    }
}

#[cfg(target_os = "macos")]
fn keychain_service() -> String {
    keychain_service_for(std::env::var_os("CLAUDE_SECURESTORAGE_CONFIG_DIR").as_deref(), std::env::var_os("CLAUDE_CONFIG_DIR").as_deref())
}

/// Claude's item in `keychain` (None: the login keychain, where Claude keeps it).
/// `security -w` prints a secret with bytes outside ASCII as hex: read either way.
#[cfg(target_os = "macos")]
fn keychain_get(keychain: Option<&Path>) -> Keychain {
    let mut cmd = Command::new("/usr/bin/security");
    cmd.args(["find-generic-password", "-a", &keychain_user(), "-s", &keychain_service(), "-w"]);
    if let Some(k) = keychain {
        cmd.arg(k);
    }
    let out = match crate::util::output(&mut cmd, Duration::from_secs(10)) {
        Ok(o) => o,
        Err(e) => return Keychain::Unknown(format!("security: {e}")),
    };
    // 44: errSecItemNotFound.
    if out.status.code() == Some(44) {
        return Keychain::Absent;
    }
    if !out.status.success() {
        return Keychain::Unknown(format!("security find-generic-password exited {}", out.status));
    }
    let text = out.stdout.trim();
    match serde_json::from_str::<Value>(text).ok().or_else(|| serde_json::from_slice(&unhex(text)?).ok()) {
        Some(v) if v.is_object() => Keychain::Json(v),
        _ => Keychain::Unknown("not JSON this tool knows".into()),
    }
}

/// "7b7d" -> b"{}"; None: not hex.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// `security -i` takes a line up to this long; Claude Code passes a longer
/// one as arguments instead, and so does this.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SECURITY_LINE_MAX: usize = 4032;

/// The `security` command that writes `v` as Claude's item, as Claude writes
/// it: the secret as hex (`-X`), on stdin (`security -i`) so it's on no
/// command line, or as arguments when the line would be too long for it.
/// (arguments, stdin).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn keychain_write_command(v: &Value, user: &str, service: &str, keychain: Option<&Path>) -> (Vec<String>, Option<String>) {
    let hex: String = v.to_string().bytes().map(|b| format!("{b:02x}")).collect();
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let mut line = format!("add-generic-password -U -a {} -s {} -X {}", quote(user), quote(service), quote(&hex));
    if let Some(k) = keychain {
        line.push(' ');
        line.push_str(&quote(&k.to_string_lossy()));
    }
    if line.len() < SECURITY_LINE_MAX {
        return (vec!["-i".into()], Some(line + "\n"));
    }
    let mut args: Vec<String> = ["add-generic-password", "-U", "-a", user, "-s", service, "-X", &hex].iter().map(|s| s.to_string()).collect();
    if let Some(k) = keychain {
        args.push(k.to_string_lossy().into_owned());
    }
    (args, None)
}

#[cfg(target_os = "macos")]
fn keychain_write_in(v: &Value, keychain: Option<&Path>) -> Result<(), String> {
    let (args, input) = keychain_write_command(v, &keychain_user(), &keychain_service(), keychain);
    let mut cmd = Command::new("/usr/bin/security");
    cmd.args(&args);
    match crate::util::run_with_input(&mut cmd, input.as_deref().unwrap_or("").as_bytes(), Duration::from_secs(20)) {
        Ok((st, _)) if st.success() => Ok(()),
        Ok((_, err)) => Err(format!("the keychain didn't take it: {}", err.trim())),
        Err(e) => Err(format!("security: {e}")),
    }
}

/// (Claude's credentials, its account record) right now.
/// A sign-in Claude has cleared (logged out, or a refresh that failed: its
/// tokens emptied, `"accessToken": ""`) is none: kept as an account's, it
/// would put that account's good tokens out of the store.
fn live(pl: &Places) -> (Option<Value>, Option<Value>) {
    let c = live_store(pl).and_then(|v| v.get("claudeAiOauth").cloned()).filter(holds_tokens);
    let o = read_json(&pl.claude_json).and_then(|v| v.get("oauthAccount").cloned()).filter(Value::is_object);
    (c, o)
}

// ------------------------------------------------------------------ the store

/// Every account kept here, by name.
fn accounts(pl: &Places) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    let Ok(rd) = std::fs::read_dir(pl.accounts_dir()) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(stem) = name.strip_suffix(".json") {
            // An account holds a sign-in; anything else here (an older tool's
            // cache files, as on Windows) isn't one.
            if let Some(a) = read_json(&e.path()).filter(|a| a.get("credentials").is_some_and(Value::is_object) || a.get("oauthAccount").is_some_and(Value::is_object)) {
                out.insert(stem.to_string(), a);
            }
        }
    }
    out
}

/// Whether an account's usage is read by a request (`probe_usage`): its token
/// can't ask the usage endpoint (no `user:profile`: a `setup-token`
/// token, or a sign-in narrowed to inference).
pub fn usage_by_probe(a: &Value) -> bool {
    a.pointer("/credentials/scopes").and_then(Value::as_array).is_some_and(|s| !s.iter().any(|x| x.as_str() == Some("user:profile")))
}

/// Whether a sign-in (`credentials` / Claude's `claudeAiOauth`) holds a token
/// at all: one Claude cleared has them empty.
pub fn holds_tokens(c: &Value) -> bool {
    ["accessToken", "refreshToken"].iter().any(|k| c.get(*k).and_then(Value::as_str).is_some_and(|t| !t.trim().is_empty()))
}

/// Whether an account's sign-in can be refreshed (it has a refresh token).
fn refreshable(a: &Value) -> bool {
    a.pointer("/credentials/refreshToken").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
}

/// A name that's safe as a file name in the store.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

fn save_account(pl: &Places, name: &str, a: &Value) -> Result<(), String> {
    write_json(&pl.accounts_dir().join(format!("{name}.json")), a, true)
}

fn email_of(a: &Value) -> &str {
    a.get("oauthAccount").and_then(|o| o.get("emailAddress")).and_then(Value::as_str).unwrap_or("")
}

/// An account's name from its email: what's before the @, made safe.
pub fn default_name(email: &str) -> String {
    let local = email.split('@').next().filter(|l| !l.is_empty()).unwrap_or("account");
    local.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect()
}

/// How long anything waits for the store's lock before it goes on without
/// (a hook that waited longer would be ended by Claude).
const LOCK_WAIT: Duration = Duration::from_secs(20);

/// Our own changes to the store and to Claude's sign-in, one process at a
/// time (every Claude session runs the status line, and any may switch):
/// held until dropped. `wait`: up to `LOCK_WAIT`; false: None at once when
/// another holds it.
fn store_lock(pl: &Places, wait: bool) -> Option<std::fs::File> {
    std::fs::create_dir_all(&pl.cache).ok()?;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(pl.cache.join("store.lock")).ok()?;
    let start = std::time::Instant::now();
    loop {
        match f.try_lock() {
            Ok(()) => return Some(f),
            Err(std::fs::TryLockError::WouldBlock) if wait && start.elapsed() < LOCK_WAIT => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
}

/// Whether two sign-ins hold the same tokens (so are one account's).
pub fn same_tokens(a: Option<&Value>, b: &Value) -> bool {
    let tok = |v: Option<&Value>, k: &str| v.and_then(|v| v.get(k)).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    ["refreshToken", "accessToken"].iter().any(|k| tok(a, k).is_some_and(|t| tok(Some(b), k) == Some(t)))
}

/// Which kept account Claude's sign-in `c` is, and whether by its tokens:
/// those are what Claude sends, so they decide; else the one `.claude.json`
/// (`o`) names (tokens Claude refreshed, which none has yet).
pub fn whose(accs: &BTreeMap<String, Value>, c: &Value, o: &Value) -> Option<(String, bool)> {
    if let Some((n, _)) = accs.iter().find(|(_, a)| same_tokens(a.get("credentials"), c)) {
        return Some((n.clone(), true));
    }
    let uuid = o.get("accountUuid").and_then(Value::as_str).filter(|u| !u.is_empty())?;
    accs.iter().find(|(_, a)| a.pointer("/oauthAccount/accountUuid").and_then(Value::as_str) == Some(uuid)).map(|(n, _)| (n.clone(), false))
}

/// Whose a sign-in's tokens are, as the API says (`/api/oauth/profile`, as
/// Claude Code asks it), as an `oauthAccount` record. None: not known (no
/// network, or a token that may not ask).
fn token_owner(c: &Value) -> Option<Value> {
    let token = c.get("accessToken").and_then(Value::as_str).filter(|t| !t.is_empty())?;
    #[cfg(test)]
    return tests::OWNERS.with(|m| m.borrow().get(token).cloned());
    #[cfg(not(test))]
    let r = agent().get(PROFILE_URL).set("Authorization", &format!("Bearer {token}")).set("Content-Type", "application/json").call().ok()?;
    #[cfg(not(test))]
    owner_record(&r.into_json::<Value>().ok()?)
}

/// A profile answer (`{"account": {"uuid", "email"}, "organization": {"uuid"}}`)
/// as the `oauthAccount` record Claude keeps.
pub fn owner_record(v: &Value) -> Option<Value> {
    let uuid = v.pointer("/account/uuid").and_then(Value::as_str).filter(|u| !u.is_empty())?;
    let mut o = json!({"accountUuid": uuid});
    if let Some(e) = v.pointer("/account/email").and_then(Value::as_str) {
        o["emailAddress"] = json!(e);
    }
    if let Some(org) = v.pointer("/organization/uuid").and_then(Value::as_str) {
        o["organizationUuid"] = json!(org);
    }
    Some(o)
}

/// Whether Claude's sign-in file has stood a moment (none: the keychain's, taken as it is).
fn settled(pl: &Places) -> bool {
    let age = std::fs::metadata(pl.creds_file()).and_then(|m| m.modified()).ok().and_then(|m| SystemTime::now().duration_since(m).ok());
    age.is_none_or(|a| a.as_secs_f64() >= SETTLE)
}

/// The live (maybe refreshed by Claude) credentials into the store; a newly
/// signed-in account is kept under a name of its own. The active account's name.
fn sync_live(pl: &Places, hint: Option<&str>, say: bool) -> Option<String> {
    let _held = store_lock(pl, true);
    sync_live_locked(pl, hint, say, true)
}

/// The status line's `sync_live`: never waits. With the store busy (another
/// switching or refreshing), only says who's active.
fn sync_live_quick(pl: &Places) -> Option<String> {
    match store_lock(pl, false) {
        Some(_held) => sync_live_locked(pl, None, false, false),
        None => {
            let (Some(c), Some(o)) = live(pl) else { return None };
            whose(&accounts(pl), &c, &o).map(|w| w.0)
        }
    }
}

/// `sync_live` with the store's lock held. New tokens are kept only once
/// Claude's done writing whose they are (`wait`: waits for that, else says
/// who's active and keeps them next time).
fn sync_live_locked(pl: &Places, hint: Option<&str>, say: bool, wait: bool) -> Option<String> {
    let (Some(mut c), Some(mut o)) = live(pl) else { return None };
    let mut accs = accounts(pl);
    let mut found = whose(&accs, &c, &o);
    if !found.as_ref().is_some_and(|f| f.1) {
        // Tokens no account holds yet: a sign-in, or ones Claude refreshed.
        // Kept only by what can wait (never the status line, which neither
        // waits nor asks the network): once Claude's done writing whose they
        // are, and then under whose they really are.
        if !wait {
            return found.map(|f| f.0);
        }
        if !settled(pl) {
            std::thread::sleep(Duration::from_secs_f64(SETTLE));
            let (Some(c2), Some(o2)) = live(pl) else { return None };
            (c, o) = (c2, o2);
            accs = accounts(pl);
            found = whose(&accs, &c, &o);
        }
        if !found.as_ref().is_some_and(|f| f.1) {
            // `.claude.json` can name another account than these tokens' (a
            // sign-in or switch half written, Claude writing back the record
            // it had; on macOS there's no file time to wait on): the API says.
            if let Some(owner) = token_owner(&c) {
                if owner.get("accountUuid") != o.get("accountUuid") {
                    let uuid = owner.get("accountUuid").and_then(Value::as_str);
                    found = accs.iter().find(|(_, a)| a.pointer("/oauthAccount/accountUuid").and_then(Value::as_str) == uuid).map(|(n, _)| (n.clone(), false));
                    pl.log(&format!("new tokens are {}'s, not the account Claude's record names", found.as_ref().map_or("a new account", |f| f.0.as_str())));
                    // That account's own record, or for a new one what the API says.
                    o = found.as_ref().and_then(|f| accs.get(&f.0)).and_then(|a| a.get("oauthAccount")).filter(|r| r.is_object()).cloned().unwrap_or(owner);
                }
            }
        }
    }
    let name = match (found.map(|f| f.0), hint) {
        (None, _) => {
            let base = hint.map(String::from).unwrap_or_else(|| default_name(o.get("emailAddress").and_then(Value::as_str).unwrap_or("")));
            let mut name = base.clone();
            let mut i = 2;
            while accs.contains_key(&name) {
                name = format!("{base}{i}");
                i += 1;
            }
            if say {
                println!("saved new account '{name}' ({})", o.get("emailAddress").and_then(Value::as_str).unwrap_or(""));
            }
            pl.log(&format!("captured new account {name}"));
            name
        }
        // Kept under another name: renamed, never over an account there.
        (Some(n), Some(h)) if h != n && !accs.contains_key(h) => match std::fs::rename(pl.accounts_dir().join(format!("{n}.json")), pl.accounts_dir().join(format!("{h}.json"))) {
            Ok(()) => h.to_string(),
            Err(_) => n,
        },
        (Some(n), _) => n,
    };
    let mut a = accounts(pl).remove(&name).unwrap_or_else(|| json!({}));
    // `.claude.json` naming another account than these tokens' (a switch
    // half done, or Claude writing back what it had): the account's own
    // record stands, never the other's.
    let kept = a.pointer("/oauthAccount/accountUuid").and_then(Value::as_str);
    if kept.is_some() && kept != o.get("accountUuid").and_then(Value::as_str) {
        o = a["oauthAccount"].clone();
    }
    if a.get("credentials") != Some(&c) || a.get("oauthAccount") != Some(&o) {
        a["credentials"] = c;
        a["oauthAccount"] = o;
        a["savedAt"] = json!(now());
        let _ = save_account(pl, &name, &a);
    }
    Some(name)
}

/// Make `name` the account Claude uses. Running Claude sessions take it on
/// their next request: Claude Code reads its sign-in again whenever the file
/// (macOS: the keychain item) changes.
/// Claude moved to `name` by hand (`switch`, `next`): auto-switch leaves it
/// there until it's full (`held_by_hand`).
fn activate(pl: &Places, name: &str) -> Result<(), String> {
    let _held = store_lock(pl, true);
    activate_locked(pl, name, false, true)
}

/// `activate` with the store's lock held; `refused`: the account left was
/// just refused at a limit (`note_live`); `by_hand`: not auto-switch's move.
fn activate_locked(pl: &Places, name: &str, refused: bool, by_hand: bool) -> Result<(), String> {
    let accs = accounts(pl);
    if !accs.contains_key(name) {
        return Err(format!("no account '{name}'. Have: {}", if accs.is_empty() { "none".into() } else { accs.keys().cloned().collect::<Vec<_>>().join(", ") }));
    }
    if let Some(old) = sync_live_locked(pl, None, false, true).filter(|old| old != name) {
        note_live(pl, &old, &last_live(pl), refused);
    }
    let a = accounts(pl).remove(name).ok_or("it went away")?;
    let mut store = live_store_state(pl)?.unwrap_or_else(|| json!({}));
    store["claudeAiOauth"] = a.get("credentials").cloned().unwrap_or(Value::Null);
    set_live_store(pl, &store)?;
    // None yet (a Claude folder Claude hasn't run in): one that has Claude
    // skip its first-run screens; one that's there but won't read is yours to
    // fix, never written over.
    let mut cj = match read_json(&pl.claude_json) {
        Some(v) if v.is_object() => v,
        None if !pl.claude_json.exists() => json!({"hasCompletedOnboarding": true}),
        _ => return Err(format!("can't read {}", pl.claude_json.display())),
    };
    cj["oauthAccount"] = a.get("oauthAccount").cloned().unwrap_or(Value::Null);
    write_json(&pl.claude_json, &cj, true)?;
    // When (macOS has no sign-in file to tell): refusals before it were another account's.
    let by = if by_hand { "hand" } else { "auto" };
    let _ = write_json(&pl.cache.join("switched"), &json!({"to": name, "at": now(), "by": by}), true);
    pl.log(&format!("switched to {name}{}", if by_hand { " (by hand: kept until it's full)" } else { "" }));
    Ok(())
}

// ------------------------------------------------------------------ tokens and usage

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(8)).user_agent("claude-accounts/1.0").build()
}

#[derive(Debug, PartialEq)]
enum Refreshed {
    Yes,
    No,
    /// It's the one Claude uses now (switched to meanwhile): Claude's to refresh.
    Claudes,
}

/// An account Claude isn't using gets new tokens, with the store's lock held:
/// one token refreshed twice (here and by a Claude just switched to it) leaves
/// the loser signed out.
fn refresh_token(pl: &Places, name: &str, a: &mut Value) -> Refreshed {
    let _held = store_lock(pl, true);
    let accs = accounts(pl);
    if let (Some(c), Some(o)) = live(pl) {
        if whose(&accs, &c, &o).is_some_and(|w| w.0 == name) {
            return Refreshed::Claudes;
        }
    }
    if let Some(kept) = accs.get(name) {
        // Already refreshed by another (`a` is what this run started with).
        let newer = kept.pointer("/credentials/accessToken") != a.pointer("/credentials/accessToken");
        *a = kept.clone();
        if newer {
            return Refreshed::Yes;
        }
    }
    if refresh_token_locked(pl, name, a) { Refreshed::Yes } else { Refreshed::No }
}

fn refresh_token_locked(pl: &Places, name: &str, a: &mut Value) -> bool {
    let Some(rt) = a.get("credentials").and_then(|c| c.get("refreshToken")).and_then(Value::as_str).map(String::from) else { return false };
    let mut last = String::new();
    for url in TOKEN_URLS {
        let got = agent().post(url).set("Content-Type", "application/json").send_json(json!({"grant_type": "refresh_token", "refresh_token": rt, "client_id": CLIENT_ID}));
        match got.map(|r| r.into_json::<Value>()) {
            Ok(Ok(r)) if r.get("access_token").and_then(Value::as_str).is_some() => {
                let c = &mut a["credentials"];
                c["accessToken"] = r["access_token"].clone();
                if let Some(nrt) = r.get("refresh_token").filter(|v| v.is_string()) {
                    c["refreshToken"] = nrt.clone();
                }
                let secs = r.get("expires_in").and_then(Value::as_f64).unwrap_or(28800.);
                c["expiresAt"] = json!(((now() + secs) * 1000.) as i64);
                // What the new tokens may do (a sign-in can come back narrower).
                if let Some(sc) = r.get("scope").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                    c["scopes"] = json!(sc.split_whitespace().collect::<Vec<_>>());
                }
                a["savedAt"] = json!(now());
                let _ = save_account(pl, name, a);
                pl.log(&format!("refreshed token for {name}"));
                return true;
            }
            Ok(Ok(_)) => last = "no access_token in the answer".into(),
            Ok(Err(e)) => last = e.to_string(),
            Err(ureq::Error::Status(code, r)) => last = format!("{code} {}", r.into_string().unwrap_or_default().chars().take(200).collect::<String>()),
            Err(e) => last = e.to_string(),
        }
    }
    pl.log(&format!("token refresh failed for {name}: {last}"));
    false
}

fn fetch_usage(pl: &Places, name: &str, a: &mut Value, active: bool, again: bool) -> Value {
    let expires_at = a.get("credentials").and_then(|c| c.get("expiresAt")).and_then(Value::as_f64);
    // A token with no end and nothing to refresh (`setup-token`'s) is used as it is.
    if refreshable(a) || expires_at.is_some() {
        let expires = expires_at.unwrap_or(0.) / 1000.;
        if expires < now() + 60. {
            if active {
                return json!({"error": "token expired (Claude refreshes it on next use)"});
            }
            match refresh_token(pl, name, a) {
                Refreshed::Yes => {}
                Refreshed::No => return json!({"error": "sign-in expired: /login with it again"}),
                Refreshed::Claudes => return json!({"error": "switched to meanwhile", "miss": true}),
            }
        }
    }
    let token = a.get("credentials").and_then(|c| c.get("accessToken")).and_then(Value::as_str).unwrap_or("").to_string();
    if usage_by_probe(a) {
        return probe_usage(&token);
    }
    match agent().get(USAGE_URL).set("Authorization", &format!("Bearer {token}")).set("anthropic-beta", "oauth-2025-04-20").call() {
        Ok(r) => r.into_json().unwrap_or_else(|e| json!({"error": format!("{e}")})),
        Err(ureq::Error::Status(401, _)) if !active && !again && refresh_token(pl, name, a) == Refreshed::Yes => fetch_usage(pl, name, a, active, true),
        // Passing: rate-limited (it answers 429 with "Retry-After: 0"), down,
        // or no network. The last numbers stand; `refresh_all` waits a while.
        Err(ureq::Error::Status(code, r)) if code == 429 || code >= 500 => {
            let after = r.header("retry-after").and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.);
            json!({"error": format!("HTTP {code}"), "miss": true, "retry_after": after})
        }
        Err(ureq::Error::Status(code, _)) => json!({"error": format!("HTTP {code}")}),
        Err(ureq::Error::Transport(t)) => json!({"error": format!("{:?}", t.kind()), "miss": true}),
    }
}

/// The usage of an account whose token can't ask the usage endpoint (an
/// inference-only one, such as `setup-token` makes), from the headers of the
/// smallest request there is: one token of the smallest model, the request
/// Claude Code's own quota check makes. One refused at its limit costs
/// nothing and still says where it stands.
pub fn probe_usage(token: &str) -> Value {
    let body = json!({"model": PROBE_MODEL, "max_tokens": 1, "messages": [{"role": "user", "content": "quota"}]});
    let res = agent().post(MESSAGES_URL).set("Authorization", &format!("Bearer {token}")).set("anthropic-beta", "oauth-2025-04-20").set("anthropic-version", "2023-06-01").send_json(body);
    match res {
        Ok(r) => usage_from_headers(r.status(), |k| r.header(k).map(String::from)),
        Err(ureq::Error::Status(code, r)) => usage_from_headers(code, |k| r.header(k).map(String::from)),
        Err(ureq::Error::Transport(t)) => json!({"error": format!("{:?}", t.kind()), "miss": true}),
    }
}

/// An answer's unified rate-limit headers (`anthropic-ratelimit-unified-5h-*`,
/// `-7d-*`: a fraction used, and when it resets in Unix seconds) in the usage
/// endpoint's shape (`five_hour`/`seven_day`: percent, reset), for
/// `summarize`. None there: what the answer's status says.
pub fn usage_from_headers(code: u16, header: impl Fn(&str) -> Option<String>) -> Value {
    let num = |k: &str| header(&format!("anthropic-ratelimit-unified-{k}")).and_then(|v| v.trim().parse::<f64>().ok()).filter(|n| n.is_finite());
    let mut u = Map::new();
    for (k, out) in [("5h", "five_hour"), ("7d", "seven_day")] {
        if let Some(used) = num(&format!("{k}-utilization")) {
            u.insert(out.into(), json!({"utilization": (used * 1000.).round() / 10., "resets_at": num(&format!("{k}-reset"))}));
        }
    }
    if !u.is_empty() {
        u.insert("probed".into(), json!(true));
        return Value::Object(u);
    }
    match code {
        401 | 403 => json!({"error": format!("HTTP {code}: its token was refused (revoked, or expired)")}),
        429 | 500.. => json!({"error": format!("HTTP {code}"), "miss": true, "retry_after": header("retry-after").and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.)}),
        200..=299 => json!({"error": "no usage in Claude's answer"}),
        c => json!({"error": format!("HTTP {c}")}),
    }
}

/// How long to wait after the `misses`-th miss in a row.
pub fn backoff(misses: f64, retry_after: f64) -> f64 {
    (BACKOFF_MIN * 2f64.powf((misses - 1.).max(0.))).min(BACKOFF_MAX).max(retry_after)
}

/// Whether two resets are the same window's: the API's `resets_at` moves by
/// a fraction of a second from one answer to the next.
fn same_reset(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(a), Some(b)) => (a - b).abs() < 600.,
        _ => a.is_null() && b.is_null(),
    }
}

/// "2026-11-20T15:00:00.5+00:00" (or …Z) -> Unix seconds.
pub fn parse_ts(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (num(0, 4)?, num(5, 7)?, num(8, 10)?, num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut rest = &s[19..];
    let mut frac = 0.;
    if let Some(r) = rest.strip_prefix('.') {
        let digits: String = r.chars().take_while(char::is_ascii_digit).collect();
        frac = format!("0.{digits}").parse().unwrap_or(0.);
        rest = &r[digits.len()..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        o if o.len() >= 6 && (o.starts_with('+') || o.starts_with('-')) => {
            let oh: i64 = o.get(1..3)?.parse().ok()?;
            let om: i64 = o.get(4..6)?.parse().ok()?;
            let sign = if o.starts_with('-') { -1 } else { 1 };
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    let days = crate::util::days_from_civil(y, mo as u32, d as u32);
    Some((days * 86400 + h * 3600 + mi * 60 + se - offset) as f64 + frac)
}

/// An /api/oauth/usage answer, cut to what's shown.
pub fn summarize(u: &Value) -> Value {
    if u.is_null() || u.get("error").is_some() {
        return json!({"error": u.get("error").cloned().unwrap_or_else(|| json!("no data"))});
    }
    let mut s = Map::new();
    for (key, out) in [("five_hour", "5h"), ("seven_day", "wk")] {
        if let Some(w) = u.get(key) {
            if let Some(p) = w.get("utilization").and_then(Value::as_f64) {
                // The endpoint's reset is a time string; a probe's, Unix seconds.
                let reset = w.get("resets_at").and_then(|r| r.as_f64().or_else(|| r.as_str().and_then(parse_ts)));
                s.insert(out.into(), json!({"pct": p, "reset": reset}));
            }
        }
    }
    for lim in u.get("limits").and_then(Value::as_array).into_iter().flatten() {
        if lim.get("kind").and_then(Value::as_str) == Some("weekly_scoped") {
            if let Some(p) = lim.get("percent").and_then(Value::as_f64) {
                let model = lim.pointer("/scope/model/display_name").and_then(Value::as_str).unwrap_or("model");
                let entry = json!({"model": model, "pct": p, "reset": lim.get("resets_at").and_then(Value::as_str).and_then(parse_ts), "active": lim.get("is_active").cloned().unwrap_or(Value::Null)});
                if let Some(a) = s.entry("scoped").or_insert_with(|| json!([])).as_array_mut() { a.push(entry) }
            }
        }
    }
    if let Some(l) = u.pointer("/seven_day/locked_reason").filter(|l| !l.is_null() && l.as_str() != Some("")) {
        s.insert("locked".into(), l.clone());
    }
    Value::Object(s)
}

fn load_cache(pl: &Places) -> Map<String, Value> {
    match read_json(&pl.usage()) {
        Some(Value::Object(mut m)) => {
            m.retain(|_, e| e.is_object());
            m
        }
        _ => Map::new(),
    }
}

fn fetched(e: Option<&Value>) -> f64 {
    e.and_then(|e| e.get("fetched")).and_then(Value::as_f64).unwrap_or(0.)
}

fn ttl(name: &str, active: Option<&str>, probed: bool) -> f64 {
    match (Some(name) == active, probed) {
        (true, false) => TTL_ACTIVE,
        (false, false) => TTL_OTHER,
        (true, true) => TTL_PROBE_ACTIVE,
        (false, true) => TTL_PROBE_OTHER,
    }
}

/// Whether an account's usage is due to be asked again: old enough, and not
/// waiting out a miss. `probed`: it's read by a request (`usage_by_probe`).
fn due(e: Option<&Value>, name: &str, active: Option<&str>, probed: bool, t: f64) -> bool {
    let waiting = e.and_then(|e| e.get("retry_at")).and_then(Value::as_f64).is_some_and(|r| t < r);
    !waiting && t - fetched(e) >= ttl(name, active, probed)
}

/// When the numbers shown were last asked successfully (older caches: the last ask).
fn got_at(e: &Value) -> f64 {
    e.get("ok").and_then(Value::as_f64).unwrap_or_else(|| fetched(Some(e)))
}

/// Every account's usage asked again where it's due (all, `force`), one
/// refresh at a time (another running: the cache as it is).
fn refresh_all(pl: &Places, force: bool) -> Map<String, Value> {
    let _ = std::fs::create_dir_all(&pl.cache);
    let Ok(lock) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(pl.cache.join("lock")) else { return load_cache(pl) };
    if lock.try_lock().is_err() {
        return load_cache(pl);
    }
    let active = sync_live(pl, None, false);
    let cache = load_cache(pl);
    let accs = accounts(pl);
    let (mut asked, mut missed) = (Map::new(), Vec::new());
    for (name, mut a) in accs.clone() {
        let mut ent = cache.get(&name).cloned().unwrap_or_else(|| json!({}));
        // `--force` skips the wait between asks, never a miss's: asking a
        // 429 again only keeps it coming.
        let waiting = ent.get("retry_at").and_then(Value::as_f64).is_some_and(|r| now() < r);
        if waiting || (!force && !due(Some(&ent), &name, active.as_deref(), usage_by_probe(&a), now())) {
            continue;
        }
        let is_active = active.as_deref() == Some(name.as_str());
        let raw = fetch_usage(pl, &name, &mut a, is_active, false);
        let s = summarize(&raw);
        if let Some(err) = s.get("error").filter(|_| raw.get("miss") == Some(&json!(true))) {
            // A miss: keep what was there (or the error, with nothing yet), and wait.
            let misses = ent.get("misses").and_then(Value::as_f64).unwrap_or(0.) + 1.;
            let wait = backoff(misses, raw.get("retry_after").and_then(Value::as_f64).unwrap_or(0.));
            if ent.get("usage").is_none() {
                ent["usage"] = s.clone();
            }
            ent["missed"] = err.clone();
            ent["misses"] = json!(misses);
            ent["retry_at"] = json!(now() + wait);
            missed.push(name.clone());
            pl.log(&format!("usage for {name}: {} (miss {misses}), asking again in {}", err.as_str().unwrap_or("?"), dur(Some(wait), false)));
        } else if s.get("error").is_some() && ent.pointer("/usage/wk").is_some() {
            ent["error"] = s["error"].clone();
            if let Some(o) = ent.as_object_mut() {
                for k in ["missed", "misses", "retry_at"] {
                    o.remove(k);
                }
            }
        } else {
            let mut history: Vec<Value> = ent.get("history").and_then(Value::as_array).cloned().unwrap_or_default();
            if let Some(wk) = s.get("wk") {
                let reset = wk.get("reset").cloned().unwrap_or(Value::Null);
                history.retain(|x| now() - x[0].as_f64().unwrap_or(0.) < 86400. && same_reset(&x[2], &reset));
                history.push(json!([now(), wk["pct"], reset]));
                let n = history.len();
                if n > 200 {
                    history.drain(..n - 200);
                }
            }
            ent = json!({"usage": s, "history": history});
            if s.get("error").is_none() {
                ent["ok"] = json!(now());
            }
        }
        ent["fetched"] = json!(now());
        asked.insert(name, ent);
    }
    // Into the cache as it is now: a switch meanwhile noted the numbers of
    // the account it left (`note_live`), and those stand unless asked again.
    let cache = {
        let _held = store_lock(pl, true);
        let mut cache = load_cache(pl);
        for (n, mut e) in asked {
            if let Some(now_ent) = cache.get(&n).filter(|_| missed.contains(&n)) {
                for k in ["usage", "refused"] {
                    if let Some(v) = now_ent.get(k) {
                        e[k] = v.clone();
                    }
                }
            }
            cache.insert(n, e);
        }
        cache.retain(|n, _| accs.contains_key(n));
        let _ = write_json(&pl.usage(), &Value::Object(cache.clone()), true);
        cache
    };
    auto_switch(pl, &last_live(pl), true);
    let _ = lock.unlock();
    cache
}

/// The status line's: the cache as it is, and a refresh started in the
/// background (this program again, detached) when any of it is due.
fn refresh_bg(pl: &Places) -> (Map<String, Value>, Option<String>) {
    let cache = load_cache(pl);
    let active = sync_live_quick(pl);
    let stale = accounts(pl).iter().any(|(n, a)| due(cache.get(n), n, active.as_deref(), usage_by_probe(a), now()));
    if stale {
        let Ok(exe) = std::env::current_exe() else { return (cache, active) };
        // The same Claude folder (CLAUDE_CONFIG_DIR) goes with the environment.
        let mut cmd = Command::new(exe);
        cmd.arg("refresh").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        crate::util::no_window(&mut cmd);
        let _ = cmd.spawn();
    }
    (cache, active)
}

// ------------------------------------------------------------------ choosing

fn window(u: Option<&Value>, k: &str) -> Option<(f64, Option<f64>)> {
    let w = u?.get(k)?;
    Some((w.get("pct")?.as_f64()?, w.get("reset").and_then(Value::as_f64)))
}

/// The highest use that would stop it at `t`, and when that clears. A window
/// past its reset is empty whatever it said (numbers from before it).
pub fn worst(u: Option<&Value>, t: f64) -> (f64, Option<f64>) {
    let mut best = (0., None);
    for k in ["5h", "wk"] {
        if let Some((p, r)) = window(u, k) {
            let (p, r) = if r.is_some_and(|r| r <= t) { (0., None) } else { (p, r) };
            if p >= best.0 {
                best = (p, r);
            }
        }
    }
    best
}

/// The room an account has left; -1: none known, or it can't be used (its
/// sign-in gone, a refusal).
fn headroom(ent: Option<&Value>, t: f64) -> f64 {
    let Some(ent) = ent else { return -1. };
    // Refused at a limit since it was last asked: full, whatever it said.
    if ent.get("refused").and_then(Value::as_f64).is_some_and(|r| r >= fetched(Some(ent))) {
        return -1.;
    }
    match (problem(ent), ent.get("usage")) {
        (Problem::Error(_), _) | (_, None) => -1.,
        (_, Some(u)) if u.get("error").is_some() => -1.,
        (_, Some(u)) => 100. - worst(Some(u), t).0,
    }
}

/// The room an account has under the mark `at`; negative: none known, or it
/// can't be used (`headroom`).
fn room(ent: Option<&Value>, t: f64, at: f64) -> f64 {
    let h = headroom(ent, t);
    if h < 0. { h } else { h - (100. - at) }
}

/// When an account's week ends, and what's left of it is lost: its weekly
/// reset; never (infinity) for a week not started (no reset given) or one
/// past its reset (numbers from before it).
pub fn week_ends(ent: Option<&Value>, t: f64) -> f64 {
    match window(ent.and_then(|e| e.get("usage")), "wk") {
        Some((_, Some(r))) if r > t => r,
        _ => f64::INFINITY,
    }
}

/// An account whose week ends sooner than the active one's by more than this
/// is moved to (`auto_target`): less, and they end together.
const SAME_END: f64 = 600.;

/// The order accounts with room are used in: the week that ends soonest
/// first (what's left of it is lost at its reset, while a later week's
/// keeps), then the most room, then by name.
fn use_order(cache: &Map<String, Value>, t: f64, at: f64) -> impl Fn(&&String, &&String) -> std::cmp::Ordering + '_ {
    move |a, b| {
        let key = |n: &String| (week_ends(cache.get(n), t), -room(cache.get(n), t, at));
        let (ka, kb) = (key(a), key(b));
        ka.0.total_cmp(&kb.0).then(ka.1.total_cmp(&kb.1)).then(a.cmp(b))
    }
}

/// Where Claude goes from `active`: of the other accounts with room (more
/// than 2 points under the mark `at`; a sign-in gone or a refusal is none),
/// the one used first (`use_order`): the one whose week ends soonest.
pub fn pick_next(names: &[String], active: Option<&str>, cache: &Map<String, Value>, t: f64, at: f64) -> Option<String> {
    names.iter().filter(|n| Some(n.as_str()) != active && room(cache.get(*n), t, at) > 2.).min_by(use_order(cache, t, at)).cloned()
}

/// Seconds until the weekly limit at the recent pace; None: not before it resets.
pub fn projection(ent: &Value, t: f64) -> Option<f64> {
    let (pct, reset) = window(ent.get("usage"), "wk")?;
    let reset = reset?;
    if pct >= 100. {
        return Some(0.);
    }
    let hist: Vec<(f64, f64)> = ent.get("history").and_then(Value::as_array).into_iter().flatten().filter_map(|x| Some((x.get(0)?.as_f64()?, x.get(1)?.as_f64()?))).collect();
    let recent: Vec<&(f64, f64)> = hist.iter().filter(|x| t - x.0 < 3. * 3600.).collect();
    let mut rate = None;
    if recent.len() >= 2 {
        let (a, b) = (recent[0], recent[recent.len() - 1]);
        if b.0 - a.0 > 600. && b.1 > a.1 {
            rate = Some((b.1 - a.1) / (b.0 - a.0));
        }
    }
    if rate.is_none() {
        let elapsed = t - (reset - 7. * 86400.);
        if elapsed > 3600. && pct > 0. {
            rate = Some(pct / elapsed);
        }
    }
    let rate = rate.filter(|r| *r > 0.)?;
    let eta = (100. - pct) / rate;
    (t + eta < reset).then_some(eta)
}

/// (Auto-switch on, the mark it never lets the active account pass). On
/// unless `auto off`; 100% unless `auto <pct>`.
fn settings(pl: &Places) -> (bool, f64) {
    let s = read_json(&pl.settings()).unwrap_or_else(|| json!({}));
    (s.get("auto_switch").and_then(Value::as_bool).unwrap_or(true), s.get("auto_switch_at").and_then(Value::as_f64).unwrap_or(AUTO_AT))
}

/// Whether Claude's live numbers (`d`, its status-line input) are the active
/// account's: not when their week is another account's (a session that
/// hasn't asked since a switch still sends the numbers of the one it used).
pub fn live_is_ours(d: &Value, cache: &Map<String, Value>, active: &str) -> bool {
    let Some(r) = d.pointer("/rate_limits/seven_day/resets_at").and_then(Value::as_f64) else { return true };
    let wk_reset = |e: &Value| window(e.get("usage"), "wk").and_then(|w| w.1);
    let close = |o: f64| (o - r).abs() < 600.;
    cache.get(active).and_then(wk_reset).is_some_and(close) || !cache.iter().any(|(n, e)| n != active && wk_reset(e).is_some_and(close))
}

/// The active account's entry as it stands at `t`: Claude's live numbers
/// laid over the last asked (`with_live`) when they're its own.
pub fn active_view(d: &Value, cache: &Map<String, Value>, active: &str, t: f64) -> Value {
    let ent = cache.get(active).cloned().unwrap_or_else(|| json!({}));
    if live_is_ours(d, cache, active) { with_live(d, ent, t) } else { ent }
}

/// One window of an account as Claude's live numbers have shown it
/// (`live.json`): the most it's been seen at, when last looked, and the last
/// rises between looks close together (what a request or two costs).
/// None: numbers from an older window (a session that hasn't asked lately).
pub fn observe(prev: Option<&Value>, pct: f64, reset: f64, t: f64) -> Option<Value> {
    let fresh = json!({"pct": pct, "reset": reset, "at": t, "steps": []});
    let num = |k: &str| prev.and_then(|p| p.get(k)).and_then(Value::as_f64);
    let (Some(last), Some(r)) = (num("pct"), num("reset")) else { return Some(fresh) };
    if !same_reset(&json!(r), &json!(reset)) {
        return (reset > r).then_some(fresh);
    }
    let mut next = prev?.clone();
    if pct > last {
        if t - num("at").unwrap_or(0.) <= STEP_GAP {
            let mut steps: Vec<Value> = next.get("steps").and_then(Value::as_array).cloned().unwrap_or_default();
            steps.push(json!(((pct - last) * 10.).round() / 10.));
            let n = steps.len();
            steps.drain(..n.saturating_sub(8));
            next["steps"] = json!(steps);
        }
        next["pct"] = json!(pct);
    }
    next["at"] = json!(t);
    Some(next)
}

/// What the next request or two may add to a window: twice the usual rise
/// between looks (`observe`), a guess while none's been seen; at least 0.2
/// points (Claude gives its numbers to a tenth of one), at most 2.
pub fn margin(seen: Option<&Value>, k: &str) -> f64 {
    let mut steps: Vec<f64> = seen.and_then(|s| s.get("steps")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_f64).collect();
    steps.sort_by(f64::total_cmp);
    let step = match steps.get(steps.len() / 2) {
        Some(s) => *s,
        None if k == "5h" => 0.5,
        None => 0.1,
    };
    (2. * step).clamp(0.2, 2.)
}

/// Why Claude should move off the active account now, if it should: Claude
/// was refused at a limit (`refused`), or a window (`view`: live over the
/// last asked) is within a request or two (`margin`) of the mark `at`. So an
/// account's used up to its limit and no request is refused for it.
pub fn switch_reason(view: &Value, seen: Option<&Value>, t: f64, at: f64, refused: bool) -> Option<String> {
    if refused {
        return Some("refused at its limit".into());
    }
    for (k, label) in [("5h", "5-hour"), ("wk", "weekly")] {
        let Some((pct, reset)) = window(view.get("usage"), k) else { continue };
        if reset.is_some_and(|r| r <= t) {
            continue;
        }
        if pct + margin(seen.and_then(|s| s.get(k)), k) >= at - 1e-6 {
            return Some(format!("{label} {pct:.1}%"));
        }
    }
    None
}

/// Where auto-switch moves Claude, and why. Off the active account when
/// `switch_reason` says so, to the next (`pick_next`), if any has room. Else
/// to the next when its week ends sooner than the active one's (by more than
/// `SAME_END`): the account whose week runs out soonest is used first, since
/// what's left of a week is lost at its reset. Not while the active account
/// was picked by hand (`held`): that stands until it's full. `seen`: the
/// active account's record in `live.json`.
#[allow(clippy::too_many_arguments)]
pub fn auto_target(names: &[String], active: &str, cache: &Map<String, Value>, d: &Value, seen: Option<&Value>, t: f64, at: f64, refused: bool, held: bool) -> Option<(String, String)> {
    let view = active_view(d, cache, active, t);
    let next = pick_next(names, Some(active), cache, t, at);
    if let Some(why) = switch_reason(&view, seen, t, at, refused) {
        return next.map(|n| (why, n));
    }
    let next = next.filter(|_| !held)?;
    let (mine, theirs) = (week_ends(Some(&view), t), week_ends(cache.get(&next), t));
    if theirs >= mine - SAME_END {
        return None;
    }
    let left = |e: f64| dur(Some(e - t), false);
    let why = if mine.is_finite() { format!("week ends in {}, {next}'s in {}", left(mine), left(theirs)) } else { format!("week not started, {next}'s ends in {}", left(theirs)) };
    Some((why, next))
}

/// Whether the active account (`active`) was picked by hand (`switch`,
/// `next`), as the last move (`activate`'s note) says: auto-switch then
/// leaves it until it's full.
fn held_by_hand(pl: &Places, active: &str) -> bool {
    read_json(&pl.cache.join("switched")).is_some_and(|s| s.get("by").and_then(Value::as_str) == Some("hand") && s.get("to").and_then(Value::as_str) == Some(active))
}

/// Claude's live numbers (`d`) into what's been seen of the active account's
/// windows (`live.json`). Never waits for the lock: the next answer brings more.
fn record_live(pl: &Places, d: &Value, active: &str) {
    let Some(rl) = d.get("rate_limits") else { return };
    if !live_is_ours(d, &load_cache(pl), active) {
        return;
    }
    let Some(_held) = store_lock(pl, false) else { return };
    let p = pl.cache.join("live.json");
    let mut seen = read_json(&p).filter(Value::is_object).unwrap_or_else(|| json!({}));
    if !seen.get(active).is_some_and(Value::is_object) {
        seen[active] = json!({});
    }
    let t = now();
    for (k, out) in [("seven_day", "wk"), ("five_hour", "5h")] {
        let num = |f: &str| rl.get(k).and_then(|w| w.get(f)).and_then(Value::as_f64);
        let (Some(pct), Some(reset)) = (num("used_percentage"), num("resets_at")) else { continue };
        if let Some(v) = observe(seen.get(active).and_then(|s| s.get(out)), pct, reset, t) {
            seen[active][out] = v;
        }
    }
    let _ = write_json(&p, &seen, true);
}

/// Whether Claude's last answer in a transcript (`tail`, its last JSON
/// lines) was a refusal at a usage limit ("You've hit your weekly limit"),
/// given after `since` (the sign-in it has now) and within half an hour of `t`.
pub fn refused_in(tail: &str, since: f64, t: f64) -> bool {
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if v.get("error").and_then(Value::as_str) != Some("rate_limit") || v.get("isApiErrorMessage") != Some(&json!(true)) {
            return false;
        }
        return v.get("timestamp").and_then(Value::as_str).and_then(parse_ts).is_some_and(|at| at > since && t - at < 1800.);
    }
    false
}

/// When Claude's sign-in last changed: its file's time, or the last switch here
/// (macOS keeps it in the keychain).
fn signed_in_at(pl: &Places) -> f64 {
    let at = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok()).map_or(0., |d| d.as_secs_f64());
    at(pl.creds_file()).max(at(pl.cache.join("switched")))
}

/// Whether the session the status line is for (`d`) was just refused at a
/// usage limit on the account it's signed in with now (`refused_in`).
fn was_refused(pl: &Places, d: &Value) -> bool {
    use std::io::{Seek, SeekFrom};
    let Some(p) = d.get("transcript_path").and_then(Value::as_str) else { return false };
    let Ok(mut f) = std::fs::File::open(p) else { return false };
    let len = f.metadata().map_or(0, |m| m.len());
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(len.saturating_sub(256 * 1024))).is_err() || f.read_to_end(&mut buf).is_err() {
        return false;
    }
    refused_in(&String::from_utf8_lossy(&buf), signed_in_at(pl), now())
}

/// The account Claude's leaving keeps Claude's last numbers for it (`d`,
/// newer than the last ask), and that it was refused if it was, so it isn't
/// picked to go back to while it's full. With the store's lock held.
fn note_live(pl: &Places, name: &str, d: &Value, refused: bool) {
    let mut cache = load_cache(pl);
    let Some(was) = cache.get(name) else { return };
    let mut ent = if d.get("rate_limits").is_some() { active_view(d, &cache, name, now()) } else { was.clone() };
    ent["ok"] = was.get("ok").cloned().unwrap_or(Value::Null);
    if refused {
        ent["refused"] = json!(now());
    }
    if &ent != was {
        cache.insert(name.to_string(), ent);
        let _ = write_json(&pl.usage(), &Value::Object(cache), true);
    }
}

/// The last status line's input while it's recent (`LIVE_FRESH`), else nothing.
fn last_live(pl: &Places) -> Value {
    let p = pl.cache.join("last-statusline.json");
    let age = std::fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(|m| SystemTime::now().duration_since(m).ok());
    match age {
        Some(a) if a.as_secs_f64() < LIVE_FRESH => read_json(&p).unwrap_or_else(|| json!({})),
        _ => json!({}),
    }
}

/// Auto-switch, when it's on (`auto_target`): the active account about to
/// pass its mark, or refused at a limit, has Claude moved to the next one;
/// and an account with room whose week ends sooner is moved to. Run by every
/// status line (Claude sends its numbers after each answer, so the move
/// lands before the next request) and after every refresh. `wait`: for the
/// store's lock (the status line's doesn't: the next one tries again).
/// Some((why, new active)): moved.
fn auto_switch(pl: &Places, d: &Value, wait: bool) -> Option<(String, String)> {
    let (on, at) = settings(pl);
    if !on {
        return None;
    }
    let refused = was_refused(pl, d);
    let decide = |active: &str| {
        let names: Vec<String> = accounts(pl).into_keys().collect();
        let seen = read_json(&pl.cache.join("live.json"));
        auto_target(&names, active, &load_cache(pl), d, seen.as_ref().and_then(|s| s.get(active)), now(), at, refused, held_by_hand(pl, active))
    };
    // A first look without the lock: most of the time nothing's due.
    let (Some(c), Some(o)) = live(pl) else { return None };
    decide(&whose(&accounts(pl), &c, &o)?.0)?;
    let _held = store_lock(pl, wait)?;
    let active = sync_live_locked(pl, None, false, wait)?;
    let (why, next) = decide(&active)?;
    match activate_locked(pl, &next, refused, false) {
        Ok(()) => {
            pl.log(&format!("auto-switched {active} ({why}) -> {next}"));
            Some((why, next))
        }
        Err(e) => {
            pl.log(&format!("auto-switch {active} ({why}) -> {next} failed: {e}"));
            None
        }
    }
}

// ------------------------------------------------------------------ drawing

struct Paint(bool);

impl Paint {
    fn c(&self, code: &'static str) -> &'static str {
        if self.0 { code } else { "" }
    }
    fn r(&self) -> &'static str {
        self.c("\x1b[0m")
    }
    fn b(&self) -> &'static str {
        self.c("\x1b[1m")
    }
    fn dim(&self) -> &'static str {
        self.c("\x1b[2m")
    }
    fn red(&self) -> &'static str {
        self.c("\x1b[31m")
    }
    fn grn(&self) -> &'static str {
        self.c("\x1b[32m")
    }
    fn yel(&self) -> &'static str {
        self.c("\x1b[33m")
    }
    fn blu(&self) -> &'static str {
        self.c("\x1b[34m")
    }
    fn mag(&self) -> &'static str {
        self.c("\x1b[35m")
    }
    fn cyn(&self) -> &'static str {
        self.c("\x1b[36m")
    }
    fn org(&self) -> &'static str {
        self.c("\x1b[38;5;208m")
    }
    /// A percentage, green / yellow / red.
    fn pc(&self, p: f64) -> String {
        let col = if p < 70. { self.grn() } else if p < 90. { self.yel() } else { self.red() };
        format!("{col}{}%{}", pct_text(p), self.r())
    }
}

/// "42", or "99.8" in the last point before a limit (Claude's live numbers
/// come to a tenth; rounded, 99.5 would read as a full 100).
pub fn pct_text(p: f64) -> String {
    if p > 99. && p < 100. { format!("{p:.1}") } else { format!("{p:.0}") }
}

/// "3d 4h", "2h 5m", "7m"; `upper`: one unit, "3D".
pub fn dur(sec: Option<f64>, upper: bool) -> String {
    let Some(sec) = sec else { return "?".into() };
    let sec = sec.max(0.) as i64;
    let (d, r) = (sec / 86400, sec % 86400);
    let (h, r) = (r / 3600, r % 3600);
    let m = r / 60;
    if upper {
        return if d > 0 { format!("{d}D") } else if h > 0 { format!("{h}H") } else { format!("{m}M") };
    }
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn git_branch(cwd: &Path) -> Option<String> {
    let mut p = Some(cwd);
    while let Some(dir) = p {
        let head = dir.join(".git").join("HEAD");
        if head.is_file() {
            let r = std::fs::read_to_string(head).ok()?.trim().to_string();
            return Some(match r.strip_prefix("ref:") {
                Some(rf) => rf.trim().splitn(3, '/').last().unwrap_or("").to_string(),
                None => r.chars().take(7).collect(),
            });
        }
        p = dir.parent();
    }
    None
}

/// What's wrong with an account's numbers, if anything.
#[derive(Debug, PartialEq)]
pub enum Problem<'a> {
    None,
    /// Needs you (a sign-in gone, a refusal): shown in red.
    Error(&'a str),
    /// Passing (rate-limited, no network): the last numbers stand meanwhile.
    Missed(&'a str),
}

pub fn problem(ent: &Value) -> Problem<'_> {
    if let Some(e) = ent.get("error").and_then(Value::as_str) {
        return Problem::Error(e);
    }
    if let Some(m) = ent.get("missed").and_then(Value::as_str) {
        return Problem::Missed(m);
    }
    match ent.pointer("/usage/error").and_then(Value::as_str) {
        Some(e) => Problem::Error(e),
        None => Problem::None,
    }
}

/// The active account's entry with Claude Code's own `rate_limits` (its
/// status-line input, as of its last answer) laid over it, which needs no
/// ask of the usage endpoint. A window only fills until it resets, so of
/// two numbers for the same window the higher is the newer. Left alone
/// when the weekly reset says they're another account's.
pub fn with_live(d: &Value, mut ent: Value, t: f64) -> Value {
    let Some(rl) = d.get("rate_limits") else { return ent };
    let live = |k: &str| -> Option<(f64, f64)> {
        let w = rl.get(k)?;
        Some((w.get("used_percentage")?.as_f64()?, w.get("resets_at")?.as_f64()?))
    };
    let Some((_, wk_reset)) = live("seven_day") else { return ent };
    let same_account = match window(ent.get("usage"), "wk") {
        Some((_, Some(r))) => (r - wk_reset).abs() < 600. || (r <= t && wk_reset > t),
        _ => true,
    };
    if !same_account {
        return ent;
    }
    if ent.get("usage").is_none_or(|u| !u.is_object() || u.get("error").is_some()) {
        ent["usage"] = json!({});
    }
    for (k, out) in [("seven_day", "wk"), ("five_hour", "5h")] {
        let Some((pct, reset)) = live(k) else { continue };
        let pct = match window(ent.get("usage"), out) {
            Some((c, Some(r))) if (r - reset).abs() < 600. => pct.max(c),
            Some((_, Some(r))) if r > reset => continue,
            _ => pct,
        };
        ent["usage"][out] = json!({"pct": pct, "reset": reset});
    }
    ent["ok"] = json!(t);
    ent
}

/// The accounts other than `active` in the status line's order, the order
/// Claude would go to them in: those with room first, in `use_order` (the
/// week that ends soonest first: `pick_next`'s is first); then those
/// without (at the mark `at`, refused at a limit), the one that clears
/// soonest first; then by name; those with no numbers, or a sign-in gone, last.
pub fn others_order<'a>(names: &'a [String], active: &str, cache: &Map<String, Value>, t: f64, at: f64) -> Vec<&'a String> {
    let group = |n: &String| -> u8 {
        let Some(ent) = cache.get(n) else { return 2 };
        if matches!(problem(ent), Problem::Error(_)) || ent.get("usage").is_none_or(|u| u.get("error").is_some()) {
            2
        } else if room(Some(ent), t, at) > 2. {
            0
        } else {
            1
        }
    };
    let clears = |n: &String| worst(cache.get(n).and_then(|e| e.get("usage")), t).1.unwrap_or(f64::INFINITY);
    let mut v: Vec<&String> = names.iter().filter(|n| n.as_str() != active).collect();
    v.sort_by(|a, b| {
        group(a).cmp(&group(b)).then_with(|| match group(a) {
            0 => use_order(cache, t, at)(a, b),
            1 => clears(a).total_cmp(&clears(b)).then(a.cmp(b)),
            _ => a.cmp(b),
        })
    });
    v
}

/// Claude Code's status line: the accounts (the active one's use and pace,
/// the next one's name, the others' in `others_order`), then the session
/// (model and effort, context, folder and branch, lines and cost). `d` is
/// what Claude sends; `at` the auto-switch mark (what counts as full).
#[allow(clippy::too_many_arguments)]
pub fn statusline(d: &Value, cache: &Map<String, Value>, active: Option<&str>, names: &[String], home: &Path, t: f64, at: f64, color: bool) -> String {
    let p = Paint(color);
    let sep = format!(" {}|{} ", p.dim(), p.r());
    let mut l1 = Vec::new();
    match active {
        Some(active) => {
            let empty = json!({});
            let ent = &active_view(d, cache, active, t);
            let u = ent.get("usage").unwrap_or(&empty);
            let mut part = format!("{}{}◆ {active}{}", p.grn(), p.b(), p.r());
            if let Some((pct, reset)) = window(Some(u), "wk") {
                part += &format!(" wk {}", p.pc(pct));
                if let Some(r) = reset {
                    part += &format!(" {}⧗{}{}", p.dim(), dur(Some(r - t), false), p.r());
                }
            }
            if let Some((pct, reset)) = window(Some(u), "5h").filter(|(pct, _)| *pct > 0.) {
                part += &format!(" 5h {} {}⧗{}{}", p.pc(pct), p.dim(), dur(reset.map(|r| r - t), false), p.r());
            }
            let wk = window(Some(u), "wk").map_or(0., |w| w.0);
            for sc in u.get("scoped").and_then(Value::as_array).into_iter().flatten() {
                let pct = sc.get("pct").and_then(Value::as_f64).unwrap_or(0.);
                if sc.get("active").and_then(Value::as_bool) == Some(true) && pct > wk {
                    part += &format!(" {} {}", sc.get("model").and_then(Value::as_str).unwrap_or("model"), p.pc(pct));
                }
            }
            if let Some(eta) = projection(ent, t) {
                part += &format!(" {}▲ limit in {}{}", p.org(), dur(Some(eta), false), p.r());
            }
            match problem(ent) {
                Problem::Error(e) => part += &format!(" {}!{e}{}", p.red(), p.r()),
                // A miss is waited out quietly: said only with nothing to show yet, or once it's old.
                Problem::Missed(_) if window(Some(u), "wk").is_none() => part += &format!(" {}usage ⟳{}", p.dim(), p.r()),
                Problem::Missed(_) if t - got_at(ent) > STALE_AFTER => part += &format!(" {}({} old){}", p.dim(), dur(Some(t - got_at(ent)), false), p.r()),
                _ => {}
            }
            l1.push(part);
            l1.push(match pick_next(names, Some(active), cache, t, at) {
                Some(n) => format!("next {}{n}{}", p.cyn(), p.r()),
                None => format!("next {}none{}", p.dim(), p.r()),
            });
            let others: Vec<String> = others_order(names, active, cache, t, at)
                .into_iter()
                .map(|n| {
                    let u = cache.get(n).and_then(|e| e.get("usage"));
                    match u {
                        Some(u) if u.get("error").is_none() => {
                            let (pct, rs) = worst(Some(u), t);
                            format!("{}{n}{} {} {}({}){}", p.mag(), p.r(), p.pc(pct), p.dim(), rs.map_or("—".into(), |r| dur(Some(r - t), true)), p.r())
                        }
                        _ => format!("{}{n} ?{}", p.dim(), p.r()),
                    }
                })
                .collect();
            if !others.is_empty() {
                l1.push(others.join(&format!(" {}·{} ", p.dim(), p.r())));
            }
        }
        None => l1.push(format!("{}no Claude account signed in{}", p.dim(), p.r())),
    }
    let m = d.get("model").cloned().unwrap_or_else(|| json!({}));
    let model = m.get("display_name").or_else(|| m.get("id")).and_then(Value::as_str).unwrap_or("?");
    let eff = d.get("effort").or_else(|| d.get("effort_level")).or_else(|| m.get("effort"));
    let eff = match eff {
        Some(Value::Object(o)) => o.get("level").or_else(|| o.get("name")).and_then(Value::as_str).map(String::from),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };
    let mut l2 = vec![match eff {
        Some(e) => format!("{model} {e}"),
        None => model.to_string(),
    }];
    if let Some(ctx) = d.pointer("/context_window/used_percentage").and_then(Value::as_f64) {
        l2.push(format!("ctx {}", p.pc(ctx)));
    }
    let cwd = d.pointer("/workspace/current_dir").or_else(|| d.get("cwd")).and_then(Value::as_str).map(PathBuf::from).or_else(|| std::env::current_dir().ok()).unwrap_or_default();
    let shown = {
        let s = cwd.to_string_lossy().into_owned();
        let h = home.to_string_lossy();
        match s.strip_prefix(h.as_ref()) {
            Some(rest) if !h.is_empty() => format!("~{rest}"),
            _ => s,
        }
    };
    let mut loc = format!("{}{shown}{}", p.blu(), p.r());
    if let Some(br) = git_branch(&cwd) {
        loc += &format!(" {}⎇ {br}{}", p.mag(), p.r());
    }
    l2.push(loc);
    let co = d.get("cost").cloned().unwrap_or_else(|| json!({}));
    let mut tail = Vec::new();
    let (add, rem) = (co.get("total_lines_added").and_then(Value::as_i64).unwrap_or(0), co.get("total_lines_removed").and_then(Value::as_i64).unwrap_or(0));
    if add != 0 || rem != 0 {
        tail.push(format!("{}+{add}{} {}-{rem}{}", p.grn(), p.r(), p.red(), p.r()));
    }
    if let Some(ms) = co.get("total_duration_ms").and_then(Value::as_f64).filter(|ms| *ms > 0.) {
        tail.push(dur(Some(ms / 1000.), false));
    }
    if let Some(usd) = co.get("total_cost_usd").and_then(Value::as_f64) {
        tail.push(format!("${usd:.2}"));
    }
    if !tail.is_empty() {
        l2.push(tail.join(" "));
    }
    format!("{}\n{}", l1.join(&sep), l2.join(&sep))
}

fn cmd_statusline(pl: &Places) -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let d: Value = serde_json::from_str(&input).unwrap_or_else(|_| json!({}));
    let _ = write_json(&pl.cache.join("last-statusline.json"), &d, true);
    // Claude sends this after every answer, its numbers as of that answer:
    // near the mark (or refused at it), the next request goes out on another account.
    if let (Some(c), Some(o)) = live(pl) {
        if let Some((active, _)) = whose(&accounts(pl), &c, &o) {
            record_live(pl, &d, &active);
        }
    }
    auto_switch(pl, &d, false);
    let (cache, active) = refresh_bg(pl);
    let names: Vec<String> = accounts(pl).into_keys().collect();
    println!("{}", statusline(&d, &cache, active.as_deref(), &names, &pl.home, now(), settings(pl).1, true));
    0
}

/// Colors for a terminal; none when the output goes elsewhere (/accounts
/// reads it through a pipe) or NO_COLOR says so.
fn color_wanted() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

fn cmd_list(pl: &Places, fresh: bool) -> i32 {
    let cache = refresh_all(pl, fresh);
    let active = sync_live(pl, None, false);
    let accs = accounts(pl);
    let p = Paint(color_wanted());
    if accs.is_empty() {
        println!("No accounts saved yet. Sign in to Claude (/login), then run: claude-accounts add");
        return 0;
    }
    let names: Vec<String> = accs.keys().cloned().collect();
    let t = now();
    let (on, at) = settings(pl);
    let next = pick_next(&names, active.as_deref(), &cache, t, at);
    println!("  {:<18}{:<30}{:<8}{:>16}{:>18}  model caps", "account", "email", "plan", "5-hour", "weekly");
    for (n, a) in &accs {
        let empty = json!({});
        let ent = cache.get(n).unwrap_or(&empty);
        let u = ent.get("usage").unwrap_or(&empty);
        let mark = if Some(n.as_str()) == active.as_deref() {
            format!("{}▶{} ", p.grn(), p.r())
        } else if Some(n) == next.as_ref() {
            format!("{}»{} ", p.cyn(), p.r())
        } else {
            "  ".into()
        };
        let cell = |k: &str, width: usize| match window(Some(u), k) {
            None => format!("{}{:>width$}{}", p.dim(), "—", p.r()),
            Some((pct, reset)) => {
                let txt = format!("{}% ⧗{}", pct_text(pct), reset.map_or("?".into(), |r| dur(Some(r - t), false)));
                let col = if pct < 70. { p.grn() } else if pct < 90. { p.yel() } else { p.red() };
                format!("{col}{txt:>width$}{}", p.r())
            }
        };
        let caps: Vec<String> = u.get("scoped").and_then(Value::as_array).into_iter().flatten().map(|s| format!("{} {:.0}%", s.get("model").and_then(Value::as_str).unwrap_or("model"), s.get("pct").and_then(Value::as_f64).unwrap_or(0.))).collect();
        let plan = a.pointer("/oauthAccount/planDisplayName").or_else(|| a.pointer("/credentials/subscriptionType")).and_then(Value::as_str).unwrap_or("?");
        let email: String = email_of(a).chars().take(29).collect();
        let plan: String = plan.chars().take(7).collect();
        let err = match problem(ent) {
            Problem::Error(e) => format!("  {}{e}{}", p.red(), p.r()),
            Problem::Missed(m) => {
                let again = ent.get("retry_at").and_then(Value::as_f64).map_or(String::new(), |r| format!(", asking again in {}", dur(Some(r - t), false)));
                let old = if window(Some(u), "wk").is_some() { format!("numbers {} old, ", dur(Some(t - got_at(ent)), false)) } else { String::new() };
                format!("  {}({old}{m}{again}){}", p.dim(), p.r())
            }
            Problem::None => String::new(),
        };
        println!("{mark}{n:<18}{email:<30}{plan:<8}{}{}  {}{err}", cell("5h", 16), cell("wk", 18), caps.join(", "));
        if Some(n.as_str()) == active.as_deref() {
            if let Some(eta) = projection(ent, t) {
                println!("{:20}{}at this pace the weekly limit hits in {}{}", "", p.org(), dur(Some(eta), false), p.r());
            }
        }
    }
    println!("\n▶ active   » next (the soonest-ending week with room)   auto-switch: {}", if on { format!("on at {at:.0}%, soonest-ending week first") } else { "off".into() });
    0
}

// ------------------------------------------------------------------ hooks

/// A session a limit stopped is woken (`hook stop-failure`) at most this
/// many times in `WAKE_WINDOW`, so a full house can't loop.
const WAKES: usize = 2;
const WAKE_WINDOW: f64 = 900.;
/// A move this recent (another session's failure, the status line's
/// auto-switch) is what a session that just stopped on its limit retries on.
const JUST_MOVED: f64 = 120.;

/// What a session that stopped on a usage limit does next (`hook stop-failure`).
#[derive(Debug, PartialEq)]
pub enum Retry {
    /// Claude was just moved to this account, which has room: try again on it.
    Again(String),
    /// The account it stopped on is full: move to this one and try again.
    Move(String),
    /// No account has room: stay stopped.
    Stay,
}

/// `Retry` for a session that stopped on `active`'s limit at `t`, Claude
/// having been moved last at `moved_at` (`activate`'s note). A first stop
/// right after a move onto an account with room retries there; else the
/// account it's on is full, and the next one (`pick_next`)
/// takes over.
pub fn retry_plan(names: &[String], active: &str, cache: &Map<String, Value>, moved_at: f64, woken_before: bool, t: f64, at: f64) -> Retry {
    let full = |e: &Value| e.get("refused").and_then(Value::as_f64).is_some_and(|r| r >= fetched(Some(e))) || worst(e.get("usage"), t).0 >= at;
    if !woken_before && t - moved_at < JUST_MOVED && !cache.get(active).is_some_and(full) {
        return Retry::Again(active.to_string());
    }
    match pick_next(names, Some(active), cache, t, at) {
        Some(n) => Retry::Move(n),
        None => Retry::Stay,
    }
}

/// The times a session was woken that still count (`WAKE_WINDOW`).
pub fn recent_wakes(v: Option<&Value>, t: f64) -> Vec<f64> {
    v.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_f64).filter(|w| t - w < WAKE_WINDOW).collect()
}

/// Claude Code's hooks: the same settings on every PC, whichever tool
/// `claude-accounts` is there (`hook session-start|prompt|stop|subagent-stop`,
/// and `stop-failure`). `prompt` moves Claude off an account at its mark
/// before the prompt goes out, and says so, and names the session
/// (`claude_title`: what it's working on, its terminal's title);
/// `session-start` gives a resumed one its title back; `stop-failure` (StopFailure, run
/// with asyncRewake) moves Claude off the account a turn stopped on and
/// wakes the session to carry on (exit 2, the note on stderr). Everything
/// else has nothing to do here. Never fails a session.
fn cmd_hook(pl: &Places, event: &str) -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let h: Value = serde_json::from_str(&input).unwrap_or_else(|_| json!({}));
    let titles = pl.cache.join("titles");
    match event {
        "prompt" => {
            let note = hook_prompt(pl, &h);
            let title = crate::title::on_prompt(&titles, &h);
            match (title, note) {
                (Some(t), note) => {
                    // Claude's schema takes a missing note, not a null one.
                    let mut out = json!({"hookEventName": "UserPromptSubmit", "sessionTitle": t});
                    if let Some(n) = note {
                        out["additionalContext"] = json!(n);
                    }
                    println!("{}", json!({ "hookSpecificOutput": out }));
                }
                (None, Some(note)) => println!("{note}"),
                (None, None) => {}
            }
            0
        }
        "session-start" => {
            if let Some(t) = crate::title::on_start(&titles, &h) {
                println!("{}", json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "sessionTitle": t}}));
            }
            0
        }
        "stop-failure" => match hook_stop_failure(pl, &h) {
            Some(note) => {
                eprintln!("{note}");
                2
            }
            None => 0,
        },
        _ => 0,
    }
}

/// What the hook knows of Claude's live numbers: the last status line's
/// (any session's: they share the sign-in), with this session's transcript
/// for a refusal in it (`was_refused`).
fn hook_view(pl: &Places, h: &Value) -> Value {
    let mut d = last_live(pl);
    if !d.is_object() {
        d = json!({});
    }
    if let Some(tp) = h.get("transcript_path") {
        d["transcript_path"] = tp.clone();
    }
    d
}

fn hook_prompt(pl: &Places, h: &Value) -> Option<String> {
    let (Some(c), Some(o)) = live(pl) else { return None };
    let before = whose(&accounts(pl), &c, &o)?.0;
    let (why, next) = auto_switch(pl, &hook_view(pl, h), true)?;
    Some(format!("claude-accounts: Claude was moved from {before} ({why}) to {next} before this prompt went out."))
}

/// How long a session that stopped with every account full waits for one to
/// have room again (a reset; more usage bought) before it's left stopped: a
/// week's limit resets within seven days. The stop-failure hook's timeout in
/// Claude's settings covers it (`STOP_HOOK_TIMEOUT`); Claude ends the wait
/// itself when the session closes.
const WAIT_FOR_ROOM: f64 = 8. * 86400.;
/// The hook's timeout (seconds): the wait and an hour.
pub const STOP_HOOK_TIMEOUT: u64 = 8 * 86400 + 3600;
/// How often the wait looks, at most (usage is asked again only when it's due:
/// `refresh_all`); it looks again just after the soonest reset it knows of.
const WAIT_LOOK: f64 = 600.;

/// Whether a session's transcript (`tail`, its last lines) still ends at the
/// usage-limit refusal that stopped it: its last turn is that refusal. Anything
/// after it (someone typed, it was woken another way, a script's carry-on)
/// means it went on without this wait.
pub fn still_stopped(tail: &str) -> bool {
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => return v.get("isApiErrorMessage") == Some(&json!(true)) && matches!(v.get("error").and_then(Value::as_str), Some("rate_limit" | "billing_error")),
            Some("user") => return false,
            _ => {}
        }
    }
    false
}

fn transcript_tail(p: &Path) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(p).ok()?;
    let len = f.metadata().map_or(0, |m| m.len());
    let mut buf = Vec::new();
    f.seek(SeekFrom::Start(len.saturating_sub(256 * 1024))).ok()?;
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// A session stopped with every account full (`hook stop-failure`): wait until
/// one has room again (`room_now`), move Claude to it, and say which and how
/// long it took; the hook then wakes the session (asyncRewake). None: the
/// session went on meanwhile (`still_stopped`), another wait is already on for
/// it, auto-switch was turned off, or `WAIT_FOR_ROOM` passed.
fn wait_for_room(pl: &Places, h: &Value, session: &str, since: f64) -> Option<(String, f64)> {
    // One wait per session: a person retrying into the same full house starts another hook.
    let dir = pl.cache.join("waiting");
    std::fs::create_dir_all(&dir).ok()?;
    let mine = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(format!("{session}.lock"))).ok()?;
    mine.try_lock().ok()?;
    let transcript = h.get("transcript_path").and_then(Value::as_str).map(PathBuf::from);
    loop {
        let t = now();
        if t - since > WAIT_FOR_ROOM {
            pl.log(&format!("{}: no account had room in {}; it stays stopped", &session[..session.len().min(8)], dur(Some(WAIT_FOR_ROOM), false)));
            return None;
        }
        let (on, at) = settings(pl);
        if !on {
            return None;
        }
        let names: Vec<String> = accounts(pl).into_keys().collect();
        let clears = room_now(&names, None, &load_cache(pl), t, at).err().flatten();
        let nap = clears.map_or(WAIT_LOOK, |c| (c - t + 30.).clamp(60., WAIT_LOOK));
        std::thread::sleep(Duration::from_secs_f64(nap));
        if transcript.as_deref().and_then(transcript_tail).is_some_and(|tail| !still_stopped(&tail)) {
            pl.log(&format!("{}: went on while it waited for an account with room", &session[..session.len().min(8)]));
            return None;
        }
        let cache = refresh_all(pl, false);
        let active = sync_live(pl, None, false);
        if let Ok(n) = room_now(&names, active.as_deref(), &cache, now(), at) {
            if active.as_deref() != Some(n.as_str()) {
                let _held = store_lock(pl, true);
                activate_locked(pl, &n, false, false).ok()?;
            }
            return Some((n, now() - since));
        }
    }
}

fn hook_stop_failure(pl: &Places, h: &Value) -> Option<String> {
    let error = h.get("error").and_then(Value::as_str).unwrap_or("?");
    // Another account only helps with a limit.
    if !matches!(error, "rate_limit" | "billing_error") {
        return None;
    }
    let session = h.get("session_id").and_then(Value::as_str).filter(|s| valid_name(s)).unwrap_or("unknown").to_string();
    let short: String = session.chars().take(8).collect();
    let wakes_file = pl.cache.join("wakes.json");
    let woken = recent_wakes(read_json(&wakes_file).as_ref().and_then(|w| w.get(&session)), now());
    let (on, at) = settings(pl);
    if !on {
        pl.log(&format!("{short} stopped on a limit ({error}): auto-switch is off, so it stays stopped"));
        return None;
    }
    // One at a time: when an account runs out, every open session stops together.
    let held = store_lock(pl, true)?;
    let active = sync_live_locked(pl, None, false, true)?;
    let names: Vec<String> = accounts(pl).into_keys().collect();
    let moved_at = read_json(&pl.cache.join("switched")).and_then(|s| s.get("at").and_then(Value::as_f64)).unwrap_or(0.);
    let t = now();
    // Woken twice already lately and stopped again: no account really has room,
    // whatever their numbers said; it waits for one like any full house.
    let plan = if woken.len() >= WAKES { Retry::Stay } else { retry_plan(&names, &active, &load_cache(pl), moved_at, !woken.is_empty(), t, at) };
    let how = match plan {
        Retry::Again(n) => format!("and Claude had just been moved to {n}"),
        Retry::Move(next) => match activate_locked(pl, &next, true, false) {
            Ok(()) => {
                pl.log(&format!("{short} stopped on {active}'s limit ({error}) -> {next}"));
                format!("so Claude was moved from {active} to {next}")
            }
            Err(e) => {
                pl.log(&format!("{short} stopped on {active}'s limit ({error}); moving to {next} failed: {e}"));
                return None;
            }
        },
        Retry::Stay => {
            // Full, whatever its numbers said: auto-switch won't pick it meanwhile.
            note_live(pl, &active, &last_live(pl), true);
            pl.log(&format!("{short} stopped on {active}'s limit ({error}): no account has room; it waits for one (a reset, more usage) and carries on then"));
            drop(held);
            let (n, waited) = wait_for_room(pl, h, &session, t)?;
            pl.log(&format!("{short}: {n} has room again after {}: woken", dur(Some(waited), false)));
            format!("then waited {} until {n} had room again, and Claude is on it now", dur(Some(waited), false))
        }
    };
    let mut wakes = read_json(&wakes_file).filter(Value::is_object).unwrap_or_else(|| json!({}));
    if let Some(m) = wakes.as_object_mut() {
        m.retain(|_, v| !recent_wakes(Some(v), t).is_empty());
    }
    let mut mine = woken;
    mine.push(now());
    wakes[session.as_str()] = json!(mine);
    let _ = write_json(&wakes_file, &wakes, true);
    Some(format!("claude-accounts: the last request stopped on a usage limit ({error}), {how}. Carry on with the task from where it stopped; don't redo work that's already done."))
}

// ------------------------------------------------------------------ Claude's settings

/// Whether one of `names` has room at `t` under the mark `at`: the active one
/// when it has (`room`), else the next (`pick_next`); Err: none has, and when
/// the first one's limit clears (None: not known).
pub fn room_now(names: &[String], active: Option<&str>, cache: &Map<String, Value>, t: f64, at: f64) -> Result<String, Option<f64>> {
    let active = active.filter(|a| names.iter().any(|n| n == a));
    if let Some(a) = active.filter(|a| room(cache.get(*a), t, at) > 0.) {
        return Ok(a.to_string());
    }
    if let Some(n) = pick_next(names, active, cache, t, at) {
        return Ok(n);
    }
    Err(names.iter().filter_map(|n| worst(cache.get(n).and_then(|e| e.get("usage")), t).1).min_by(f64::total_cmp))
}

/// The hooks this tool gives Claude, `prog` being how Claude runs it
/// (`program_for_claude`).
fn accounts_hooks(prog: &str) -> [(&'static str, &'static str, Value); 3] {
    [
        ("SessionStart", "hook session-start", json!({"hooks": [{"type": "command", "command": format!("{prog} hook session-start"), "timeout": 30}]})),
        ("UserPromptSubmit", "hook prompt", json!({"hooks": [{"type": "command", "command": format!("{prog} hook prompt"), "timeout": 60}]})),
        ("StopFailure", "hook stop-failure", json!({"matcher": "rate_limit|billing_error", "hooks": [{"type": "command", "command": format!("{prog} hook stop-failure"), "asyncRewake": true, "timeout": STOP_HOOK_TIMEOUT}]})),
    ]
}

/// How Claude's settings run this program (`exe`, its own path): plain
/// `claude-accounts` when that's what PATH (`path`) finds, else its full path
/// with `/` (Git Bash, which runs Claude's hooks on Windows, reads those too),
/// in quotes only when it has a space. None: a path no shell can be handed
/// safely (a quote, `$`, a backtick and the like in it).
pub fn program_for_claude(exe: &Path, path: Option<&std::ffi::OsStr>) -> Option<String> {
    let name = exe.file_name()?;
    let on_path = path.is_some_and(|p| std::env::split_paths(p).any(|d| d.join(name) == exe));
    let stem = exe.file_stem()?.to_string_lossy();
    if on_path && stem == "claude-accounts" {
        return Some("claude-accounts".into());
    }
    let full = exe.to_string_lossy().replace('\\', "/");
    if full.chars().any(|c| matches!(c, '"' | '\'' | '`' | '$' | '%' | '!' | '&' | '|' | ';' | '<' | '>' | '(' | ')' | '^') || c.is_control()) {
        return None;
    }
    Some(if full.contains(' ') { format!("\"{full}\"") } else { full })
}

/// The stop-failure hook in `list` (an event's entries) with a timeout under
/// `STOP_HOOK_TIMEOUT` (60 s, from before it waited for a reset) given it:
/// Claude ends an async hook at its timeout, which would end the wait. Whether
/// one was raised.
fn raise_stop_timeout(list: &mut [Value]) -> bool {
    let mut raised = false;
    for e in list.iter_mut() {
        for h in e.get_mut("hooks").and_then(Value::as_array_mut).into_iter().flatten() {
            let ours = h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains("claude-accounts") && c.contains("hook stop-failure"));
            if ours && h.get("timeout").and_then(Value::as_u64).unwrap_or(0) < STOP_HOOK_TIMEOUT {
                h["timeout"] = json!(STOP_HOOK_TIMEOUT);
                raised = true;
            }
        }
    }
    raised
}

/// Claude's settings (`s`) with this tool's status line (where there's none:
/// one of yours is left) and its hooks (where that event has no
/// `claude-accounts` hook of the kind), `prog` being how Claude runs it; or
/// None: nothing to add, or settings that aren't an object (yours to fix).
pub fn with_accounts_settings(mut s: Value, prog: &str) -> Option<Value> {
    let o = s.as_object_mut()?;
    let mut changed = false;
    if o.get("statusLine").is_none_or(Value::is_null) {
        o.insert("statusLine".into(), json!({"type": "command", "command": format!("{prog} statusline"), "padding": 0, "refreshInterval": 30}));
        changed = true;
    }
    if !o.get("hooks").is_some_and(Value::is_object) {
        if o.get("hooks").is_some_and(|h| !h.is_null()) {
            return changed.then_some(s);
        }
        o.insert("hooks".into(), json!({}));
    }
    let hooks = o.get_mut("hooks").and_then(Value::as_object_mut)?;
    for (event, what, entry) in accounts_hooks(prog) {
        let list = hooks.entry(event).or_insert_with(|| json!([]));
        let Some(list) = list.as_array_mut() else { continue };
        let has = list.iter().any(|e| e.get("hooks").and_then(Value::as_array).into_iter().flatten().any(|h| h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains("claude-accounts") && c.contains(what))));
        if !has {
            list.push(entry);
            changed = true;
        } else if event == "StopFailure" && raise_stop_timeout(list) {
            changed = true;
        }
    }
    changed.then_some(s)
}

/// `with_accounts_settings` on Claude's settings.json here (a copy of the
/// one it changes kept beside it once, `settings.json.before-claude-accounts`).
/// Whether it changed.
fn ensure_claude_settings(pl: &Places, prog: &str) -> Result<bool, String> {
    let p = pl.claude.join("settings.json");
    let s = match read_json(&p) {
        Some(v) => v,
        None if !p.exists() => json!({}),
        None => return Err(format!("{} doesn't read as JSON; left as it is", p.display())),
    };
    match with_accounts_settings(s, prog) {
        Some(v) => {
            let before = pl.claude.join("settings.json.before-claude-accounts");
            if p.exists() && !before.exists() {
                std::fs::copy(&p, &before).map_err(|e| format!("{}: {e}", before.display()))?;
            }
            write_json(&p, &v, true).map(|()| true)
        }
        None => Ok(false),
    }
}

/// Claude's `/accounts` command (`commands/accounts.md`), `prog` being how
/// Claude runs this program.
pub fn accounts_command(prog: &str) -> String {
    format!(
        "---\n\
description: Claude accounts: usage on each, switch (switch <name> | next | auto on/off/<pct> | add [name] | rename | remove)\n\
argument-hint: \"[list | switch <name> | next | auto on|off|<pct> | add [name] | rename <old> <new> | remove <name>]\"\n\
allowed-tools: Bash(claude-accounts:*)\n\
---\n\
Output of `claude-accounts $ARGUMENTS`:\n\
\n\
!`{prog} $ARGUMENTS 2>&1`\n\
\n\
Show the user that output as-is (in a code block). If they asked to switch, tell them every Claude session, this one included, uses the new account from its next request (Claude Code reads its sign-in again when it changes; no restart needed). Auto-switch is on by default: it uses the account whose week ends soonest first (what's left of a week is lost at its reset), each up to 100% (or the mark `auto <pct>` set), moving to the next a request or two before the limit by Claude's live numbers, or at once if Claude was refused at a limit; an account they switch to by hand is kept until it's full. If they are stuck at a limit anyway, `/accounts next` switches by hand (it runs locally, so it works even while refused). If an account says its sign-in expired, tell them to /login with that account in Claude (no /logout first) and it gets picked up automatically. Don't run anything else unless they ask.\n"
    )
}

/// `install`: Claude's status line, the hooks and `/accounts` set up to run
/// this program, and the account Claude is signed in with kept.
fn cmd_install(pl: &Places) -> i32 {
    let exe = match std::env::current_exe().and_then(|e| e.canonicalize().or(Ok(e))) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("claude-accounts: where am I? {e}");
            return 1;
        }
    };
    let Some(prog) = program_for_claude(&crate::util::plain_path(&exe), std::env::var_os("PATH").as_deref()) else {
        eprintln!("claude-accounts: {} has characters a shell would read; put it somewhere plainer (or on PATH) and run install again", exe.display());
        return 1;
    };
    match ensure_claude_settings(pl, &prog) {
        Ok(true) => println!("Claude's settings ({}): the status line and hooks run {prog}", pl.claude.join("settings.json").display()),
        Ok(false) => println!("Claude's settings ({}): already set (a status line of your own is left as it is)", pl.claude.join("settings.json").display()),
        Err(e) => {
            eprintln!("claude-accounts: {e}");
            return 1;
        }
    }
    let cmd_file = pl.claude.join("commands").join("accounts.md");
    let text = accounts_command(&prog);
    match std::fs::read_to_string(&cmd_file) {
        Ok(t) if t == text => {}
        Ok(_) => println!("/accounts: {} is there already and left as it is", cmd_file.display()),
        Err(_) => {
            let wrote = cmd_file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&cmd_file, &text));
            match wrote {
                Ok(()) => println!("/accounts: {}", cmd_file.display()),
                Err(e) => eprintln!("claude-accounts: {}: {e}", cmd_file.display()),
            }
        }
    }
    match sync_live(pl, None, true) {
        Some(n) => println!("Claude is signed in as {n}: kept. Sign in with each other account (/login in Claude, no /logout first) and each is kept too."),
        None => println!("Claude isn't signed in yet: /login in Claude, and the account is kept."),
    }
    0
}

const HELP: &str = "\
claude-accounts: several Claude subscriptions in Claude Code, switched by themselves

  list [--fresh]          usage on every account (the default)
  add [name]              keep the account Claude is signed in with now
  switch <name>           make <name> the one Claude uses (every session, from its next request)
  next                    switch to the next: the soonest-ending week with room
  auto [on|off|<pct>]     auto-switch (on at 100% unless told otherwise)
  rename <old> <new>      rename a kept account
  remove <name>           forget a kept account (not the active one)
  refresh [--force]       ask every account's usage again where it's due
  install                 set Claude's status line, hooks and /accounts to run this program
  statusline              Claude Code's statusLine command (JSON on stdin)
  hook <event>            Claude Code's hooks: session-start, prompt, stop-failure
  version
";

/// `claude-accounts …`.
pub fn command_line(args: &[String]) -> i32 {
    let Some(pl) = places() else {
        eprintln!("claude-accounts: no home folder");
        return 1;
    };
    crate::util::private_dir(&pl.conf);
    crate::util::private_dir(&pl.cache);
    let cmd = args.first().map_or("list", String::as_str);
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    let fail = |e: &str| {
        eprintln!("{e}");
        1
    };
    match cmd {
        "statusline" => cmd_statusline(&pl),
        "hook" => cmd_hook(&pl, rest.first().map_or("", String::as_str)),
        "list" | "ls" | "usage" | "status" => cmd_list(&pl, rest.iter().any(|a| a == "--fresh" || a == "-f")),
        "refresh" => {
            refresh_all(&pl, rest.iter().any(|a| a == "--force"));
            0
        }
        "install" | "setup" => cmd_install(&pl),
        "add" => {
            if live(&pl).0.is_none() {
                return fail("Claude isn't signed in. Run /login in Claude first.");
            }
            if let Some(n) = rest.first().filter(|n| !valid_name(n)) {
                return fail(&format!("'{n}' can't be a name: letters, digits, . _ - only"));
            }
            if let Some(h) = rest.first() {
                let active = sync_live(&pl, None, false);
                if active.as_deref() != Some(h.as_str()) && accounts(&pl).contains_key(h) {
                    return fail(&format!("'{h}' is another account already: pick another name (or claude-accounts remove {h} first)"));
                }
            }
            match sync_live(&pl, rest.first().map(String::as_str), true) {
                Some(n) => {
                    println!("saved: {n}");
                    0
                }
                None => fail("Claude isn't signed in. Run /login in Claude first."),
            }
        }
        "switch" | "use" => {
            let Some(target) = rest.first() else { return fail("usage: claude-accounts switch <name>") };
            let accs = accounts(&pl);
            let mut target = target.clone();
            if !accs.contains_key(&target) {
                let t = target.to_lowercase();
                let m: Vec<&String> = accs.iter().filter(|(n, a)| n.to_lowercase().starts_with(&t) || email_of(a).to_lowercase().starts_with(&t)).map(|(n, _)| n).collect();
                if m.len() == 1 {
                    target = m[0].clone();
                }
            }
            match activate(&pl, &target) {
                Ok(()) => {
                    println!("switched to {target}. Every Claude session uses it from its next request.");
                    0
                }
                Err(e) => fail(&e),
            }
        }
        "next" => {
            let active = sync_live(&pl, None, false);
            let cache = refresh_all(&pl, false);
            let names: Vec<String> = accounts(&pl).into_keys().collect();
            let Some(next) = pick_next(&names, active.as_deref(), &cache, now(), settings(&pl).1) else { return fail("no other account has headroom") };
            match activate(&pl, &next) {
                Ok(()) => {
                    println!("switched {} → {next}", active.unwrap_or_default());
                    0
                }
                Err(e) => fail(&e),
            }
        }
        "auto" => {
            let mut s = read_json(&pl.settings()).filter(Value::is_object).unwrap_or_else(|| json!({}));
            match rest.first().map(String::as_str) {
                Some("on") => s["auto_switch"] = json!(true),
                Some("off") => s["auto_switch"] = json!(false),
                Some(v) => match v.trim_end_matches('%').parse::<u32>() {
                    Ok(pct @ 1..=100) => {
                        s["auto_switch"] = json!(true);
                        s["auto_switch_at"] = json!(pct);
                    }
                    _ => return fail("usage: claude-accounts auto [on|off|<pct> (1-100: the use it switches at)]"),
                },
                None => {}
            }
            if let Err(e) = write_json(&pl.settings(), &s, false) {
                return fail(&e);
            }
            let (on, at) = settings(&pl);
            println!("auto-switch {}", if on { format!("on: Claude uses the account whose week ends soonest while it has room, and moves on a request or two before the active one passes {at:.0}% (or at once if it's refused at a limit); one you switch to by hand is kept until it's full") } else { "off".into() });
            0
        }
        "rename" | "mv" => {
            let [old, new] = rest else { return fail("usage: claude-accounts rename <old> <new>") };
            let _held = store_lock(&pl, true);
            if !valid_name(new) || !accounts(&pl).contains_key(old) {
                return fail("usage: claude-accounts rename <old> <new> (an account you have; letters, digits, . _ -)");
            }
            if accounts(&pl).contains_key(new) || pl.accounts_dir().join(format!("{new}.json")).exists() {
                return fail(&format!("'{new}' is taken: pick another name"));
            }
            if let Err(e) = std::fs::rename(pl.accounts_dir().join(format!("{old}.json")), pl.accounts_dir().join(format!("{new}.json"))) {
                return fail(&e.to_string());
            }
            let mut c = load_cache(&pl);
            if let Some(v) = c.remove(old) {
                c.insert(new.clone(), v);
                let _ = write_json(&pl.usage(), &Value::Object(c), true);
            }
            println!("renamed {old} → {new}");
            0
        }
        "remove" | "rm" => {
            let Some(name) = rest.first().filter(|n| accounts(&pl).contains_key(n.as_str())) else { return fail("usage: claude-accounts remove <name>") };
            if sync_live(&pl, None, false).as_deref() == Some(name.as_str()) {
                return fail("that's the active account; switch away first");
            }
            match std::fs::remove_file(pl.accounts_dir().join(format!("{name}.json"))) {
                Ok(()) => {
                    println!("removed {name}");
                    0
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        "-V" | "--version" | "version" => {
            println!("claude-accounts {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "-h" | "--help" | "help" => {
            print!("{}", HELP);
            0
        }
        other => fail(&format!("unknown command '{other}' (claude-accounts help)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// What the profile endpoint says of each token, in tests (no network).
        pub static OWNERS: std::cell::RefCell<std::collections::HashMap<String, Value>> = Default::default();
    }

    #[test]
    fn times_read_as_the_api_writes_them() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0.));
        assert_eq!(parse_ts("2026-11-20T15:00:00+00:00"), Some(1_795_186_800.));
        assert_eq!(parse_ts("2026-11-20T16:00:00.5+01:00"), Some(1_795_186_800.5));
        assert_eq!(parse_ts("2026-11-20T10:00:00-05:00"), Some(1_795_186_800.));
        assert_eq!(parse_ts("soon"), None);
    }

    #[test]
    fn durations_read_as_before() {
        assert_eq!(dur(Some(3. * 86400. + 4. * 3600. + 59.), false), "3d 4h");
        assert_eq!(dur(Some(2. * 3600. + 5. * 60.), false), "2h 5m");
        assert_eq!(dur(Some(-5.), false), "0m");
        assert_eq!(dur(Some(2. * 86400.), true), "2D");
        assert_eq!(dur(Some(7200.), true), "2H");
        assert_eq!(dur(None, false), "?");
    }

    #[test]
    fn usage_is_cut_to_what_shows() {
        let u = json!({
            "five_hour": {"utilization": 12.0, "resets_at": "2026-11-20T15:00:00Z"},
            "seven_day": {"utilization": 40.5, "resets_at": "2026-11-24T00:00:00Z", "locked_reason": null},
            "limits": [{"kind": "weekly_scoped", "percent": 61.0, "is_active": true, "scope": {"model": {"display_name": "Opus"}}, "resets_at": "2026-11-24T00:00:00Z"},
                       {"kind": "other", "percent": 1.0}]
        });
        let s = summarize(&u);
        assert_eq!(s["5h"]["pct"], 12.0);
        assert_eq!(s["wk"]["reset"], 1_795_478_400.0);
        assert_eq!(s["scoped"][0]["model"], "Opus");
        assert!(s.get("locked").is_none());
        assert_eq!(summarize(&json!({"error": "HTTP 401"}))["error"], "HTTP 401");
    }

    #[test]
    fn of_weeks_ending_alike_the_next_account_has_the_most_room() {
        let cache: Map<String, Value> = serde_json::from_value(json!({
            "a": {"usage": {"wk": {"pct": 50.0, "reset": null}}},
            "b": {"usage": {"wk": {"pct": 99.0, "reset": null}}},
            "c": {"usage": {"5h": {"pct": 20.0, "reset": null}, "wk": {"pct": 10.0, "reset": null}}},
            "d": {"usage": {"error": "HTTP 401"}}
        }))
        .unwrap();
        let names: Vec<String> = ["a", "b", "c", "d"].map(String::from).to_vec();
        let t = 1_000_000.;
        assert_eq!(pick_next(&names, Some("a"), &cache, t, AUTO_AT).as_deref(), Some("c"));
        assert_eq!(pick_next(&names, Some("c"), &cache, t, AUTO_AT).as_deref(), Some("a"));
        assert_eq!(pick_next(&["b".to_string()], None, &cache, t, AUTO_AT), None, "under 2% left doesn't count");
        assert_eq!(pick_next(&names, Some("c"), &cache, t, 51.).as_deref(), None, "a has 1 point left under a 51% mark");
    }

    #[test]
    fn a_full_week_that_has_reset_is_room_and_a_lost_sign_in_is_none() {
        let t = 1_000_000.;
        let cache: Map<String, Value> = serde_json::from_value(json!({
            "on": {"usage": {"wk": {"pct": 99.0, "reset": t + 86400.}}},
            // Its numbers are from before its week reset: it's empty now.
            "reset": {"usage": {"wk": {"pct": 100.0, "reset": t - 60.}, "5h": {"pct": 100.0, "reset": t - 7200.}}},
            // Most room, but its sign-in is gone: switching to it would stop Claude.
            "gone": {"usage": {"wk": {"pct": 1.0, "reset": t + 86400.}}, "error": "sign-in expired: /login with it again"},
            "half": {"usage": {"wk": {"pct": 50.0, "reset": t + 86400.}}, "missed": "HTTP 429"}
        }))
        .unwrap();
        let names: Vec<String> = cache.keys().cloned().collect();
        assert_eq!(worst(cache["reset"].get("usage"), t), (0., None));
        // half's week ends tomorrow; reset's new week hasn't started, so nothing of it is lost by waiting.
        assert_eq!(pick_next(&names, Some("on"), &cache, t, AUTO_AT).as_deref(), Some("half"), "a 429 keeps the last numbers usable");
        assert_eq!(pick_next(&names, Some("reset"), &cache, t, AUTO_AT).as_deref(), Some("half"));
        assert_eq!(pick_next(&names, Some("half"), &cache, t, AUTO_AT).as_deref(), Some("reset"), "on has 1% left, gone can't be used");
    }

    #[test]
    fn auto_switch_goes_by_claudes_live_numbers() {
        let t = 1_000_000.;
        let (a_wk, b_wk) = (t + 86400., t + 4. * 86400.);
        // The last asked: a at 90% (minutes ago), b at 10%.
        let cache: Map<String, Value> = serde_json::from_value(json!({
            "a": {"usage": {"wk": {"pct": 90.0, "reset": a_wk}}},
            "b": {"usage": {"wk": {"pct": 10.0, "reset": b_wk}}}
        }))
        .unwrap();
        let names: Vec<String> = cache.keys().cloned().collect();
        let live = |wk: f64, reset: f64, h5: f64| json!({"rate_limits": {"seven_day": {"used_percentage": wk, "resets_at": reset}, "five_hour": {"used_percentage": h5, "resets_at": t + 3600.}}});
        let go = |cache: &Map<String, Value>, active: &str, d: &Value| auto_target(&names, active, cache, d, None, t, 100., false, false);
        // Claude's last answer put a's week within a request or two of 100%:
        // off to b before the next, without waiting for an ask.
        assert_eq!(go(&cache, "a", &live(99.8, a_wk, 40.)), Some(("weekly 99.8%".into(), "b".into())));
        assert_eq!(go(&cache, "a", &live(99.7, a_wk, 40.)), None, "still room for a couple of requests");
        // The 5-hour window counts too.
        assert_eq!(go(&cache, "a", &live(91., a_wk, 99.5)), Some(("5-hour 99.5%".into(), "b".into())));
        assert_eq!(go(&cache, "a", &json!({})), None, "no live numbers: the last asked");
        // Refused at a limit (the numbers stop where the last answer left them): off at once.
        assert_eq!(auto_target(&names, "a", &cache, &live(97.1, a_wk, 40.), None, t, 100., true, false), Some(("refused at its limit".into(), "b".into())));
        // ...even from an account picked by hand: that's kept only until it's full.
        assert_eq!(auto_target(&names, "a", &cache, &live(99.8, a_wk, 40.), None, t, 100., false, true).map(|x| x.1).as_deref(), Some("b"));
        // Just switched to b, a full: a session that hasn't asked since still
        // sends a's full week. Those aren't b's, so b stays.
        let mut moved = cache.clone();
        moved.insert("a".into(), json!({"usage": {"wk": {"pct": 100.0, "reset": a_wk}}}));
        assert_eq!(go(&moved, "b", &live(100., a_wk, 100.)), None);
        assert_eq!(window(active_view(&live(100., a_wk, 100.), &moved, "b", t).get("usage"), "wk").unwrap().0, 10.);
        // ...even with nothing asked of b's week yet (unused, no reset).
        moved.insert("b".into(), json!({"usage": {"wk": {"pct": 0.0, "reset": null}}}));
        assert_eq!(go(&moved, "b", &live(100., a_wk, 100.)), None);
        // Both full: nowhere to go.
        let mut full = cache.clone();
        full.insert("b".into(), json!({"usage": {"wk": {"pct": 99.5, "reset": b_wk}}}));
        assert_eq!(go(&full, "a", &live(100., a_wk, 10.)), None);
        // b refused since its last ask: not a place to go, whatever it said.
        let mut refused = cache.clone();
        refused.insert("b".into(), json!({"usage": {"wk": {"pct": 10.0, "reset": b_wk}}, "fetched": t - 100., "refused": t - 50.}));
        assert_eq!(go(&refused, "a", &live(99.9, a_wk, 10.)), None);
        // A lower mark (`auto 95`) is never passed either.
        assert_eq!(auto_target(&names, "a", &cache, &live(94.8, a_wk, 10.), None, t, 95., false, false).map(|x| x.1).as_deref(), Some("b"));
    }

    #[test]
    fn the_account_whose_week_ends_soonest_is_used_first() {
        let t = 1_000_000.;
        let (h, d) = (3600., 86400.);
        // alice at 22% with 5d 22h of her week left, bob at 19% with 3d 12h
        // left. What's left of a week is lost at its reset, so bob's goes first.
        let at = |pct: f64, reset: f64| json!({"usage": {"wk": {"pct": pct, "reset": reset}}});
        let (alice_end, bob_end) = (t + 5. * d + 22. * h + 60., t + 3. * d + 12. * h + 60.);
        let cache: Map<String, Value> = serde_json::from_value(json!({"alice": at(22., alice_end), "bob": at(19., bob_end)})).unwrap();
        let names: Vec<String> = cache.keys().cloned().collect();
        let go = |cache: &Map<String, Value>, active: &str, held: bool| auto_target(&names, active, cache, &json!({}), None, t, AUTO_AT, false, held);
        assert_eq!(go(&cache, "alice", false), Some(("week ends in 5d 22h, bob's in 3d 12h".into(), "bob".into())));
        assert_eq!(pick_next(&names, Some("alice"), &cache, t, AUTO_AT).as_deref(), Some("bob"));
        // On it, it stays: the other's week ends later.
        assert_eq!(go(&cache, "bob", false), None);
        assert_eq!(pick_next(&names, Some("bob"), &cache, t, AUTO_AT).as_deref(), Some("alice"));
        // Picked by hand: kept until it's full.
        assert_eq!(go(&cache, "alice", true), None);
        // Weeks ending within 10 minutes of each other end together: no move.
        let mut close = cache.clone();
        close.insert("bob".into(), at(19., alice_end - 300.));
        assert_eq!(go(&close, "alice", false), None);
        // No room on it (2 points or less, or its 5 hours used up): stay, and
        // go back once its 5 hours reset.
        let mut full = cache.clone();
        full.insert("bob".into(), at(98.5, bob_end));
        assert_eq!(go(&full, "alice", false), None);
        let five_full = json!({"usage": {"wk": {"pct": 40.0, "reset": bob_end}, "5h": {"pct": 100.0, "reset": t + h}}});
        full.insert("bob".into(), five_full);
        assert_eq!(go(&full, "alice", false), None);
        assert_eq!(auto_target(&names, "alice", &full, &json!({}), None, t + 2. * h, AUTO_AT, false, false).map(|x| x.1).as_deref(), Some("bob"));
        // Used up to its mark, it's left for the other; that doesn't send it back.
        let live = json!({"rate_limits": {"seven_day": {"used_percentage": 99.9, "resets_at": bob_end}}});
        assert_eq!(auto_target(&names, "bob", &cache, &live, None, t, AUTO_AT, false, false), Some(("weekly 99.9%".into(), "alice".into())));
        let mut spent = cache.clone();
        spent.insert("bob".into(), at(99.9, bob_end));
        assert_eq!(go(&spent, "alice", false), None);
        // A week not started (nothing used, no reset) loses nothing by waiting.
        let mut unused = cache.clone();
        unused.insert("alice".into(), json!({"usage": {"wk": {"pct": 0.0, "reset": null}}}));
        assert_eq!(go(&unused, "alice", false), Some(("week not started, bob's ends in 3d 12h".into(), "bob".into())));
        assert_eq!(go(&unused, "bob", false), None);
        // A sign-in gone or a refusal is no place to go, however soon its week ends.
        let mut gone = cache.clone();
        gone.insert("bob".into(), json!({"usage": {"wk": {"pct": 19.0, "reset": bob_end}}, "error": "sign-in expired: /login with it again"}));
        assert_eq!(go(&gone, "alice", false), None);
    }

    #[test]
    fn the_margin_is_what_a_request_or_two_costs() {
        let t = 1_000_000.;
        let r = t + 86400.;
        // Rises between looks close together are steps; a long gap isn't one.
        let a = observe(None, 50.0, r, t).unwrap();
        let b = observe(Some(&a), 50.4, r, t + 20.).unwrap();
        let c = observe(Some(&b), 50.4, r, t + 40.).unwrap();
        let d = observe(Some(&c), 50.9, r, t + 60.).unwrap();
        let e = observe(Some(&d), 55.0, r, t + 600.).unwrap();
        assert_eq!(e["steps"], json!([0.4, 0.5]));
        assert_eq!(e["pct"], 55.0);
        // An idle session's older (lower) number: nothing new, but it was a look.
        assert_eq!(observe(Some(&e), 52.0, r, t + 610.).unwrap()["pct"], 55.0);
        // An older window's numbers: ignored; a new window starts over.
        assert_eq!(observe(Some(&e), 99.0, r - 86400., t + 620.), None);
        assert_eq!(observe(Some(&e), 1.0, r + 7. * 86400., t + 630.).unwrap()["steps"], json!([]));
        // Twice the usual step, 0.2..2; a guess with none seen.
        assert_eq!(margin(Some(&e), "5h"), 1.0);
        assert_eq!(margin(Some(&json!({"steps": [0.1, 0.1, 3.0]})), "wk"), 0.2);
        assert_eq!(margin(Some(&json!({"steps": [5.0]})), "5h"), 2.0);
        assert_eq!((margin(None, "wk"), margin(None, "5h")), (0.2, 1.0));
        // With those steps seen, a 5-hour window moves on at 99% of it.
        let view = json!({"usage": {"5h": {"pct": 99.0, "reset": t + 3600.}}});
        assert!(switch_reason(&view, Some(&json!({"5h": e})), t, 100., false).is_some());
        assert!(switch_reason(&view, Some(&json!({"5h": {"steps": [0.1]}})), t, 100., false).is_none());
        // A window past its reset never has it move.
        assert!(switch_reason(&json!({"usage": {"5h": {"pct": 100.0, "reset": t - 1.}}}), None, t, 100., false).is_none());
        assert!(switch_reason(&json!({"usage": {"wk": {"pct": 100.0, "reset": null}}}), None, t, 100., false).is_some());
    }

    #[test]
    fn a_refusal_in_the_transcript_counts_once_and_for_the_account_it_hit() {
        let t = 1_794_807_800.; // 2026-11-16T05:43:20Z
        let refusal = r#"{"type":"assistant","timestamp":"2026-11-16T05:41:55.602Z","error":"rate_limit","isApiErrorMessage":true,"message":{"model":"<synthetic>","content":[{"type":"text","text":"You've hit your weekly limit · resets 2pm (America/New_York)"}]}}"#;
        let user = r#"{"type":"user","timestamp":"2026-11-16T05:41:55.000Z","message":{"content":"carry on"}}"#;
        let system = r#"{"type":"system","timestamp":"2026-11-16T05:41:56.000Z"}"#;
        let answer = r#"{"type":"assistant","timestamp":"2026-11-16T05:44:21.000Z","message":{"model":"claude-opus-5-5","content":[]}}"#;
        let tail = format!("partial line\n{user}\n{refusal}\n{system}\n");
        assert!(refused_in(&tail, t - 3600., t), "the last answer was a refusal");
        assert!(!refused_in(&tail, t - 60., t), "signed in to another since");
        assert!(!refused_in(&tail, t - 3600., t + 3600.), "too long ago");
        assert!(!refused_in(&format!("{tail}{answer}\n"), t - 3600., t), "answered since");
        assert!(!refused_in(&format!("{user}\n"), 0., t));
    }

    #[test]
    fn the_sign_in_is_whose_tokens_it_holds() {
        let accs: BTreeMap<String, Value> = serde_json::from_value(json!({
            "a": {"credentials": {"accessToken": "at-a", "refreshToken": "rt-a"}, "oauthAccount": {"accountUuid": "u-a"}},
            "b": {"credentials": {"accessToken": "at-b", "refreshToken": "rt-b"}, "oauthAccount": {"accountUuid": "u-b"}}
        }))
        .unwrap();
        // b's tokens while .claude.json still names a (a switch half written): b's.
        assert_eq!(whose(&accs, &json!({"accessToken": "at-b", "refreshToken": "rt-b"}), &json!({"accountUuid": "u-a"})), Some(("b".into(), true)));
        // Tokens Claude refreshed: whose .claude.json says.
        assert_eq!(whose(&accs, &json!({"accessToken": "at-new", "refreshToken": "rt-new"}), &json!({"accountUuid": "u-a"})), Some(("a".into(), false)));
        // Only the access token renewed: still a's.
        assert_eq!(whose(&accs, &json!({"accessToken": "at-new", "refreshToken": "rt-a"}), &json!({"accountUuid": "u-b"})), Some(("a".into(), true)));
        assert_eq!(whose(&accs, &json!({"accessToken": "x"}), &json!({"accountUuid": "u-c"})), None, "a new account");
        assert!(!same_tokens(Some(&json!({"accessToken": ""})), &json!({"accessToken": ""})), "empty isn't a match");
    }

    #[test]
    fn the_pace_says_when_the_week_runs_out() {
        let t = 1_000_000.;
        let reset = t + 3. * 86400.;
        // 10 points in the last hour: 50 left takes 5 hours, well before the reset.
        let ent = json!({"usage": {"wk": {"pct": 50.0, "reset": reset}}, "history": [[t - 3600., 40.0, reset], [t, 50.0, reset]]});
        let eta = projection(&ent, t).unwrap();
        assert!((eta - 5. * 3600.).abs() < 1., "{eta}");
        let slow = json!({"usage": {"wk": {"pct": 50.0, "reset": reset}}, "history": [[t - 3600., 49.9, reset], [t, 50.0, reset]]});
        assert_eq!(projection(&slow, t), None, "not before it resets");
        assert_eq!(projection(&json!({"usage": {"wk": {"pct": 100.0, "reset": reset}}}), t), Some(0.));
    }

    #[test]
    fn the_status_line_reads_as_it_should() {
        let t = 1_000_000.;
        let cache: Map<String, Value> = serde_json::from_value(json!({
            "main": {"usage": {"wk": {"pct": 40.0, "reset": t + 86400. + 3600.}, "5h": {"pct": 0.0, "reset": null}}, "history": []},
            "alt": {"usage": {"wk": {"pct": 10.0, "reset": t + 2. * 86400.}}}
        }))
        .unwrap();
        let d = json!({"model": {"display_name": "Opus 5.5"}, "effort": {"level": "xhigh"}, "context_window": {"used_percentage": 12.0},
                       "workspace": {"current_dir": "/home/k/src"}, "cost": {"total_lines_added": 3, "total_lines_removed": 1, "total_cost_usd": 1.5}});
        let names = vec!["alt".to_string(), "main".to_string()];
        let s = statusline(&d, &cache, Some("main"), &names, Path::new("/home/k"), t, AUTO_AT, false);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "◆ main wk 40% ⧗1d 1h | next alt | alt 10% (2D)");
        assert_eq!(lines[1], "Opus 5.5 xhigh | ctx 12% | ~/src | +3 -1 $1.50");
        let none = statusline(&json!({}), &Map::new(), None, &[], Path::new("/h"), t, AUTO_AT, false);
        assert!(none.starts_with("no Claude account signed in\n"));
    }

    #[test]
    fn a_session_stopped_on_a_limit_moves_on_or_retries_where_claude_just_went() {
        let t = 1_000_000.;
        let wk = |pct: f64| json!({"usage": {"wk": {"pct": pct, "reset": t + 86400.}}, "fetched": t - 60.});
        let cache: Map<String, Value> = serde_json::from_value(json!({"main": wk(100.), "alt": wk(20.), "full": wk(100.)})).unwrap();
        let names: Vec<String> = cache.keys().cloned().collect();
        // Moved long ago: the account it stopped on is full, so on to the one with room.
        assert_eq!(retry_plan(&names, "main", &cache, t - 3600., false, t, AUTO_AT), Retry::Move("alt".into()));
        // Another session's stop moved Claude a moment ago onto one with room: retry there.
        assert_eq!(retry_plan(&names, "alt", &cache, t - 30., false, t, AUTO_AT), Retry::Again("alt".into()));
        // Woken once already since: that one's full after all, and nothing else has room.
        assert_eq!(retry_plan(&names, "alt", &cache, t - 30., true, t, AUTO_AT), Retry::Stay);
        // Just moved, but onto a full one (refused since it was asked): move on.
        let mut refused = cache.clone();
        refused.insert("alt".into(), json!({"usage": {"wk": {"pct": 40.0, "reset": t + 86400.}}, "fetched": t - 600., "refused": t - 5.}));
        assert_eq!(retry_plan(&names, "alt", &refused, t - 30., false, t, AUTO_AT), Retry::Stay, "main and full are full too");
        // Nobody has room: stay stopped.
        let all_full: Map<String, Value> = serde_json::from_value(json!({"main": wk(100.), "alt": wk(99.), "full": wk(100.)})).unwrap();
        assert_eq!(retry_plan(&names, "main", &all_full, t - 3600., false, t, AUTO_AT), Retry::Stay);
        // Just moved onto one past a lower mark: that's full, so on to the one with room.
        let near: Map<String, Value> = serde_json::from_value(json!({"main": wk(96.), "alt": wk(50.)})).unwrap();
        let two = vec!["alt".to_string(), "main".to_string()];
        assert_eq!(retry_plan(&two, "main", &near, t - 30., false, t, AUTO_AT), Retry::Again("main".into()));
        assert_eq!(retry_plan(&two, "main", &near, t - 30., false, t, 95.), Retry::Move("alt".into()));
        // Wakes count for 15 minutes.
        assert_eq!(recent_wakes(Some(&json!([t - 1000., t - 800., t - 10.])), t), vec![t - 800., t - 10.]);
        assert!(recent_wakes(None, t).is_empty());
    }

    #[test]
    fn the_others_go_in_the_order_theyd_be_used() {
        let t = 1_000_000.;
        let (h, d) = (3600., 86400.);
        let cache: Map<String, Value> = serde_json::from_value(json!({
            "main": {"usage": {"wk": {"pct": 5.0, "reset": t + 6. * d}}},
            // full at the mark, clears in 3 days
            "a-full": {"usage": {"wk": {"pct": 100.0, "reset": t + 3. * d + h}}},
            // refused at a limit since it was asked: as full as 100%, clears in 7 hours
            "b-refused": {"usage": {"wk": {"pct": 97.0, "reset": t + 7. * h + 60.}}, "fetched": t - 600., "refused": t - 60.},
            "c-half": {"usage": {"wk": {"pct": 50.0, "reset": t + 2. * d}}},
            // same use as c-half, clears sooner
            "d-half-soon": {"usage": {"wk": {"pct": 50.2, "reset": t + d}}},
            "e-unknown": {"usage": {"error": "HTTP 401"}},
            // its week has reset since it was asked: empty
            "f-reset": {"usage": {"wk": {"pct": 100.0, "reset": t - 60.}}}
        }))
        .unwrap();
        let names: Vec<String> = cache.keys().cloned().collect();
        let order: Vec<&str> = others_order(&names, "main", &cache, t, AUTO_AT).into_iter().map(String::as_str).collect();
        // With room, the week that ends soonest first (f-reset's new week hasn't
        // started); then the full ones, the one that clears soonest first.
        assert_eq!(order, ["d-half-soon", "c-half", "f-reset", "b-refused", "a-full", "e-unknown"]);
        let line = statusline(&json!({}), &cache, Some("main"), &names, Path::new("/h"), t, AUTO_AT, false);
        assert!(line.lines().next().unwrap().ends_with("| next d-half-soon | d-half-soon 50% (1D) · c-half 50% (2D) · f-reset 0% (—) · b-refused 97% (7H) · a-full 100% (3D) · e-unknown ?"), "{line}");
        // A lower mark: 97% is as full as 100%, so the one that clears sooner goes first.
        let at97: Vec<&str> = others_order(&names, "main", &cache, t, 97.).into_iter().map(String::as_str).collect();
        assert_eq!(&at97[3..5], ["b-refused", "a-full"]);
        let mut cache2 = cache.clone();
        cache2.insert("b-refused".into(), json!({"usage": {"wk": {"pct": 97.0, "reset": t + 7. * h + 60.}}}));
        assert_eq!(&others_order(&names, "main", &cache2, t, 97.).into_iter().map(String::as_str).collect::<Vec<_>>()[3..5], ["b-refused", "a-full"]);
        // Not refused, 97% has room under 100%, and its week ends in 7 hours: first.
        assert_eq!(&others_order(&names, "main", &cache2, t, AUTO_AT).into_iter().map(String::as_str).collect::<Vec<_>>()[0..2], ["b-refused", "d-half-soon"]);
    }

    #[test]
    fn a_429_is_waited_out_not_shown() {
        let t = 1_000_000.;
        let d = json!({"model": {"display_name": "Opus 5.5"}});
        let names = vec!["main".to_string()];
        let line = |ent: Value| {
            let cache: Map<String, Value> = serde_json::from_value(json!({"main": ent})).unwrap();
            statusline(&d, &cache, Some("main"), &names, Path::new("/h"), t, AUTO_AT, false).lines().next().unwrap().to_string()
        };
        let wk = json!({"wk": {"pct": 40.0, "reset": t + 86400.}});
        // Just missed: the last numbers, nothing else.
        assert_eq!(line(json!({"usage": wk, "ok": t - 200., "fetched": t - 10., "missed": "HTTP 429", "retry_at": t + 110.})), "◆ main wk 40% ⧗1d 0h | next none");
        // Missed for long enough that the numbers are old: says how old, quietly.
        assert_eq!(line(json!({"usage": wk, "ok": t - 3. * 3600., "missed": "HTTP 429"})), "◆ main wk 40% ⧗1d 0h (3h 0m old) | next none");
        // Nothing yet: no number, no error.
        assert_eq!(line(json!({"usage": {"error": "HTTP 429"}, "missed": "HTTP 429"})), "◆ main usage ⟳ | next none");
        // A real error still shows.
        assert_eq!(line(json!({"usage": wk, "error": "sign-in expired: /login with it again"})), "◆ main wk 40% ⧗1d 0h !sign-in expired: /login with it again | next none");
        assert_eq!(problem(&json!({"usage": {"error": "HTTP 401"}})), Problem::Error("HTTP 401"));
        // The last point before a limit isn't rounded up to a full 100.
        assert_eq!([pct_text(99.5), pct_text(99.96), pct_text(100.), pct_text(42.4)], ["99.5", "100.0", "100", "42"]);
    }

    #[test]
    fn misses_back_off() {
        assert_eq!(backoff(1., 0.), BACKOFF_MIN);
        assert_eq!(backoff(2., 0.), 2. * BACKOFF_MIN);
        assert_eq!(backoff(40., 0.), BACKOFF_MAX);
        assert_eq!(backoff(1., 900.), 900., "Retry-After wins when longer");
        let t = 1_000_000.;
        assert!(!due(Some(&json!({"fetched": t - 4000., "retry_at": t + 5.})), "x", None, false, t), "waits out a miss");
        assert!(due(Some(&json!({"fetched": t - 4000., "retry_at": t - 5.})), "x", None, false, t));
        assert!(!due(Some(&json!({"fetched": t - 10.})), "x", Some("x"), false, t));
        assert!(due(None, "x", Some("x"), false, t));
    }

    #[test]
    fn the_pace_history_survives_a_jittery_reset() {
        assert!(same_reset(&json!(1_794_855_599.803_77), &json!(1_794_855_600.100_588)));
        assert!(!same_reset(&json!(1_794_855_600.), &json!(1_794_855_600. + 86400.)));
        assert!(same_reset(&Value::Null, &Value::Null) && !same_reset(&json!(1.), &Value::Null));
    }

    #[test]
    fn claude_codes_own_numbers_fill_in() {
        let t = 1_000_000.;
        let reset = t + 86400.;
        let d = |pct: f64, r: f64| json!({"rate_limits": {"seven_day": {"used_percentage": pct, "resets_at": r}, "five_hour": {"used_percentage": 30.0, "resets_at": t + 3600.}}});
        let ent = json!({"usage": {"wk": {"pct": 40.0, "reset": reset - 0.3}}, "ok": t - 5000., "missed": "HTTP 429"});
        // Same week, newer (higher): taken, and the numbers count as fresh.
        let e = with_live(&d(45., reset), ent.clone(), t);
        assert_eq!(window(e.get("usage"), "wk").unwrap().0, 45.);
        assert_eq!(window(e.get("usage"), "5h").unwrap().0, 30.);
        assert_eq!(got_at(&e), t);
        // An idle session's older (lower) number doesn't undo a newer ask.
        assert_eq!(window(with_live(&d(35., reset), ent.clone(), t).get("usage"), "wk").unwrap().0, 40.);
        // Another account's week: left alone.
        assert_eq!(with_live(&d(5., reset + 3. * 86400.), ent.clone(), t), ent);
        // No cache yet: Claude's numbers are all there is.
        let e = with_live(&d(12., reset), json!({"usage": {"error": "HTTP 429"}, "missed": "HTTP 429"}), t);
        assert_eq!(window(e.get("usage"), "wk").unwrap().0, 12.);
        assert_eq!(problem(&e), Problem::Missed("HTTP 429"));
        assert_eq!(with_live(&json!({}), ent.clone(), t), ent);
    }

    #[test]
    fn names_are_only_ever_file_names() {
        assert_eq!(default_name("jane.doe+x@example.com"), "jane.doe_x");
        assert_eq!(default_name(""), "account");
        assert!(valid_name("work-2") && !valid_name("../x") && !valid_name(".hidden") && !valid_name("a b"));
    }

    #[test]
    fn hex_reads_back() {
        assert_eq!(unhex("7b7d").as_deref(), Some(&b"{}"[..]));
        assert_eq!(unhex("7b7"), None);
        assert_eq!(unhex("zz"), None);
    }

    /// macOS (the pipeline's Mac): Claude's sign-in into a keychain and back
    /// out, written as Claude writes it (`security -i`, hex on stdin), in a
    /// throwaway keychain; written again, the second replaces the first (-U).
    #[cfg(target_os = "macos")]
    #[test]
    fn the_keychain_takes_claudes_sign_in_and_gives_it_back() {
        struct Gone(PathBuf);
        impl Drop for Gone {
            fn drop(&mut self) {
                let _ = Command::new("/usr/bin/security").arg("delete-keychain").arg(&self.0).output();
            }
        }
        let kc = std::env::temp_dir().join(format!("claude-accounts-test-{}.keychain-db", std::process::id()));
        let _ = std::fs::remove_file(&kc);
        let made = Command::new("/usr/bin/security").args(["create-keychain", "-p", "claude-accounts-test"]).arg(&kc).output();
        if !made.as_ref().is_ok_and(|o| o.status.success()) {
            eprintln!("skipped: no keychain could be made here ({made:?})");
            return;
        }
        let _gone = Gone(kc.clone());
        let _ = Command::new("/usr/bin/security").args(["unlock-keychain", "-p", "claude-accounts-test"]).arg(&kc).output();
        let first = json!({"claudeAiOauth": {"accessToken": "sk-\"quoted\" é", "refreshToken": "r1", "expiresAt": 1}});
        keychain_write_in(&first, Some(&kc)).unwrap();
        assert_eq!(keychain_get(Some(&kc)), Keychain::Json(first));
        let second = json!({"claudeAiOauth": {"accessToken": "a2", "refreshToken": "r2", "expiresAt": 2}});
        keychain_write_in(&second, Some(&kc)).unwrap();
        assert_eq!(keychain_get(Some(&kc)), Keychain::Json(second), "-U replaces it");
        // One too long for `security -i`'s line (MCP servers' sign-ins beside Claude's) goes as arguments.
        let big = json!({"claudeAiOauth": {"accessToken": "a3", "refreshToken": "r3"}, "mcpOAuth": {"server": {"accessToken": "x".repeat(3000)}}});
        keychain_write_in(&big, Some(&kc)).unwrap();
        assert_eq!(keychain_get(Some(&kc)), Keychain::Json(big));
    }

    /// A throwaway store: home and Claude's folder in it.
    fn temp_places(tag: &str) -> (Places, PathBuf) {
        let root = std::env::temp_dir().join(format!("claude-accounts-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        (places_in(home, None).unwrap(), root)
    }

    #[test]
    fn a_probes_headers_are_its_usage() {
        let h = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        // As the API answered a full account (2026-11-22): refused, with its numbers.
        let full = usage_from_headers(429, h(&[("anthropic-ratelimit-unified-7d-utilization", "1.0"), ("anthropic-ratelimit-unified-7d-reset", "1795460400"), ("anthropic-ratelimit-unified-status", "rejected")]));
        let s = summarize(&full);
        assert_eq!(s["wk"]["pct"], 100.);
        assert_eq!(s["wk"]["reset"], 1795460400.);
        assert!(s.get("5h").is_none());
        let some = usage_from_headers(200, h(&[("anthropic-ratelimit-unified-5h-utilization", "0.1234"), ("anthropic-ratelimit-unified-5h-reset", "1795400000"), ("anthropic-ratelimit-unified-7d-utilization", "0.95"), ("anthropic-ratelimit-unified-7d-reset", "1795600000")]));
        let s = summarize(&some);
        assert_eq!((s["5h"]["pct"].as_f64(), s["wk"]["pct"].as_f64()), (Some(12.3), Some(95.)));
        assert_eq!(worst(Some(&s), 1795300000.), (95., Some(1795600000.)));
        // No numbers: what the status says; a refused token is an error, not a miss.
        assert!(usage_from_headers(401, h(&[])).get("miss").is_none());
        assert!(summarize(&usage_from_headers(401, h(&[]))).get("error").is_some());
        let busy = usage_from_headers(429, h(&[("retry-after", "30")]));
        assert_eq!((busy["miss"].as_bool(), busy["retry_after"].as_f64()), (Some(true), Some(30.)));
        assert_eq!(usage_from_headers(529, h(&[]))["miss"], true);
        // The endpoint's own answer still reads (a time string).
        assert_eq!(summarize(&json!({"seven_day": {"utilization": 50., "resets_at": "2026-11-20T15:00:00+00:00"}}))["wk"]["reset"], 1_795_186_800.);
    }

    #[test]
    fn a_probed_account_is_asked_less_often() {
        let e = json!({"fetched": 1000.});
        assert!(due(Some(&e), "a", Some("b"), false, 1000. + TTL_OTHER));
        assert!(!due(Some(&e), "a", Some("b"), true, 1000. + TTL_OTHER));
        assert!(due(Some(&e), "a", Some("b"), true, 1000. + TTL_PROBE_OTHER));
        assert!(!due(Some(&e), "a", Some("a"), true, 1000. + TTL_ACTIVE));
        assert!(due(Some(&e), "a", Some("a"), true, 1000. + TTL_PROBE_ACTIVE));
    }

    fn ent(wk: f64, reset: f64) -> Value {
        json!({"usage": {"wk": {"pct": wk, "reset": reset}}, "fetched": 0., "ok": 0.})
    }

    #[test]
    fn room_is_the_active_ones_or_the_next_else_when_the_first_clears() {
        let t = 1000.;
        let names: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let mut cache = Map::new();
        cache.insert("a".into(), ent(100., t + 3. * 86400.));
        cache.insert("b".into(), ent(40., t + 5. * 86400.));
        assert_eq!(room_now(&names, Some("a"), &cache, t, 100.), Ok("b".into()), "the next, the active one full");
        assert_eq!(room_now(&names, Some("b"), &cache, t, 100.), Ok("b".into()));
        cache.insert("b".into(), ent(100., t + 3600.));
        assert_eq!(room_now(&names, Some("a"), &cache, t, 100.), Err(Some(t + 3600.)), "all full: the first to clear");
        // Past that reset the numbers are from before it: room again.
        assert_eq!(room_now(&names, Some("a"), &cache, t + 3601., 100.), Ok("b".into()));
        assert_eq!(room_now(&names, None, &Map::new(), t, 100.), Err(None), "nothing known");
    }

    #[test]
    fn a_session_is_still_stopped_while_its_last_turn_is_the_refusal() {
        let refusal = r#"{"type":"assistant","error":"rate_limit","isApiErrorMessage":true,"message":{"content":[{"type":"text","text":"You've hit your weekly limit"}]}}"#;
        let user = r#"{"type":"user","message":{"role":"user","content":"carry on"}}"#;
        let answer = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Back at it."}]}}"#;
        let system = r#"{"type":"system","subtype":"stop_hook_summary"}"#;
        assert!(still_stopped(&format!("{answer}\n{refusal}\n{system}\n")), "a system line after it doesn't count");
        assert!(!still_stopped(&format!("{refusal}\n{user}\n")), "someone typed (or the relay's carry-on)");
        assert!(!still_stopped(&format!("{refusal}\n{user}\n{answer}\n")), "it went on");
        assert!(still_stopped(&format!("{refusal}\n{user}\n{refusal}\n")), "a retry into the same full house");
        assert!(!still_stopped(answer), "never stopped");
        assert!(!still_stopped("not json\n"));
    }

    #[test]
    fn the_stop_hook_may_wait_for_a_reset() {
        let s = with_accounts_settings(json!({}), "claude-accounts").unwrap();
        assert_eq!(s["hooks"]["StopFailure"][0]["hooks"][0]["timeout"], STOP_HOOK_TIMEOUT);
        assert!(STOP_HOOK_TIMEOUT as f64 > WAIT_FOR_ROOM, "the hook outlives its wait");
        // One written before (60 s, from before the hook waited) is raised; someone's own hooks stay.
        let old = json!({"statusLine": {"command": "x"}, "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": "claude-accounts hook session-start"}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "claude-accounts hook prompt"}]}],
            "StopFailure": [{"matcher": "rate_limit|billing_error", "hooks": [{"type": "command", "command": "claude-accounts hook stop-failure", "asyncRewake": true, "timeout": 60}, {"type": "command", "command": "notify-me", "timeout": 5}]}]}});
        let s = with_accounts_settings(old, "claude-accounts").unwrap();
        assert_eq!(s["hooks"]["StopFailure"][0]["hooks"][0]["timeout"], STOP_HOOK_TIMEOUT);
        assert_eq!(s["hooks"]["StopFailure"][0]["hooks"][1]["timeout"], 5);
        assert_eq!(s["hooks"]["StopFailure"].as_array().unwrap().len(), 1, "not added twice");
        assert_eq!(with_accounts_settings(s, "claude-accounts"), None, "then nothing to change");
    }

    #[test]
    fn install_gives_claude_the_status_line_and_hooks_once() {
        let s = with_accounts_settings(json!({}), "claude-accounts").unwrap();
        assert_eq!(s["statusLine"]["command"], "claude-accounts statusline");
        assert_eq!(s["hooks"]["StopFailure"][0]["matcher"], "rate_limit|billing_error");
        assert_eq!(s["hooks"]["StopFailure"][0]["hooks"][0]["asyncRewake"], true);
        assert_eq!(s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"], "claude-accounts hook prompt");
        assert_eq!(with_accounts_settings(s.clone(), "claude-accounts"), None, "already there: unchanged");
        // Someone's own status line and hooks stay; only the missing ones are added.
        let mine = json!({"statusLine": {"type": "command", "command": "my-line"}, "hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": "/x/claude-accounts hook prompt"}]}], "Stop": [{"hooks": [{"type": "command", "command": "beep"}]}]}, "model": "opus"});
        let s = with_accounts_settings(mine, "claude-accounts").unwrap();
        assert_eq!(s["statusLine"]["command"], "my-line");
        assert_eq!(s["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 1);
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], "beep");
        assert_eq!(s["hooks"]["SessionStart"][0]["hooks"][0]["command"], "claude-accounts hook session-start");
        assert_eq!(s["model"], "opus");
        assert_eq!(with_accounts_settings(json!([1]), "claude-accounts"), None);
    }

    #[test]
    fn install_writes_claudes_settings_beside_its_sign_in() {
        let (pl, root) = temp_places("settings");
        assert_eq!(ensure_claude_settings(&pl, "claude-accounts"), Ok(true));
        let s = read_json(&pl.claude.join("settings.json")).unwrap();
        assert_eq!(s["statusLine"]["command"], "claude-accounts statusline");
        assert!(!pl.claude.join("settings.json.before-claude-accounts").exists(), "nothing was there to keep");
        assert_eq!(ensure_claude_settings(&pl, "claude-accounts"), Ok(false), "once");
        // Yours, kept beside it once before it's changed.
        std::fs::write(pl.claude.join("settings.json"), r#"{"model": "opus"}"#).unwrap();
        assert_eq!(ensure_claude_settings(&pl, "/opt/x/claude-accounts"), Ok(true));
        assert_eq!(std::fs::read_to_string(pl.claude.join("settings.json.before-claude-accounts")).unwrap(), r#"{"model": "opus"}"#);
        let s = read_json(&pl.claude.join("settings.json")).unwrap();
        assert_eq!((s["model"].as_str(), s["statusLine"]["command"].as_str()), (Some("opus"), Some("/opt/x/claude-accounts statusline")));
        // A settings file that doesn't read is left alone.
        std::fs::write(pl.claude.join("settings.json"), "{ not json").unwrap();
        assert!(ensure_claude_settings(&pl, "claude-accounts").is_err());
        assert_eq!(std::fs::read_to_string(pl.claude.join("settings.json")).unwrap(), "{ not json");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn claude_runs_this_program_by_name_on_path_else_by_its_path() {
        let bin = std::env::temp_dir().join("claude-accounts-bin");
        let exe = bin.join(format!("claude-accounts{}", std::env::consts::EXE_SUFFIX));
        let path = std::env::join_paths([Path::new("/usr/bin"), bin.as_path()]).unwrap();
        assert_eq!(program_for_claude(&exe, Some(&path)).as_deref(), Some("claude-accounts"));
        let away = std::env::join_paths([Path::new("/usr/bin")]).unwrap();
        assert_eq!(program_for_claude(&exe, Some(&away)), Some(exe.to_string_lossy().replace('\\', "/")));
        assert_eq!(program_for_claude(Path::new("/opt/my tools/claude-accounts"), None).as_deref(), Some("\"/opt/my tools/claude-accounts\""));
        assert_eq!(program_for_claude(Path::new(r"C:\Users\me\.cargo\bin\claude-accounts.exe"), None).as_deref(), Some("C:/Users/me/.cargo/bin/claude-accounts.exe"));
        assert_eq!(program_for_claude(Path::new("/tmp/$(boom)/claude-accounts"), None), None, "never one a shell would read");
        assert_eq!(program_for_claude(Path::new("/tmp/it's/claude-accounts"), None), None);
        let cmd = accounts_command("claude-accounts");
        assert!(cmd.starts_with("---\ndescription: Claude accounts") && cmd.contains("\n!`claude-accounts $ARGUMENTS 2>&1`\n"), "{cmd}");
    }

    /// The live sign-in's file as Claude writes it, and `.claude.json` naming whose it is.
    #[cfg(not(target_os = "macos"))]
    fn sign_in(pl: &Places, access: &str, refresh: &str, uuid: &str, email: &str) {
        write_json(&pl.creds_file(), &json!({"claudeAiOauth": {"accessToken": access, "refreshToken": refresh, "expiresAt": 4_102_444_800_000_i64, "scopes": ["user:inference", "user:profile"]}}), true).unwrap();
        write_json(&pl.claude_json, &json!({"oauthAccount": {"accountUuid": uuid, "emailAddress": email}}), true).unwrap();
    }

    // Not on macOS: there Claude's sign-in is the login keychain's (the real one, whatever
    // HOME says), which a test must never touch; the keychain has its own test above.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_sign_in_claude_cleared_never_replaces_an_accounts_tokens() {
        let (pl, root) = temp_places("cleared");
        sign_in(&pl, "a1", "r1", "u1", "jane@example.com");
        assert_eq!(sync_live(&pl, None, false).as_deref(), Some("jane"), "a new sign-in is kept, named for its email");
        sign_in(&pl, "b1", "s1", "u2", "sam@example.com");
        assert_eq!(sync_live(&pl, None, false).as_deref(), Some("sam"));
        // Claude logged out of sam (or its refresh failed): its tokens emptied,
        // .claude.json still naming it. Before, sam's kept tokens became these.
        let cleared = json!({"claudeAiOauth": {"accessToken": "", "refreshToken": "", "expiresAt": 0}});
        write_json(&pl.creds_file(), &cleared, true).unwrap();
        assert_eq!(sync_live(&pl, None, false), None, "not signed in");
        assert_eq!(accounts(&pl)["sam"]["credentials"]["refreshToken"], "s1", "sam's tokens stand");
        // And switching back in from there signs Claude in with them.
        activate(&pl, "jane").unwrap();
        assert_eq!(read_json(&pl.creds_file()).unwrap()["claudeAiOauth"]["refreshToken"], "r1");
        assert_eq!(read_json(&pl.claude_json).unwrap()["oauthAccount"]["emailAddress"], "jane@example.com");
        assert_eq!(sync_live(&pl, None, false).as_deref(), Some("jane"));
        activate(&pl, "sam").unwrap();
        assert_eq!(sync_live(&pl, None, false).as_deref(), Some("sam"));
        assert_eq!(accounts(&pl).len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_keychain_item_is_named_as_claude_names_it() {
        use std::ffi::OsStr;
        assert_eq!(keychain_service_for(None, None), "Claude Code-credentials");
        assert_eq!(keychain_service_for(None, Some(OsStr::new(""))), "Claude Code-credentials");
        // sha256("/Users/me/.claude-work") starts 1e91dd84.
        let named = keychain_service_for(None, Some(OsStr::new("/Users/me/.claude-work")));
        assert_eq!(named, "Claude Code-credentials-1e91dd84");
        assert_eq!(keychain_service_for(Some(OsStr::new("/Users/me/.claude-work")), None), named, "the secure-storage folder when set");
        assert_eq!(keychain_service_for(Some(OsStr::new("")), Some(OsStr::new("/x"))), "Claude Code-credentials", "set but empty: the plain name");
        assert_eq!(keychain_user_from(Some("jane".into())), "jane");
        assert_eq!(keychain_user_from(Some("jane doe".into())), "claude-code-user");
        assert_eq!(keychain_user_from(None), "claude-code-user");
    }

    #[test]
    fn a_keychain_write_never_puts_the_secret_on_a_command_line_unless_it_must() {
        let small = json!({"claudeAiOauth": {"accessToken": "a\"b"}});
        let (args, input) = keychain_write_command(&small, "jane", "Claude Code-credentials", None);
        assert_eq!(args, ["-i"]);
        let line = input.unwrap();
        assert!(line.starts_with("add-generic-password -U -a \"jane\" -s \"Claude Code-credentials\" -X \"7b22") && line.ends_with("\"\n"), "{line}");
        assert!(!line.contains("a\\\"b"), "the secret only as hex");
        let big = json!({"claudeAiOauth": {"accessToken": "x".repeat(3000)}});
        let (args, input) = keychain_write_command(&big, "jane", "Claude Code-credentials", Some(Path::new("/k.keychain-db")));
        assert_eq!((args[0].as_str(), args.last().map(String::as_str), input), ("add-generic-password", Some("/k.keychain-db"), None));
    }

    #[test]
    fn a_profile_says_whose_a_token_is() {
        let v = json!({"account": {"uuid": "u1", "email": "jane@example.com", "display_name": "Jane"}, "organization": {"uuid": "o1"}});
        assert_eq!(owner_record(&v), Some(json!({"accountUuid": "u1", "emailAddress": "jane@example.com", "organizationUuid": "o1"})));
        assert_eq!(owner_record(&json!({"account": {"uuid": ""}})), None);
        assert_eq!(owner_record(&json!({"error": "x"})), None);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn new_tokens_go_to_whose_they_are_not_whom_claudes_record_names() {
        let (pl, root) = temp_places("owner");
        OWNERS.with(|m| {
            let mut m = m.borrow_mut();
            m.insert("a1".into(), json!({"accountUuid": "u1", "emailAddress": "jane@example.com"}));
            m.insert("c1".into(), json!({"accountUuid": "u3", "emailAddress": "cat@example.com"}));
            m.insert("a2".into(), json!({"accountUuid": "u1", "emailAddress": "jane@example.com"}));
        });
        sign_in(&pl, "a1", "r1", "u1", "jane@example.com");
        let mut cj = read_json(&pl.claude_json).unwrap();
        cj["oauthAccount"]["displayName"] = json!("Jane");
        write_json(&pl.claude_json, &cj, true).unwrap();
        assert_eq!(sync_live(&pl, None, false).as_deref(), Some("jane"));
        // /login as cat, its tokens written, .claude.json still naming jane:
        // before, jane's file got cat's tokens.
        sign_in(&pl, "c1", "q1", "u1", "jane@example.com");
        assert_eq!(sync_live(&pl, None, false).as_deref(), Some("cat"), "filed as the API says");
        let accs = accounts(&pl);
        assert_eq!(accs["jane"]["credentials"]["refreshToken"], "r1", "jane's stand");
        assert_eq!(accs["cat"]["oauthAccount"]["emailAddress"], "cat@example.com");
        // Jane's refreshed by Claude while .claude.json names cat (written back late):
        // the new ones are jane's, and jane keeps her own record.
        sign_in(&pl, "a2", "r2", "u3", "cat@example.com");
        assert_eq!(sync_live(&pl, None, false).as_deref(), Some("jane"));
        let accs = accounts(&pl);
        assert_eq!((accs["jane"]["credentials"]["refreshToken"].as_str(), accs["jane"]["oauthAccount"]["displayName"].as_str()), (Some("r2"), Some("Jane")), "her whole record, not the API's few fields");
        assert_eq!(accs["cat"]["credentials"]["refreshToken"], "q1");
        // Not known (no network): as .claude.json says, as before.
        sign_in(&pl, "z9", "y9", "u5", "zed@example.com");
        assert_eq!(sync_live(&pl, None, false).as_deref(), Some("zed"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_name_in_use_is_never_written_over() {
        let (pl, root) = temp_places("names");
        sign_in(&pl, "a1", "r1", "u1", "jane@example.com");
        assert_eq!(sync_live(&pl, None, false).as_deref(), Some("jane"));
        sign_in(&pl, "b1", "s1", "u2", "sam@example.com");
        assert_eq!(sync_live(&pl, None, false).as_deref(), Some("sam"));
        // `add jane` while signed in as sam: sam stays sam, jane stays jane.
        assert_eq!(sync_live(&pl, Some("jane"), false).as_deref(), Some("sam"));
        let accs = accounts(&pl);
        assert_eq!((accs["jane"]["credentials"]["refreshToken"].as_str(), accs["sam"]["credentials"]["refreshToken"].as_str()), (Some("r1"), Some("s1")));
        // A free name renames.
        assert_eq!(sync_live(&pl, Some("samwork"), false).as_deref(), Some("samwork"));
        assert_eq!(accounts(&pl).keys().cloned().collect::<Vec<_>>(), ["jane", "samwork"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_sign_in_holds_tokens_or_is_none() {
        assert!(holds_tokens(&json!({"accessToken": "a", "refreshToken": ""})));
        assert!(holds_tokens(&json!({"refreshToken": "r"})));
        assert!(!holds_tokens(&json!({"accessToken": "", "refreshToken": "", "expiresAt": 0})));
        assert!(!holds_tokens(&json!({"accessToken": " "})));
        assert!(!holds_tokens(&json!({})));
    }
}
