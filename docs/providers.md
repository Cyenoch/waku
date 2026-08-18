# Provider endpoints

Waku uses the built-in Rust harness to connect directly to configured HTTP
model endpoints. It supports OpenAI Responses, OpenAI Chat Completions, and
Anthropic wire formats. It never launches, detects, installs, or authenticates
third-party CLIs.

## Built-in providers

The daemon always knows these identities. Sign in from Settings; provider APIs
discover their model catalogs live.

| ID | Auth | Base URL |
| --- | --- | --- |
| `openai-responses` / `openai-chat` | Official OpenAI API key (`OPENAI_API_KEY`) | `https://api.openai.com/v1` |
| `openai-codex` | ChatGPT subscription OAuth: browser PKCE with loopback `:1455`, or device fallback | `https://chatgpt.com/backend-api/codex` |
| `anthropic` | API key (`ANTHROPIC_API_KEY`) | `https://api.anthropic.com/v1` |
| `opencode-zen` | API key; stored separately from Go (`OPENCODE_API_KEY` environment fallback) | `https://opencode.ai/zen/v1` |
| `opencode-go` | API key; stored separately from Zen (`OPENCODE_API_KEY` environment fallback) | `https://opencode.ai/zen/go/v1` |
| `xai` | xAI API key (`XAI_API_KEY`) | `https://api.x.ai/v1` |
| `xai-oauth` | SuperGrok / X Premium+ device OAuth (`XAI_OAUTH_TOKEN` environment fallback) | `https://api.x.ai/v1` |

Codex browser and device verification pages, and xAI device verification,
open through the system default browser only. `XAI_API_KEY` never authenticates
SuperGrok. Go and Zen never share a stored key.

## Custom endpoints

Open **Settings → Providers** for a configurable endpoint:

- **ID**, **Name**, and **Base URL** (including `/v1`)
- **API format**: OpenAI Responses, OpenAI Chat Completions, or Anthropic
- **API-key environment**: daemon environment-variable name; Waku never stores the secret
- **Headers**: non-secret only
- **Models**: explicit IDs used only after live discovery returns an error

## Discovery and selection

Sessions persist `ProviderId` and model ID. Provider APIs are queried at
`GET {base}/models` (Codex uses its catalog envelope). A successful discovery
response, including an empty catalog, is authoritative. Discovery errors use
the last-good cache, or an applicable provider seed; custom endpoint model IDs
are the fallback only after that discovery error. Catalog entries carry
compatibility metadata, so unsupported models remain unavailable and cannot be
selected. Typed service tier is available only for supported entries in the
official OpenAI API catalogs, not merely because an endpoint speaks an OpenAI
format.

## Secrets

API keys and OAuth refresh tokens entered in Waku are stored in a daemon-owned
`credentials.json` file under the application data directory. On Unix the parent
directory is owner-only (`0700`) and the file is owner read/write (`0600`).
Environment-variable credentials are read at runtime; `settings.json` never
contains tokens or keys. Existing Keychain items are not read or migrated;
sign in again after this cutover.

## Safety

The default access mode is **Ask**. Legacy access values are migrated safely:
removed `Auto` becomes Ask, and legacy combined Plan becomes Ask plus
Interaction Plan. Users may explicitly choose Auto-accept edits or Full access
for a session. Missing fields never imply full access.
