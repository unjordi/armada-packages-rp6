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
average color of the left EDGE band of the screen to the left-side targets and
the right edge band to the right-side targets (the first half of the profile's
target list is treated as "left", the rest as "right"). The capture is a **raw
NV12 buffer** (`<path>.nv12.bin`): the extension tells gamescope to write its
native pixel format with no PNG encode, and the daemon reads luma/chroma
straight out of the raw Y and interleaved U/V planes — no image decode. This is
the efficiency fix (armada#27): the old path wrote a PNG (compositor-side
encode) and decoded the whole ~2 MP frame just to average a few edge columns.
Measured on the RP6 (Game Mode, 1920×1080): NV12 capture ~261 ms wall vs PNG
~646 ms, and the 2 MP PNG decode is gone entirely (the `image` crate dependency
was dropped). Sampling is still on a bounded grid (at most ~64 samples per axis)
of only the outer edge band, and gated to a slow, fixed 3-second cadence so
even the compositor-side readback stays a negligible background cost.

The NV12 geometry is validated against the actual file size before sampling; if
it does not match the configured output resolution (e.g. a resolution change or
external display), the effect falls back to the base color rather than sampling
garbage. Defaults are 1920×1080 with 4096-byte page-aligned planes (the RP6's
Game Mode output); override with `ARMADA_RGB_SCREEN_WIDTH` /
`ARMADA_RGB_SCREEN_HEIGHT` / `ARMADA_RGB_NV12_PLANE_ALIGN` if a device differs.

`armada-rgb run` is a system service with no graphical session environment of
its own, so it never inherits `XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY` — this
effect always sets them explicitly before invoking `gamescopectl`, defaulting
to `/run/user/1000` and `gamescope-0` (override with
`ARMADA_RGB_GAMESCOPE_XDG_RUNTIME_DIR` / `ARMADA_RGB_GAMESCOPE_WAYLAND_DISPLAY`
if a device differs). If direct env injection cannot reach the session's
Wayland socket on some device, set `ARMADA_RGB_SCREEN_SYNC_USER` to run the
capture through `runuser -u <user> -- ...` instead (e.g. the session user, so
it runs with that user's environment rather than root's). Deliberately
`runuser`, not `su -`/`su -l`: a login shell opens a brand-new PAM/logind
session on every call, and at the 3s screen_sync cadence that floods logind
badly enough to starve out the real Game Mode session (confirmed on-device:
~34 session-opens/90s, Game Mode failed to start until the flood was
stopped). `runuser -u <user> --` execs the target directly as that user with
no login/PAM session at all. `gamescopectl` itself
is resolved via `ARMADA_RGB_GAMESCOPECTL_BIN` (default: `gamescopectl` on
`PATH`) and the captured NV12 buffer is written to `ARMADA_RGB_SCREENSHOT_PATH`
(default: `/run/armada-rgb/screen-sync.nv12.bin`, tmpfs, to avoid wearing flash
storage with a capture every few seconds — the `.nv12.bin` extension is what
selects the raw format). A capture is bounded by a hard
2.5-second timeout (comfortably under the 3s cadence above, so a slow capture
under load just delays the next tick instead of the two ever overlapping) —
a wedged compositor is killed and reaped, never left running or awaited
indefinitely, so this can never hang the daemon past that timeout either way.
On any failure (no graphical session yet, timeout, unreadable/size-mismatched
buffer) the effect logs a one-time diagnostic and keeps showing the last
successfully sampled colors (black before the first successful capture)
instead of flickering.

## Charging indicator (deep-sleep, kernel-triggered)

During true deep suspend the CPU is powered off, so no userspace process can
repaint the LEDs — but the LED controller hardware keeps holding whatever
value was last written, and the kernel's LED trigger framework can drive the
nodes on its own. The suspend/wake hook
(`system_files/usr/lib/systemd/system-sleep/56-armada-rgb-suspend-charging`)
therefore arms a **kernel trigger** rather than a CLI command:

- **pre-sleep** (opt-in): sets `keep_alive=1` on the RGB nodes and arms the
  `battery-charging-orange-full-green` trigger on each `rgb:l?`/`rgb:r?` node,
  so the hardware shows orange while charging and green when full, with no
  software involved.
- **post-wake**: clears the trigger (`none`) and `keep_alive=0`, then runs
  `armada-rgb apply` to restore the user's normal configuration.

There is no `charge-indicator` CLI subcommand and no `charge.json` pin: the
kernel trigger is the single source of truth while the CPU is off, and
`armada-rgb apply` is the single source of truth once it is back.

## Testing overrides

For tests, the profile catalog and device model paths can be overridden with
`ARMADA_RGB_PROFILES_PATH` and `ARMADA_RGB_MODEL_PATH`. The daemon additionally
honors `ARMADA_RGB_CONFIG_PATH`, `ARMADA_RGB_SYSFS_ROOT`,
`ARMADA_RGB_STAT_PATH` (CPU load source), `ARMADA_RGB_POWER_ROOT` (battery
source), `ARMADA_RGB_BACKLIGHT_ROOT` / `ARMADA_RGB_BACKLIGHT_NAME` (screen
backlight source), and `ARMADA_RGB_SCREENSHOT_PATH` /
`ARMADA_RGB_GAMESCOPECTL_BIN` / `ARMADA_RGB_GAMESCOPE_XDG_RUNTIME_DIR` /
`ARMADA_RGB_GAMESCOPE_WAYLAND_DISPLAY` / `ARMADA_RGB_SCREEN_SYNC_USER` /
`ARMADA_RGB_SCREEN_WIDTH` / `ARMADA_RGB_SCREEN_HEIGHT` /
`ARMADA_RGB_NV12_PLANE_ALIGN` (screen capture).
