# Screen-ambient fixes — 2026-10-27

Two user-reported issues with 屏幕取色 (screen-ambient source):

## 1. Washed-out colours ("颜色很淡")

`grade()` multiplied every channel by the gain and clamped each channel at 255
independently. Any bright zone therefore clipped its brightest channels to
white: a bright warm window average like (230,180,160) at gain 2 rendered as
pure white, and the keyboard read as a pale grey wash far duller and colder
than the screen (and than the app preview, which shows the pre-quantization
colour).

The grade now subtracts the black floor, applies the gain, then rescales the
three channels uniformly so the brightest channel lands at 255 at most. Hue and
saturation survive at any gain: the same zone now renders (255,198,175), and a
gain-1 zone is byte-identical to the old behaviour. The firmware's own 200/255
custom-mode ceiling still applies on top (unchanged, hardware-side).

## 2. A "volume breathing" overlay on top of the mirror

`AudioSession.render` flagged the screen renderer stale when no zone packet had
arrived for 350 ms. The shell only emits zone packets when the desktop actually
redraws; on a static screen it sends listening keepalives (1 s) instead. So
every idle moment faded the keyboard toward black (τ≈220 ms) and every tiny
redraw — a blinking cursor, a progress bar, a volume indicator — lit it back
up, which reads exactly like a volume-synced breathing effect layered over the
mirror.

Staleness for screen sessions now tracks the whole packet stream
(`lastPacket`, 2.5 s) instead of zone updates: keepalives prove the capture is
alive, so a static screen holds its colours indefinitely. A capture that stops
sending anything still fades out (and the 6 s session expiry stops playback as
before). While fixing this, the `average` effect was found to paint the raw
zone mean without ever decaying it — the fade now decays `average` alongside
the smoothed grid so all three screen effects darken on a lapsed capture.

## Tests

- `screen-ambient.test.mjs`: gain keeps a bright warm zone saturated instead of
  clipping to white; a static screen with fresh keepalives holds its colour
  while a lapsed capture fades (AudioSession-level).
- Suite: 361 tests, 350 passed, 11 pre-existing platform skips, 0 failed.
