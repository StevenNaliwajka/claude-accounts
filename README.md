# claude-accounts

Several Claude subscriptions in [Claude Code](https://claude.com/claude-code), on Linux, macOS and Windows.

- **Keeps every account you sign in with.** `/login` in Claude with another account and it's kept; switch between them with one command, and every running Claude session uses the new one from its next request (no restart).
- **Switches by itself.** A request or two before the active account reaches its limit (Claude's own live numbers, after every answer), Claude moves to the next account with room. If a request is refused at a limit anyway, it moves at once and the stopped session is woken to carry on. With every account full, the session waits for the first reset and then carries on by itself.
- **Uses the week that ends soonest first.** What's left of a week is lost at its reset, so of the accounts with room the one whose weekly window resets first is used first. An account you pick by hand is kept until it's full.
- **A status line with every account's usage**, the next one up, and the session (model, context, folder and branch, lines changed, time, cost):

  ```
  ◆ home wk 35% ⧗1d 5h | next work | work 62% (3D) · spare 100% (4H)
  Opus 5.5 xhigh | ctx 23% | ~/proj ⎇ main | +120 -14 25m $4.20
  ```

- **Session titles.** Each session is named after what it's working on (the first request, or a `/goal`), which Claude Code shows as the terminal's title: a tab in Windows Terminal, Konsole or iTerm says which session is which. The name comes from a small model (`claude -p --model haiku`, a couple of seconds), else from the request's own words. A name you give (`/rename`) is never replaced.

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
- **Usage** is read from the endpoint Claude Code's own `/usage` reads, at most every 3 minutes for the active account and 5 for the others, backing off when it rate-limits. Between reads, the active account's numbers come live from Claude Code's status-line input. A token that can't read that endpoint (one narrowed to inference, as `claude setup-token` makes) has its usage read from the rate-limit headers of a one-token request to the smallest model, as Claude Code's own quota check does.
- **Tokens** of accounts Claude isn't using are refreshed with Claude Code's own OAuth client when they run out; the active one is left to Claude. One file lock keeps every Claude session's status line and hooks from stepping on each other.
- Every change is logged to `~/.cache/claude-accounts/log` (never a token).

## Notes

- It relies on how Claude Code keeps its sign-in and on endpoints Claude Code itself uses, none of them a published API: a Claude Code update can change them. Not affiliated with Anthropic.
- Use it with your own accounts. A subscription is for one person; this tool doesn't share accounts between people.
- Tested on Linux, macOS and Windows (`cargo test`; CI runs all three).

## License

MIT
