//! claude-accounts: several Claude subscriptions in Claude Code, on Linux,
//! macOS and Windows. See README.md.

mod accounts;
mod title;
mod util;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(accounts::command_line(&args));
}
