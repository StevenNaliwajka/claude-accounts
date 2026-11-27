# claude-accounts

Several Claude subscriptions in [Claude Code](https://claude.com/claude-code) as one pool, with a status line that says when your tokens run out. Linux, macOS and Windows.

```
◆ home wk 43%/100 ⧗5h 13m ▲ limit in 2h 21m (04:00) | next work | work 31% (5D) · spare 100% (3D)
ETA ▼ all out Sun 04:14 (in 2d 2h) → back Mon 14:00 (spare) · 77 pts left at 1.5/h
Opus 5.5 xhigh | ctx 15% 145k/1M · cache 52m | ~/proj ⎇ main | +120 -14 25m $4.20
```

- **When you run out, before you do.** The active account's ETA at its recent pace: `▲ limit in 2h 21m (04:00)`, or `✓ ~72% by reset` when it lasts. The ETA line plays the whole pool forward at your last day's burn (every account, its reset, the order they'd be used in): when **every** account is out, and which one comes back first and when. Or `✓ tokens last the week`.
- **Switches by itself.** A request or two before the active account reaches its limit (Claude's own live numbers, after every answer), Claude moves to the next account with room. If a request is refused at a limit anyway, it moves at once and the stopped session is woken to carry on. With every account full, the session waits for the first reset and then carries on by itself.
- **Uses the week that ends soonest first.** What's left of a week is lost at its reset, so of the accounts with room the one whose weekly window resets first is used first. An account you pick by hand is kept until it's full.
- **The session too**: model and effort (and fast mode), the context's tokens, when the prompt cache expires (`cache 52m`, or `cache cold`: the next request writes the whole context again), folder and branch, lines changed, time, cost.
- **Session titles.** Each session is named after what it's working on (the first request, or a `/goal`), which Claude Code shows as the terminal's title: a tab in Windows Terminal, Konsole or iTerm says which session is which. The name comes from a small model (`claude -p --model haiku`, a couple of seconds), else from the request's own words. A name you give (`/rename`) is never replaced.

Times are in your own time zone: `04:00` today, `Sun 04:14` this week, `Dec 7` later. "pts" are points of an account's week (100 is one full week of one account).

## Install

With Rust (1.89 or newer):

```sh
cargo install --git https://github.com/StevenNaliwajka/claude-accounts
```

Or download a build for your system from [Releases](https://github.com/StevenNaliwajka/claude-accounts/releases) and put `claude-accounts` (`claude-accounts.exe` on Windows) somewhere on your PATH, such as `~/.local/bin`. On macOS, a downloaded binary is unsigned: `xattr -d com.apple.quarantine claude-accounts` lets it run.

Then, once:

```sh
claude-accounts install
```

This sets Claude Code's status line and hooks in `~/.claude/settings.json` (a status line of your own is left alone, and your settings file is copied to `settings.json.before-claude-accounts` first), adds the `/accounts` command, and keeps the account Claude is signed in with now.

To add another account: in Claude, `/login` with it (no `/logout` first). It's picked up on the next status line, or run `claude-accounts add [name]`. Accounts are named after the part of their email before the `@`; `claude-accounts rename` changes that.

## Commands

```
claude-accounts [list [--fresh]]   usage on every account
claude-accounts add [name]         keep the account Claude is signed in with now
claude-accounts switch <name>      make <name> the one Claude uses (a prefix of its name or email works)
claude-accounts next               switch to the next: the soonest-ending week with room
claude-accounts auto [on|off|<pct>]   auto-switch on/off, or the use it switches at (100% unless told)
claude-accounts rename <old> <new>
claude-accounts remove <name>      forget an account (not the active one)
claude-accounts refresh [--force]  ask every account's usage again
claude-accounts install            set up Claude's status line, hooks and /accounts
```

In Claude, `/accounts` runs the same commands: `/accounts`, `/accounts switch work`, `/accounts next`. It runs locally, so it works even while Claude is refused at a limit.

`claude-accounts statusline` and `claude-accounts hook <event>` are what Claude Code runs (`install` sets them up):

| Hook | What it does |
|---|---|
| `SessionStart` | a resumed session gets its title back |
| `UserPromptSubmit` | moves Claude off an account at its limit before the prompt goes out; names the session |
| `StopFailure` (`rate_limit`, `billing_error`) | moves Claude off the account a turn stopped on and wakes the session to carry on; with every account full, waits for a reset (up to 8 days) first |

## How it works

- **Claude's sign-in** is where Claude keeps it: `~/.claude/.credentials.json` on Linux and Windows, the login keychain on macOS (item `Claude Code-credentials`, written the way Claude writes it, the secret never on a command line). A switch writes the account's tokens there and its record into `~/.claude.json`; Claude Code reads its sign-in again whenever it changes. `CLAUDE_CONFIG_DIR` is honoured.
- **The store**: `~/.config/claude-accounts/accounts/<name>.json`, one file per account, readable by you only (on Windows, your profile folder's permissions). Usage, logs and session titles are in `~/.cache/claude-accounts`.
- **The ETA** uses each account's usage history (its last day, kept in the usage cache): the active account's own pace over the last 3 hours (else its week's average) for its limit, and every account's rise over the last day for the pool's burn (shown after 2 hours of history). The pool is played forward up to 7 days: an account is used up to the auto-switch mark, a reset empties it, and Claude stays on the account it's on unless that would leave part of a week ending sooner unused.
- **Usage** is read from the endpoint Claude Code's own `/usage` reads, at most every 3 minutes for the active account and 5 for the others, backing off when it rate-limits. Between reads, the active account's numbers come live from Claude Code's status-line input. A token that can't read that endpoint (one narrowed to inference, as `claude setup-token` makes) has its usage read from the rate-limit headers of a one-token request to the smallest model, as Claude Code's own quota check does.
- **Tokens** of accounts Claude isn't using are refreshed with Claude Code's own OAuth client when they run out; the active one is left to Claude. One file lock keeps every Claude session's status line and hooks from stepping on each other.
- Every change is logged to `~/.cache/claude-accounts/log` (never a token).

## Notes

- It relies on how Claude Code keeps its sign-in and on endpoints Claude Code itself uses, none of them a published API: a Claude Code update can change them. Not affiliated with Anthropic.
- Use it with your own accounts. A subscription is for one person; this tool doesn't share accounts between people.
- Tested on Linux, macOS and Windows (`cargo test`; CI runs all three).

## License

MIT
