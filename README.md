# Teams MCP

A local Microsoft Teams MCP server written in Rust. Reads use the visible Teams UI; sending messages is explicitly confirmation-gated.

It uses a dedicated persistent Chrome profile for Microsoft authentication. The application never reads passwords, MFA codes, cookies, or Microsoft Graph tokens. You log in through the normal browser UI, and later MCP calls reuse that browser session to inspect the visible Teams application.

## Quick start

```bash
cargo run -- login
cargo run -- status
cargo run -- mcp
cargo run -- http
```

The default MCP transports are:

- stdio: `cargo run -- mcp`
- Streamable HTTP: `http://127.0.0.1:3031/mcp`

HTTP has no built-in authentication and should remain bound to loopback unless an authenticated proxy is placed in front of it.

## Read tools

- `get_teams_overview` — bounded snapshot of the visible Teams page.
- `list_teams` — visible team labels.
- `list_channels` — visible channels, optionally scoped to an exact visible team label.
- `list_chats` — visible chat labels.
- `list_chat_members` — visible users/participants for an exact visible chat label.
- `get_chat_messages` — paginated visible messages for an exact visible chat label. Page 1 is newest; higher pages scroll toward older messages. The default is 10 messages per page.
- `send_chat_message` — preview or confirmation-gated send of a single-line message to an exact visible chat. It never sends unless `confirm: true` is passed.
- `inspect_teams_page` — bounded adapter diagnostics.

The first version intentionally uses the visible Teams UI rather than undocumented network calls or Microsoft Graph credentials. Team, channel, and chat names are UI references, not invented backend IDs. Message results include visible text plus author, author ID, timestamp, message ID, and edited metadata when the page exposes them. Teams is a large SPA and its DOM can change; `inspect_teams_page` and the warnings in results make that behavior observable.

## Browser configuration

The profile is stored in the OS application-data directory under `teams/teams-mcp/browser-profile`.

For local development:

```bash
TEAMS_BROWSER_EXECUTABLE=/path/to/chrome cargo run -- login
TEAMS_CDP_URL=ws://127.0.0.1:9222/devtools/browser/... cargo run -- status
# Reads are headless by default. Set this only when debugging the live UI:
TEAMS_HEADFUL=1 cargo run -- status
```

Use `cargo run -- logout` to remove the saved profile after confirmation.

## Development

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build
```

Sending is the only write operation currently supported. The tool refuses empty, multiline, overlong, ambiguous, or already-drafted sends and uses the visible Teams composer and Send control. File downloads, edits/deletes, and Microsoft Graph integration are deliberately not included.
