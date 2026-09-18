# Armada RGB

`armada-rgb` controls RGB LEDs exposed through Linux's multicolor or individual
channel LED interfaces. It detects the device-tree model and loads the matching
hardware profile from `/usr/share/armada-rgb/profiles.json`.

The first version supports a solid color, brightness, persistent off, and
restoring the saved configuration:

```text
armada-rgb get
armada-rgb set --color FF8000 --brightness 25
armada-rgb off
armada-rgb apply
armada-rgb supported
```

Settings are saved to `/etc/armada/rgb.json` after the hardware was
updated successfully. Only LED names declared by the matched profile are used.
Profiles using the `channels` backend provide explicit target mappings such as
`red=l:r1`.

Profiles can provide conditional red, green, and blue channel reductions. The
saved correction can be changed with `armada-rgb set --correction`.

## Animated effects

On top of the solid color, an optional animation can be selected. `static` (the
default) keeps the original solid-color behaviour and is omitted when the
configuration is saved, so devices that never select an effect are unchanged.

```text
armada-rgb set --color FF0000 --brightness 40 --effect breathing --speed 150
armada-rgb run
```

Available effects: `static`, `breathing` (base color pulsing), `color_cycle`
(hue sweep, same on every LED), `rainbow` (hue sweep with a per-LED offset),
`load` (hue follows CPU load), `battery` (hue follows charge), and
`screen_sync` (per-side color sampled from the screen content — an
ambilight). `--speed` is a percentage where `100` is the default rate (ignored
by `screen_sync`, which follows live system state instead of a fixed cycle).

Brightness-follows-screen-backlight is **not** in this list — it is the
orthogonal `sync-brightness` modifier below, which composes with *any* of
these effects instead of replacing one.

`armada-rgb run` is the lighting daemon: it keeps the saved configuration
painted, animates the selected effect, and reloads live whenever
`/etc/armada/rgb.json` changes (so a UI writing that file is reflected at once).
Re-asserting the hardware every frame also restores the LEDs after a
suspend/resume that clears the controller. Animated effects use the multicolor
per-LED path; other backends fall back to a uniform color.

The versioned catalog groups exact device-tree model names with a `channels` or
`multicolor` backend, its target list, and an optional default correction.

## Brightness-follows-backlight (`sync-brightness`) — a modifier, not an effect

```text
armada-rgb sync-brightness on
armada-rgb sync-brightness off
```

This is a **toggle on top of whatever color/effect is already configured**,
not a replacement for one — it can be combined with `static`, `rainbow`,
`breathing`, `screen_sync`, anything. When on, `run` reads
`/sys/class/backlight/*/brightness` and `max_brightness` and scales
whatever brightness the current effect just computed (the *configured*
brightness for `static`, the mid-breath value for `breathing`, etc.) by that
percentage, so dimming the screen dims the LEDs proportionally instead of
replacing the brightness the user picked. Color is never touched. This
happens in the one shared write path in `Controller::run` (not inside any
single effect), so it applies uniformly regardless of which effect is active.

Persists as `sync_brightness` in `/etc/armada/rgb.json` and takes effect on
`run`'s next tick (at most ~1s later) without touching color, effect, or
brightness. If more than one backlight device exists (some panels expose both
a named node and a generic `pwm-backlight` wrapper — the Retroid Pocket 6
exposes both `ae94000.dsi.0` and a generic `backlight`), the named one is
preferred by default; set `ARMADA_RGB_BACKLIGHT_NAME` to pin an exact device
name if the wrong one is picked, or `ARMADA_RGB_BACKLIGHT_ROOT` if backlight
devices live somewhere other than `/sys/class/backlight` on a given device. If
no backlight device can be resolved, `run` logs a one-time diagnostic and
falls back to the configured brightness unscaled (never goes dark because of a
missing/renamed node). That RP6 case — one named node plus the generic
alias — is resolved unambiguously (there is exactly one non-generic
candidate); if a device ever exposes *more than one* non-generic candidate
(e.g. a hypothetical dual-panel device), the pick is a genuine guess and
`run` logs a one-time diagnostic naming every candidate and which one it
picked, rather than staying silent about it.

### `screen_sync`

Captures the screen with `gamescopectl screenshot <path>` and paints the
average color of the left half of the screen to the left-side targets and the
right half to the right-side targets (the first half of the profile's target
list is treated as "left", the rest as "right"). Sampling is on a bounded
grid (at most ~64 samples per axis) and gated to a slow, fixed 3-second cadence
— capturing and decoding a screenshot is real CPU/IO work, unlike a sysfs
read, and this keeps it from becoming a background thermal/CPU drain.

`armada-rgb run` is a system service with no graphical session environment of
its own, so it never inherits `XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY` — this
effect always sets them explicitly before invoking `gamescopectl`, defaulting
to `/run/user/1000` and `gamescope-0` (override with
`ARMADA_RGB_GAMESCOPE_XDG_RUNTIME_DIR` / `ARMADA_RGB_GAMESCOPE_WAYLAND_DISPLAY`
if a device differs). If direct env injection cannot reach the session's
Wayland socket on some device, set `ARMADA_RGB_SCREEN_SYNC_USER` to run the
capture through `su - <user> -c '...'` instead (e.g. the session user, so it
runs with that user's environment rather than root's). `gamescopectl` itself
is resolved via `ARMADA_RGB_GAMESCOPECTL_BIN` (default: `gamescopectl` on
`PATH`) and the captured PNG is written to `ARMADA_RGB_SCREENSHOT_PATH`
(default: `/run/armada-rgb/screen-sync.png`, tmpfs, to avoid wearing flash
storage with a capture every few seconds). A capture is bounded by a hard
2.5-second timeout (comfortably under the 3s cadence above, so a slow capture
under load just delays the next tick instead of the two ever overlapping) —
a wedged compositor is killed and reaped, never left running or awaited
indefinitely, so this can never hang the daemon past that timeout either way.
On any failure (no graphical session yet, timeout, decode error) the effect
logs a one-time diagnostic and keeps showing the last successfully sampled
colors (black before the first successful capture) instead of flickering.

## Charging indicator (deep-sleep wake hook)

```text
armada-rgb charge-indicator on [--color FFA500] [--brightness 15]
armada-rgb charge-indicator off
```

During true deep suspend the CPU is powered off, so nothing can drive an
animation — but the LED controller hardware itself keeps holding whatever
value was last written, with no help from software. A brief
wake-on-charge-attach can therefore call `charge-indicator on` to paint a
fixed, non-animated color directly to the hardware right before the system
goes back to sleep; the hardware then holds it through the rest of the
suspend on its own. `charge-indicator on` works even if `run` is not active
(it writes to hardware directly) and, if `run` *is* thawed during the same
brief wake window, pins that color so the daemon defers to it instead of
racing to repaint the normal configuration over it — see `armada-rgb::charging`
for the exact mechanism. It never touches `/etc/armada/rgb.json`: the user's
own configuration is left untouched and is exactly what `charge-indicator off`
restores, immediately (it does not wait for `run`'s next tick, which may not
be running or may still be thawing). The pin lives at
`/run/armada-rgb/charge.json` by default (tmpfs, overridable with
`ARMADA_RGB_CHARGE_PATH`) so a pin left over from a crash mid-suspend cannot
survive a reboot into a fresh session.

## Testing overrides

For tests, the profile catalog and device model paths can be overridden with
`ARMADA_RGB_PROFILES_PATH` and `ARMADA_RGB_MODEL_PATH`. The daemon additionally
honors `ARMADA_RGB_CONFIG_PATH`, `ARMADA_RGB_CHARGE_PATH`, `ARMADA_RGB_SYSFS_ROOT`,
`ARMADA_RGB_STAT_PATH` (CPU load source), `ARMADA_RGB_POWER_ROOT` (battery
source), `ARMADA_RGB_BACKLIGHT_ROOT` / `ARMADA_RGB_BACKLIGHT_NAME` (screen
backlight source), and `ARMADA_RGB_SCREENSHOT_PATH` /
`ARMADA_RGB_GAMESCOPECTL_BIN` / `ARMADA_RGB_GAMESCOPE_XDG_RUNTIME_DIR` /
`ARMADA_RGB_GAMESCOPE_WAYLAND_DISPLAY` / `ARMADA_RGB_SCREEN_SYNC_USER`
(screen capture).
