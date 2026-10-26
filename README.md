# hermes-gem-cli-rs

Rust port of [lesterppo/hermes-gem-cli](https://github.com/lesterppo/hermes-gem-cli)
("Gemini Gem CLI via WebAPI — chat, Gem chat, multi-turn conversation,
image generation, deep research, Gem CRUD, no API key").

The Python original wraps the `gemini-webapi` library plus a
browser-shaped batchexecute client for read RPCs. This crate reimplements
that WebAPI protocol directly in Rust: init-token extraction from
`gemini.google.com/app` (`SNlM0e` / `cfb2h` / `FdrFJe`), StreamGenerate
chat (the 81-slot request from gemini-webapi's `_generate`), and
batchexecute RPCs (`LIST_BOTS`, `GET_USER_STATUS`, `LIST_CONVERSATIONS`,
`LIST_CONVERSATION_TURNS`, `CREATE_BOT`, `DELETE_BOT`).

## Why Rust

Single binary, millisecond startup, no `gemini-webapi` / `browser-cookie3`
venv to bootstrap, and typed handling of the batchexecute payloads.
The output contract is unchanged: the full response goes to a file and
stdout is a compact JSON pointer, e.g.
`{"ok":true,"f":"./out.md","s":1234,"model":"flash","gem":"GemName"}`.

## Install

```bash
cargo build --release
# binary: target/release/gem-cli-rs
```

Rust 1.99+ recommended. The client loads native TLS roots plus
`/etc/ssl/certs/hatch-egress-ca.pem` when present (needed behind the
TLS-intercepting proxy on the author's VM), and strips bracketed IPv6
entries from `NO_PROXY` at startup.

## Auth

Auth tiers, in the same order as the Python original:

1. `GEMINI_SID` + `GEMINI_TS` env vars
2. `~/.gemini-cli/auth.json` (`__Secure-1PSID` / `__Secure-1PSIDTS`)
3. Chromium cookie stores (v10 CBC-peanuts decryption, plus AES-GCM
   when a Local State key is present)

```bash
gem-cli-rs --init     # validate + cache the current session to auth.json
gem-cli-rs --doctor   # which sources exist, which are live (no values printed)
```

Cookie values are held in memory only and are never printed, logged,
or committed. `auth.json` and session files are git-ignored. If no
tier validates against `/app` (no `SNlM0e` in the page), every
authenticated command fails with `AUTH_EXPIRED` and tells you to
re-sign in at gemini.google.com — do not paste raw cookies into chat.

Known auth pitfall, inherited from the Python original: a cookie pair
can be *chat-valid* while being *read-invalid* — StreamGenerate may
accept it while SNlM0e-gated calls (Gem CRUD, `--list-gems`,
`--list-chats`, account status) are rejected. Also never mix cookie
jars: a 1PSID from one source combined with a 1PSIDTS from another is
rejected as signed-out even when both halves are valid alone. This
port keeps one source's jar together (SID/TS pair wins over jar
entries of the same name).

## Usage

```bash
gem-cli-rs "<gem-url-or-id>" "prompt"            # Gem chat
gem-cli-rs "<gem-url-or-id>" -m flash --brief --raw "prompt"
gem-cli-rs "<gem-id>" -c sess.json --new "start" # multi-turn
gem-cli-rs "<gem-id>" -c sess.json "continue"
gem-cli-rs "<gem-id>" -c sess.json --clear       # delete conversation
gem-cli-rs "<gem-id>" --img "a flying cat"       # image generation
gem-cli-rs "<gem-id>" -m pro --deep-research "q" # deep research (600s timeout)
gem-cli-rs --list-models
gem-cli-rs --list-gems
gem-cli-rs "<url>" --gem-info
gem-cli-rs --create-gem "Name" -p "instructions"
gem-cli-rs --delete-gem <id>
gem-cli-rs --list-chats
gem-cli-rs --read-chat <cid>
gem-cli-rs --fetch-latest <cid>
gem-cli-rs --deep-research-status <cid>
gem-cli-rs "<gem-id>" --extract-code 1 "prompt"
gem-cli-rs "<gem-id>" --extract-canvas out.html "prompt"
```

Flags mirror the Python CLI: `-p/--prompt-flag`, `-m/--model`,
`--thinking standard|plus|extended`, `-o/--output`, `--json-out`,
`--brief`, `-q/--quiet`, `--raw`, `-t/--timeout` (default 120s,
auto-extends to 600s for deep research), `--no-retry`, `--temporary`,
`--show-thoughts`, `--save-videos DIR`, `--save-media DIR`,
`--resume <cid>`, `--stream` (see parity note).

Error codes mirror the Python original: `AUTH_EXPIRED`, `RATE_LIMIT`
(`retry_after_s`), `TIMEOUT` (`timeout_s`), `GEN_FAILED`, `BAD_URL`,
`NO_URL`, `NO_PROMPT`, `NO_CONV`, `GEM_INFO_FAILED`,
`GEM_CREATE_FAILED`, `GEM_DELETE_FAILED`, `LIST_FAILED`,
`LIST_CHATS_FAILED`, `READ_CHAT_FAILED`, `FETCH_LATEST_FAILED`,
`BAD_CODE_INDEX`, `RETRY_EXHAUSTED`, plus `NOT_YET` /
`UPLOAD_NOT_YET` for unimplemented paths (below).

## Parity table

Legend: ✅ Implemented · ◐ Partial · ⏳ Not yet (reason given, never faked).

| Command / flag | Status | Notes |
|---|---|---|
| Direct chat, Gem chat | ✅ | StreamGenerate, 81-slot request, pointer JSON |
| Multi-turn `-c sess.json`, `--new`, `--clear` | ✅ | Same JSON state file (`metadata`/`cid`/`turns`) |
| `--resume <cid>` | ◐ | Creates state with the cid; metadata is not refetched from history |
| `--list-models` | ✅ | GET_USER_STATUS parsed as gemini-webapi AvailableModel (name/alias/header); live `["gemini-自動"]` == Python |
| `-m/--model`, `--thinking` | ✅ | Registry resolution + model header + slot 79; unknown names fail listing available models, as Python does |
| `--list-gems`, `--gem-info` | ✅ | Browser-shaped LIST_BOTS, system + custom |
| `--create-gem`, `--delete-gem` | ✅ | oMH3Zd / UXcSJb, live-verified 2026-10-26 |
| `--list-chats`, `--read-chat`, `--fetch-latest` | ✅ | MaZiqc / hNvQHb, oldest-first turns |
| `--deep-research`, `--deep-research-status` | ✅ / ◐ | Full Python flow (plan turn, confirm turn, poll for the report); status remains a heuristic (report present in chat vs in progress) |
| `--img`, `--img-gen` | ✅ | Via the chat path (`Generate an image: …`); generated-image URLs parsed from rich content |
| `--extract-code`, `--extract-canvas` | ✅ | Code blocks / HTML artifact from response text |
| `--save-videos`, `--save-media` | ✅ | Downloads parsed media URLs (unverified live) |
| `--temporary`, `--show-thoughts`, `--brief`, `--raw`, `--quiet`, `--json-out`, `--no-retry`, `-t` | ✅ | |
| `--stream` | ✅ | Real streaming via leyline decoded body stream, text-delta emission (Rust-only flag; Python gem-cli.py has no --stream) |
| `--timeout-soft` | ◐ | Accepted for CLI compatibility; the hard `-t` timeout governs |
| `--doctor`, `--init` | ✅ | Doctor reports existence/liveness only, never values; `--init` validates and caches the current session from env/auth.json/Chromium/Firefox/Edge |
| Firefox / Edge cookie scan (browser tier) | ✅ | Firefox plaintext sqlite + Edge Chromium profile; `--browser` / `GEMINI_BROWSER` orders the scan (Python order: chrome, firefox, edge; Safari has no Linux store) |
| `-f/--file`, `-i/--image` upload | ✅ | Content-push multipart + StreamGenerate file_data; live-verified with a text file (BANANA) and a PNG (lighthouse description) |
| `--login` (browser flow) | ◐ | Implemented (xdg-open + 120s cookie-scan poll); not live-tested end-to-end — it needs a person signing in |
| `--edit-gem`, `--delete-chat` | ✅ | UPDATE_BOT_METADATA / DELETE_CONVERSATION, live-verified 2026-10-26 (delete-chat removes from history; turns stay readable, exactly as Python) |
| CDP cookie tier (`GEMINI_CDP_URL`) | ✅ | /json/version + WebSocket Network.getAllCookies via tokio-tungstenite, after local browsers (Python gemini.py ordering); GEMINI_NO_CDP disables it |
| Chromium GCM cookies without a Local State key | ⏳ | Decryption needs the profile's key (keyring); when unavailable the tier yields nothing and auth falls through to re-sign-in |

## Live-test status (2026-10-26)

**BLOCKED on auth, not on code.** Evidence (no cookie values involved):

- `GEMINI_SID` / `GEMINI_TS` unset; `~/.gemini-cli/auth.json` does not exist.
- The Python original's own `--doctor` reports zero candidates and
  `live_available: false` (its `browser_cookie3` tier is not installed
  in this environment, so it cannot scan at all).
- The Chromium profile (`~/.gemini-cli/cr-profile`) does hold Google
  1PSID cookie rows, but they are v10 AES-GCM blobs and the profile's
  Local State carries no `os_crypt.encrypted_key`, so the decryption
  key is not available here (it lives in the system keyring / browser
  session). CBC-peanuts and GCM-with-derived-key decryption both fail
  in-memory tests; no CDP browser is listening on 9223/9224.
- The earlier `NOT_SIGNED_IN` report for the Gemini web profile is
  consistent with the above.

What was verified instead: `cargo test` (unit + a local mock-server
end-to-end test covering init tokens, StreamGenerate chat, list-gems,
list-models, list-chats, create/delete Gem) and the offline paths
(`--help`, `--doctor` reporting no live session, `AUTH_EXPIRED` for
authenticated commands without a session, `BAD_URL`/`NO_PROMPT`).
Mock outputs are never presented as live results.

**Live test update (2026-10-26 evening):** the full binary now runs
live — transport is leyline-http (Chrome 154 profile; plain
reqwest/rustls could not reach gemini.google.com from this VM), and
a StreamGenerate slot-41 bug (`1` → `[1]`) found live is fixed.
`--doctor` reports `live_available:true`, Storybook chat and its
same-conversation follow-up match the Python original (Paris /
Seine). Full numbers in DEEP_TEST.md. To run live yourself: sign in
at gemini.google.com in a browser on a machine where the session
lives, then either set `GEMINI_SID`/`GEMINI_TS` for the run or run
`gem-cli-rs --init` after the browser session is available to this
VM. Do not paste raw cookie values into chat.

## Testing

```bash
cargo test --release   # unit tests + mock-server end-to-end test
# GEMINI_RS_BASE_URL=http://127.0.0.1:PORT gem-cli-rs --doctor   # test-only base override
```

`GEMINI_RS_BASE_URL` is a test-only override; production use always
talks to `https://gemini.google.com`.

## License

MIT — see LICENSE. Original Python implementation:
https://github.com/lesterppo/hermes-gem-cli
