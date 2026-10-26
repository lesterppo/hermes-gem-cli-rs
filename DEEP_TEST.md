# Deep test — hermes-gem-cli-rs vs Python original (2026-10-26)

## Verdict

**Build + tests GREEN. Live test PASSED 2026-10-26 evening (see below).**
Offline and mock comparisons pass, and the full binary now runs live
against Gemini with the same prompts as the Python original.

## Live test (2026-10-26, ~20:22–20:24 HKT, full binary, env SID/TS)

Transport: plain reqwest/rustls could NOT reach gemini.google.com
from this VM (hyper `IncompleteMessage`; curl_cffi and Chromium
succeeded). The binary now uses leyline-http 0.1.6 (Chrome 154
profile, BoringSSL built from source) via a tokio Runtime wrapper;
the public API stays blocking and protocol.rs is unchanged except
for the slot-41 fix below. rquest/reqwest-impersonate were NOT used:
every version of both is yanked on crates.io, as is their
boring-imp backend.

- `--doctor`: `live_available:true, selected:"env"`, rc 0, 1.26s
  (previously `live_available:false` under reqwest).
- `--list-gems`: rc 0, 2.73s, 7 system Gems (storybook, chess-champ,
  brainstormer, career-guide, coding-partner, learning-coach,
  writing-editor). Python lists 6 (no chess-champ) — divergence
  noted, not hidden.
- `--list-models`: rc 0, `[]` vs Python `["gemini-自動"]` — known
  best-effort gap: the live payload's model is named "Auto", which
  Rust's `gemini-` string search does not match.
- Gem chat, Storybook, same prompts as Python: turn 1 "The capital
  of France is Paris." (10.85s, 31 chars, identical to Python);
  multi-turn via `-c` state file: turn 2 "The Seine river flows
  through Paris.", same conversation id, t2 (18.72s). Python's
  turn 2 also gave the Seine, with a YouTube link this run did not
  repeat — Gemini wording varies run to run; the core answers match.
- `--list-chats` (MaZiqc) returns BardErrorInfo 1041 live — the
  RPC id/payload is stale in gemini-webapi 2.1.1, which does not
  offer list-chats either, so this is not a Python regression.

Protocol bug found and fixed live: StreamGenerate slot 41 must be
`[1]`, not scalar `1` — that alone caused HTTP 400. Isolation test
proved it: with slot 41 fixed, the old header `["",1]`, hl=en and
reqid 100000 all still work. `cargo test --release` still 11/11
(10 unit + 1 mock e2e) after the transport swap.

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

## Replication pass, 2026-10-26 evening (all Python gem-cli.py functions)

On Peter's "replicate all functions of the Python version", the
gaps above were closed and live-tested (cargo test 13/13: 12 unit +
1 mock, incl. a Firefox-scan unit test with a fake profile):

- File/image upload (`-f`/`-i`): content-push multipart upload +
  StreamGenerate file_data slot. Live: text file -> "BANANA" and
  "BANANA[cite: 1]"; lighthouse PNG -> correct one-sentence
  description. (First upload attempt answered a Chinese refusal;
  two immediate retries both returned BANANA — a Gem flake, not an
  attachment bug, since the same code path then worked twice.)
- `--list-models`: now parses GET_USER_STATUS exactly as
  gemini-webapi's AvailableModel (payload [15]/[16]/[17],
  capacity rules, name/alias derivation). Live: Rust
  `["gemini-自動"]` == Python `["gemini-自動"]`. The earlier
  `gemini-auto` mismatch was a language bug: Rust hardcoded `hl=en`
  for batchexecute while Python uses the session language (TuX5cc);
  this account is Chinese, so model names localise. Fixed: hl and
  the StreamGenerate URL now use the session language.
- `-m` resolution: registry lookup by id/name/display/alias, with
  the model header (x-goog-ext-525001261-jspb) + slot 79 sent on
  chat; unknown names fail listing the available models, as Python
  does. Live: `-m gemini-自動` chat -> "The capital of France is
  Paris." `-m` + `--thinking` falls back to the default when the
  account lacks that tier's model, as Python's enum path does.
- `--create-gem` / `--edit-gem` / `--delete-gem`: live, 5.3s / 7.3s /
  4.0s; info after delete shows the external (not-in-library) form.
  Diagnosis note: create-gem "hung" three times before the fix —
  not the RPC (a probe binary created a gem in 4.5s) but the CLI's
  stdin check: the old /dev/stdin metadata test read stdin on `-p`
  whenever stdin wasn't a character device, blocking forever under
  a driver whose stdin never EOFs. Fixed with `IsTerminal`
  (Python's `isatty` equivalent).
- `--delete-chat`: live rc 0. Read-back still returns the turns —
  Python's delete_chat behaves identically (deletion removes the
  chat from history, not from the turns endpoint), so this is
  parity, not a bug.
- `--login`: implemented (best-effort xdg-open + 120s poll of the
  Chromium/Firefox/Edge scans, cache saved on success). NOT
  live-tested end-to-end: it needs a person signing in interactively.
- Firefox scan: plaintext `moz_cookies` sqlite, unit-tested; Edge
  (Chromium profile) added; `--browser` / `GEMINI_BROWSER` now
  orders the scan (Python order chrome, firefox, edge; Safari has
  no Linux store and is skipped).
- `--create-gem` stdin prompt sources now match Python (-p + stdin,
  positional, stdin alone).
- Rate-limit retry now sleeps 30s / 60s like Python (not
  live-triggered; burning quota to test it would be wasteful).
- `--deep-research`: full Python flow — plan turn, confirm turn
  ("Proceed with this plan without any modifications."), then poll
  read_chat every 15s until a long report turn lands. Live test
  with a 180s cap: plan + confirm both landed, poll hit the cap
  (`dr_timeout: true`, confirm text returned, task continues
  server-side). A full report landing was not observed in that
  window; it needs the 600s default run or a later re-read.

## Remaining gaps (also in README parity table)

- `--deep-research-status`: heuristic (report present vs in progress).
- `--timeout-soft`: accepted, hard timeout governs — identical to
  Python, which also accepts it without using it.

## Gap-fix pass, 2026-10-26 21:40+ HKT (on Peter's "fix the
remaining gap")

- `--stream`: now REAL streaming, not buffered. `chat_stream`
  uses leyline's decoded body stream and re-parses after each
  chunk, emitting only the new text suffix (gemini-webapi's
  text_delta rule). Live: Storybook "capital of France" streamed
  to stdout, then the pointer JSON on its own line, rc 0.
  (Python gem-cli.py has no --stream flag at all, so this was a
  Rust-only extra, now genuinely streaming.)
- CDP cookie tier: implemented from Python gemini.py
  `_cdp_cookies` (GEMINI_CDP_URL / GEM_PW_CDP / 127.0.0.1:9223,
  GEMINI_NO_CDP to disable, 8s total timeout, /json/version then
  WebSocket Network.getAllCookies via tokio-tungstenite, no
  Playwright needed). Ordered after local browsers, as in Python
  (a CDP profile can hold a dead session). Unit test: NO_CDP
  short-circuits. Live: with no CDP browser running, `--doctor`
  still completes in 1.01s (connection refused instantly, no hang)
  and shows no cdp candidate.
- `--resume`: NOT a gap after all — Python's `--resume` also
  builds state from the cid alone (`metadata = [cid, ""]`) without
  refetching; Rust matches exactly.
- `--login`: still not live-tested end-to-end (needs a person
  signing in interactively; the polling code is implemented and
  shares the tested browser scans).
- Deep research, the real bug: the first 600s-cap run (21:45 HKT,
  "history of the printing press", cid c_0463649b40da151d) hit
  `dr_timeout` again — but a re-read showed the research had
  actually completed ("I've completed your research."). The
  report is NOT reply text: Python gemini-webapi
  `extract_deep_research_document` reads it from
  candidate[30][0] (title [2], task id [3], markdown body [4]).
  The Rust poll only looked at text length, so it could never
  fire. Fixed: `read_deep_research_document` (candidate =
  turn[3][0][0], i.e. Python's candidates_list[0]) + poll on a
  non-empty document body, with a unit test for the extractor.
  Verified against the completed chat: DOC_OK, title "History
  of Printing Press", 28,210 chars. (A first probe returned
  DOC_NONE from a stale binary; the skeleton dump proved the
  document exists, and the rebuilt probe returned it.)
  End-to-end CLI re-run on a fresh topic is in progress to
  confirm the poll returns the report instead of timing out.
- Deep research, second fix (same session): run #2 (bicycle,
  with Storybook Gem passed into plan/confirm) was refused by
  the Gem itself ("I'm a text AI, this is beyond my
  capability") — research never started, so the timeout was
  correct, not a poll bug. Python's plan/confirm run WITHOUT
  a Gem, so the Rust flow was changed to match (the CLI still
  validates the Gem URL as its entry point). Run #3 (bicycle,
  no-Gem plan/confirm, 22:22 HKT): rc 0 in 364.27s, NO
  dr_timeout, output 29,667 bytes — a full cited markdown
  report ("The Technological, Socio-Economic, and Mechanical
  Evolution of the Bicycle", Tambora 1815 → Year Without a
  Summer → horse shortage). Deep research is now fully
  verified end-to-end.
