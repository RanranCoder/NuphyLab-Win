# Side-light linkage fix — 2026-10-26

## Problem

On the Air75 V3, the 20 side-strip LEDs (outside the main key area) are driven
by lighting-state bytes 9–16, independent of the main 84-key channels. Starting
NuPhyLab and connecting to the keyboard turned the strip off; only a keyboard
reset brought it back. The Caps indicator kept working because the firmware
drives indicator LEDs separately.

## Root cause

Two stacked causes:

1. The user's settings had Codex agent lighting enabled. Since v1.0.11 agent
   lighting is "exclusive": every connect (auto-connect included) ran
   `startAgentLighting()` → `muteGameSide()`, which wrote the side zone to
   static black at brightness 0 through a D6 state write. The strip stayed dark
   for the whole session because agent mode never un-mutes.
2. Game, trigger and audio modes shared the same mute; only explicit scene
   transitions restored the strip.

## Fix

The mute system is replaced by frame-following linkage (`server/main.mjs`):

- `output()` now evaluates every painted (or unchanged) frame and, when the
  frame contains lit pixels, syncs the side strip to the frame's average lit
  colour at a brightness that tracks the frame's luma
  (`syncSideFrame`/`frameSideTarget`). Writes are throttled to one per 600 ms,
  quantized by colour/brightness buckets, and ride the normal serialized
  hardware queue after the paint.
- An all-dark frame writes nothing: the board's own side lighting keeps showing
  through. Only the explicit sleep blackout writes darkness (`lightPower`
  sleeping). Connecting with an idle agent session therefore leaves the strip
  exactly as the keyboard had it.
- The strip returns to the journaled original configuration through the
  existing restore path on stop, disconnect and exit.
- Quota-side users keep the old contract: whenever the mode allows quota writes
  (everything except game/trigger/audio/agent), quota still owns the strip and
  the sync stays idle.
- The Caps indicator is untouched: the sync writes side-zone state bytes 9–16
  only, and indicator LEDs are firmware-driven at a higher priority.

## New protocol operation

`sideSync {color, brightness}` in `native-win/hidbridge/src/ops.rs` →
`Air75Device::sync_side` (`device.rs`): armed-only, refreshes the lighting
profile, patches side bytes 9–16 onto a fresh D5 read, skips when the board
already reports the target (`states_match`), then writes D6 once and checks the
echo. Unlike `set_side` it performs no sleep/readback verification and never
rolls back — a missed sync write is corrected by a later frame, and sync
failures are cosmetic by contract. The full-verified `side` op remains for
quota and restore-side operations.

## Tests

- `tests/side-sync.test.mjs` (new): connecting agent lighting with nothing to
  show performs zero side writes and leaves brightness 65 (fake original);
  a running task links `#304ffe` to the strip; static artwork syncs and does
  not rewrite unchanged frames; stopping restores the original configuration;
  an enabled quota colour keeps ownership.
- Updated expectations in `lighting-power-service` (sleep blacks the strip via
  sync, wake relights it with the artwork colour), `main-recovery`
 (arm → paint → sideSync ordering after rollback/reconnect; paused-game
  reconnect keeps the strip linked), `arcade-service`, `codex-whole`,
  `lighting-ownership`, `trigger-service` (black wait frames leave the board's
  own side lighting showing; flashes link the strip), `bluetooth-input`.
- `tests/helpers/fake-service.mjs` models `sideSync`.
- Native self-test `test_side_sync`: unarmed refusal, static-mode/brightness/
  colour write with main and preset fields preserved, skip on unchanged zone,
  exact original restoration afterwards (included in the 800+ offline checks).
- Node suite: 359 tests, 348 passed, 11 pre-existing platform skips, 0 failed.

## Physical verification (Air75 V3 ANSI, firmware 1.0.23.6, Win profile)

Replaced `bin/HIDBridge.exe`, restarted the app with Codex lighting enabled:
connect restored the journaled original side configuration (brightness 60, red)
and performed no mute write; live agent task frames moved the strip to static
mode with the frame's average colour and luma-tracked brightness; the session
journal remains owned until exit, restoring the original strip configuration.
