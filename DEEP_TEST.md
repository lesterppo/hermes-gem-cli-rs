# Deep test — hermes-gem-cli-rs vs Python original (2026-10-26)

## Verdict

**Build + tests GREEN. Live test BLOCKED on auth, not on code.**
Offline and mock comparisons pass; no live Gemini output exists to
compare, and none was invented.

## Test results

### cargo test (release)

- 10 unit tests pass: auth cookie-header preference, CBC round-trip,
  batchexecute form shape + parse, Gem-id extraction, StreamGenerate
  token extraction / request indices / mock-frame parsing, error
  classification, ISO timestamp shape.
- 1 mock-server end-to-end test passes: init tokens, StreamGenerate
  chat (text/cid/rid/completed), list-gems, list-models, list-chats,
  create-gem, delete-gem — all against a local TCP mock, no live
  session, no real cookies.
- `cargo build --release` succeeds; binary
  `target/release/gem-cli-rs` (~11.8 MB).

### Offline comparison (same VM, same dead session)

| Check | Python original | Rust port | Match |
|---|---|---|---|
| `--help` | full flag list | full flag list (clap) | Flags match; Python also has `--browser` (used only by its browser tier) |
| `--doctor` | `auth_cache_exists:false, live_available:false, selected:null, sources:[]` | identical keys/values | ✅ (different underlying reason: Python lacks browser_cookie3 in this env; Rust cannot decrypt the Chromium GCM cookies — see below) |
| `"not a url" "hi"` | `{"ok":false,"err":"BAD_URL","msg":"Cannot extract Gem ID from: not a url"}` | identical after the ordering fix (BAD_URL now validated before auth) | ✅ |
| bare invocation, no args | argparse usage, exit 2 | usage to stderr, exit 2 | ✅ |
| `gem123 "hello"` (no session) | AUTH_EXPIRED | `{"ok":false,"err":"AUTH_EXPIRED",...}` | ✅ |
| `gem1 "hello"` (4-char id) | BAD_URL (bare id needs ≥5 chars) | BAD_URL | ✅ (found during mock testing) |

### CLI-level mock run (local server, test cookies only)

With `GEMINI_SID=test-sid GEMINI_TS=test-ts` and
`GEMINI_RS_BASE_URL` pointing at `tests/mock_cli_server.py`:

- `--doctor` → `live_available:true, selected:"env"` (test session only).
- Chat `testgem1 "hello"` → pointer
  `{"ok":true,"f":...,"s":22,"model":"default","gem":"testgem1"}`,
  file contains `Hello from mock Gemini`.
- `--list-gems` → system + custom legs, same Gem both sides.
- `--list-chats` → `[{"cid":"c_chat1","title":"Chat one"}]`.
- `--read-chat c_chat1` → oldest-first user/model turn pair.
- `--gem-info testgem1` → external-Gem fallback note (not in library).
- Multi-turn: `--new` → pointer `t:1`; continue with
  `"follow up please"` → `t:2`, file `Mock follow-up reply`;
  `sess.json` holds `cid`, `metadata [cid, rid]`, `turns:2`, `updated`.
- `--create-gem` → id `newgem123`; `--delete-gem` → ok.

Pointer fields (`ok/f/s/model/gem/c/t`) and the conversation-file
shape match the Python contract.

### Live deep test — BLOCKED (evidence, no values)

- `GEMINI_SID`/`GEMINI_TS` unset; `~/.gemini-cli/auth.json` absent.
- Python `--doctor`: 0 candidates, `live_available:false`.
- Rust `--doctor`: 0 candidates, `live_available:false`.
- Chromium profile `~/.gemini-cli/cr-profile` holds Google 1PSID rows,
  but they are v10 AES-GCM blobs and the profile's Local State has no
  `os_crypt.encrypted_key`; the key lives in the system keyring /
  browser session and is unavailable here. CBC-peanuts and
  derived-key GCM decryption fail in-memory. No CDP browser on
  9223/9224. This is **not** proof the Google session is server-side
  dead — only that no decryptable credential is available to this
  environment.
- Unblock: sign in at gemini.google.com in a browser where the
  session lives, then `gem-cli-rs --init` (or set env vars). Do not
  paste raw cookie values into chat.

## Known gaps (also in README parity table)

- File/image upload (`-f`/`-i`): `UPLOAD_NOT_YET`.
- `--login` browser flow: `NOT_YET`.
- `--edit-gem` / `--delete-chat`: `NOT_YET` (payloads unverified live).
- `--stream`: accepted but buffered (no live deltas).
- `--resume`: creates state keyed by cid, metadata not refetched.
- `--deep-research-status`: heuristic (report present vs in progress).
- `--list-models`: best-effort labels from GET_USER_STATUS; `-m`
  labels the pointer but does not yet select a model header.
- CDP cookie tier and Firefox cookie scan: not implemented
  (Chromium profiles only).
- `--timeout-soft`: accepted, hard timeout governs.
- Deep research / image-gen / video / canvas paths are implemented
  from the Python payloads but **unverified against a live session** —
  they need the same unblock step above before they can be called
  live-verified.
