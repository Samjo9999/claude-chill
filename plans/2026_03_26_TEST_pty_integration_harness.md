# PTY Integration Test Harness

## Goal

Build a test infrastructure that exercises the proxy's I/O pipeline with real PTY pairs and predictable child processes, covering scenarios the unit tests and expect script can't reach.

## Architecture

### Mock child binary: `pty-mock`

Single binary in a new crate (`crates/pty-mock`) with clap subcommands. Each subcommand is a deterministic child process that exercises a specific proxy behavior. The binary reads from stdin and writes to stdout — when spawned by the proxy through a PTY, this becomes the PTY slave side.

Subcommands:

- `echo` — reads stdin line by line, writes each line back. Baseline for verifying I/O passthrough and history recording.
- `alt-screen` — enters alt screen on startup, writes content, waits for a trigger byte on stdin, exits alt screen, writes more content. Tests alt-screen state tracking.
- `paste-echo` — enables bracketed paste mode (`\x1b[?2004h`), reads paste input, writes a known response. Tests paste pipeline.
- `buffer-fill` — writes large output continuously while reading stdin slowly. Forces the drain path to activate. Tests `write_to_pty_draining` and the drain buffer flowing through `process_output`.
- `sync-blocks` — wraps all output in synchronized update blocks (`\x1b[?2026h` ... `\x1b[?2026l`). Tests sync block tracking and history segmentation.
Note: split paste end marker testing doesn't need a mock child subcommand — it's an input-side issue. We split the marker across two `process_input` calls from the test harness.

### Test harness: integration tests in `crates/claude-chill/tests/`

Rust integration tests that:

1. Open a PTY pair manually (`openpty`)
2. Spawn `pty-mock <subcommand>` on the slave side
3. Construct a `Proxy` with the master fd (requires a new constructor or builder that skips terminal setup, kitty detection, and stdin/stdout — since we provide our own fds)
4. Drive the proxy's event loop from the test, feeding input and collecting output
5. Assert on proxy state: history contents, `in_alternate_screen`, `in_bracketed_paste`, etc.

### Proxy refactoring needed

`Proxy::spawn` currently does too much: opens PTY, sets up terminal, detects kitty, spawns child, constructs self. For testing we need to separate construction from terminal/child setup.

Options (in order of preference):

**Option A: `Proxy::new_for_test`** — a `#[cfg(test)]` constructor that takes a pre-opened PTY master fd, a pre-spawned Child, and a ProxyConfig. Skips terminal guard, kitty detection, bracketed paste enable. Minimal refactoring, no production code changes.

**Option B: Builder pattern** — `ProxyBuilder` that lets you override each component. More flexible but more code for something only tests use right now.

**Option C: Extract an event loop runner** — Instead of `run()` using real stdin/stdout, expose `process_input`/`process_output` as pub (they already are `fn`, just need `pub`) and let tests call them directly with fake fds. This avoids needing a full event loop in tests.

Recommendation: **Option A + Option C combined.** The test constructor gives us a Proxy with a real PTY to a mock child. Making `process_input`/`process_output` pub(crate) lets tests drive them directly without needing to replicate the poll loop. We also need pub(crate) accessors for state we want to assert on (history, in_alternate_screen, etc.).

Stretch goal: make `Proxy::spawn` work in tests by running inside a PTY-within-a-PTY. Would let us exercise the full startup path including terminal guard and kitty detection. Not needed initially.

## Implementation Steps

### Step 1: Create `crates/pty-mock` crate
- Add to workspace
- clap with subcommands
- Start with `echo` subcommand only
- Use raw stdin/stdout (no buffering surprises)

### Step 2: Add test constructor to Proxy
- `Proxy::new_for_test(pty_master: OwnedFd, child: Child, config: ProxyConfig) -> Self`
- `#[cfg(test)]` gated
- Skips terminal guard, kitty detection, signal handlers, bracketed paste enable
- Hardcodes kitty_supported=false, kitty_initial_stack=0

### Step 3: Expose proxy internals for test assertions
- `pub(crate)` accessors:
  - `history()` -> &LineBuffer
  - `in_alternate_screen()` -> bool
  - `in_bracketed_paste()` -> bool
  - `in_sync_block()` -> bool
  - `in_lookback_mode()` -> bool

### Step 4: Write first integration test (echo baseline)
- Open PTY pair
- Spawn `pty-mock echo` on slave
- Construct Proxy with master fd
- Feed input bytes via `process_input`
- Read output from PTY, feed through `process_output`
- Assert: history contains expected lines, output matches expected bytes

### Step 5: Add `alt-screen` subcommand + test
- Mock child enters alt screen, writes content, exits on trigger
- Test: verify `in_alternate_screen` tracks correctly, history is correct after exit

### Step 6: Add `paste-echo` subcommand + test
- Test normal paste flow
- Test split paste end marker: drive stdin with `\x1b[201~` split across two `process_input` calls
- Assert: `in_bracketed_paste` returns to false, subsequent input goes through lookback

### Step 7: Add `buffer-fill` subcommand + test
- Mock child floods output while test feeds large input
- Assert: drain buffer gets processed through `process_output`, no output lost, history is populated

### Step 8: Add `sync-blocks` subcommand + test
- Mock child wraps output in sync blocks
- Assert: history records complete sync blocks correctly

## Open Questions

- `pty-mock` is a separate crate (`crates/pty-mock`) to avoid bloating the main binary. Cargo builds it automatically when referenced as a dev-dependency.
- Do we need a timeout/watchdog in the test harness? If a mock child hangs, the test hangs. A simple `Duration`-based timeout on reads should suffice.
- The `process_input`/`process_output` approach bypasses the poll loop. Should we also have a test that exercises the full `run()` loop? Maybe later — the expect script already covers that at a high level.
