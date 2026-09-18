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
`load` (hue follows CPU load), and `battery` (hue follows charge). `--speed` is
a percentage where `100` is the default rate.

`armada-rgb run` is the lighting daemon: it keeps the saved configuration
painted, animates the selected effect, and reloads live whenever
`/etc/armada/rgb.json` changes (so a UI writing that file is reflected at once).
Re-asserting the hardware every frame also restores the LEDs after a
suspend/resume that clears the controller. Animated effects use the multicolor
per-LED path; other backends fall back to a uniform color.

The versioned catalog groups exact device-tree model names with a `channels` or
`multicolor` backend, its target list, and an optional default correction.

For tests, the profile catalog and device model paths can be overridden with
`ARMADA_RGB_PROFILES_PATH` and `ARMADA_RGB_MODEL_PATH`. The daemon additionally
honors `ARMADA_RGB_CONFIG_PATH`, `ARMADA_RGB_SYSFS_ROOT`, `ARMADA_RGB_STAT_PATH`
(CPU load source), and `ARMADA_RGB_POWER_ROOT` (battery source).
