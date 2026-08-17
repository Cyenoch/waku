# Waku

Waku is a fast, native desktop app built in Rust with GPUI. It uses a built-in
Rust harness and connects to direct HTTP model endpoints while keeping projects,
sessions, and transcripts on your machine.

[Download Waku](https://waku.sh)

## Providers

Waku uses its built-in Rust harness to connect directly to HTTP model endpoints.
The harness supports OpenAI Responses, OpenAI Chat Completions, and Anthropic
wire formats. Built-in providers include the official OpenAI API, ChatGPT Codex
subscription, Anthropic, OpenCode Zen, OpenCode Go, xAI API, and SuperGrok /
X Premium+ (xAI device OAuth). Their model catalogs are discovered live.

Add custom endpoints in Settings → Providers with an ID, API root URL (including
any required `/v1` path), API format, API-key environment variable, and
non-secret headers. A successful discovery response, including an empty catalog,
is authoritative; discovery errors fall back to the last-good cache or provider
seed. Explicit model IDs on custom endpoints are used only when discovery fails.
Catalog compatibility prevents unsupported model selection, and typed service
tier applies only to supported entries in the official OpenAI API catalogs.
API keys entered in Waku are stored in the macOS Keychain; environment-variable
credentials are read at runtime and never persisted.


## Highlights

- Keep projects and independent agent sessions in one native app.
- Switch configured models and access modes from a shared interface.
- Queue or steer follow-up messages while an agent is working.
- Rewind Git-backed tasks with conversation-aware checkpoints.
- Store app state locally, with no Waku account or remote service required.

## Architecture

The native desktop is an RPC client of the standalone `waku-daemon` process.
Provider sessions run through the built-in HTTP harness in
[`waku-core`](crates/waku-core), behind the authenticated, versioned WebSocket
contract in [`waku-protocol`](crates/waku-protocol). Waku Desktop depends on
[`waku-client`](crates/waku-client), not on the daemon implementation. The
daemon owns task SQLite data, uploaded attachments, harness session snapshots,
an append-only usage ledger, and all workspace filesystem and Git operations;
paths returned by it always refer to the daemon host. The desktop retains only
presentation state and a disposable preview cache. The harness keeps the
generic `Tool` seam for local tools; the removed legacy Computer Use integration
is not part of the runtime.

The browser client lives at [`apps/web`](apps/web) and uses the generated
browser transport in [`packages/waku-client`](packages/waku-client). Its
checked-in types are generated directly from the Rust protocol, while its
WebSocket client implements the same handshake, request IDs, subscriptions,
sequence deduplication, and replay cursors as the Rust client. Run
`bun run protocol:generate` after changing a wire type and
`bun run protocol:check` to verify that generated files are current.

Projectless task workspaces live on the daemon host under
`~/.waku/projects/<date>/<slug>`. The daemon moves workspaces created by the
older `~/.waku/<date>/<slug>` layout on first load.

Configuration ownership is separate too: the Release desktop writes
`~/.waku/app.json`, while Debug stays isolated at `temp/app.json`. Daemon settings live in `~/.waku/settings.json`. The desktop's Settings → Daemon page can explicitly
expose the child daemon on a fixed port, configure exact browser origins, and
copy its stable authentication token. It remains loopback-only by default.

When connected to a daemon managed outside the desktop process, Waku never
interprets daemon paths on the client machine. The local folder picker and PTY
are therefore unavailable until the protocol gains daemon-host picker and
terminal-stream endpoints; files, diffs, Git, skills, usage, task state, and
attachments already use daemon RPC.

Release apps bundle and sign `waku-daemon`. Development keeps the daemon at
`target/debug/waku-debug-daemon`, allowing provider-only edits to rebuild and
replace the daemon without relaunching Waku Debug.

## Development

Development is supported on macOS and Linux and requires
[Rust 1.96 or newer](https://www.rust-lang.org/tools/install) and
[Bun](https://bun.sh/). Linux supports both Wayland and X11; install the native
build prerequisites listed in [CONTRIBUTING.md](CONTRIBUTING.md) first.

```sh
bun install
bun run dev
```

The embedded browser currently remains macOS-only. Agent sessions, projects,
transcripts, skills, usage, diffs, file editing, and the terminal run natively
on Linux.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow and checks.
Release maintainers should also read [RELEASING.md](RELEASING.md).

## Sponsorship

You can support the project development via [GitHub Sponsors](https://github.com/sponsors/egoist).

## License

Waku is licensed under the [GNU General Public License v3.0 only](LICENSE).
