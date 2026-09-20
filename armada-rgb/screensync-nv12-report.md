# screen_sync efficiency: raw NV12 instead of PNG (armada#27 / QG-2 follow-up)

**Branch:** `fix/screensync-nv12` · **Crate:** `armada-rgb` · **Measured on the RP6 in Game Mode, 2026-09-20.**

## What changed

The `screen_sync` (ambilight) effect used to capture a **PNG** (`gamescopectl
screenshot <path>.png`) every 3 s and decode the whole ~2 MP frame with the
`image` crate just to average a few edge columns. Jordi flagged this as too
heavy for a live-sync on a handheld.

Now it captures a **raw NV12 buffer** (`<path>.nv12.bin` — the extension is what
selects the encoder) and reads luma/chroma **straight out of the raw planes** —
no compositor-side PNG encode, and no 2 MP decode on our side. The `image`
crate dependency was **removed entirely** (proof the decode is gone). The edge
band is still sampled on the same bounded grid.

## The real NV12 layout (measured, not assumed)

gamescope composites the RP6's portrait panel as a **landscape 1920×1080** frame
(`gamescope --force-orientation left …`). Capturing the same static frame as
both PNG and NV12:

| format | bytes | wall time (capture) |
|--------|------:|--------------------:|
| PNG (`.png`)      | ~82 KB–2.4 MB (content-dependent) | 363–461 ms |
| NV12 (`.nv12.bin`)| **3 117 056** (fixed)             | 158–199 ms |

`3 117 056` is **not** `W·H·1.5` (that would be 3 110 400). The extra 6 656 bytes
are **page (4096-byte) alignment of each plane**, which reproduces the size to
the byte:

```
Y  plane: stride = 1920 (== width, no per-row padding), 1080 rows
          Y size      = 1920·1080     = 2 073 600
          padded to 4096              = 2 076 672   ← uv_offset
UV plane: interleaved U,V, stride 1920, 540 rows (2×2 subsampled)
          UV size     = 1920·540      = 1 036 800
          padded to 4096              = 1 040 384
total     = 2 076 672 + 1 040 384     = 3 117 056   ✓ (exact match)
```

So: `uv_offset = align_up(W·H, 4096)`, `stride_y = stride_uv = W`. The code
(`Nv12Layout::resolve`) validates the file size against this (it also accepts a
tightly-packed buffer and a page-aligned-Y-with-tight-UV variant); on any
mismatch (e.g. a resolution change) it **falls back to the base color (QG-2)**
rather than sampling garbage. Geometry is env-overridable
(`ARMADA_RGB_SCREEN_WIDTH` / `_HEIGHT` / `ARMADA_RGB_NV12_PLANE_ALIGN`).

A partial-read hazard was found and fixed: `gamescopectl` returns almost
immediately and the compositor **grows the file asynchronously**, so reading on
mere existence caught a partial buffer (observed a 2 822 144-byte short read).
`run_screenshot_command` now waits until the file size **stabilizes** before
reading (format-agnostic, bounded).

## Before / after cost (per tick, measured on the device)

| stage | OLD (PNG) | NEW (NV12) |
|-------|-----------|------------|
| compositor capture (wall) | 363–461 ms (encode) | 158–199 ms (no encode); ~301 ms via the daemon incl. the 60 ms stabilize wait |
| our-side process | **2 MP PNG decode** ≈ hundreds of ms (ffmpeg decode of the 2 MP frame on this ARM: 232–1009 ms, incl. ffmpeg startup) + edge average | **read 3 MB from tmpfs + sample** = **1.4–6.2 ms**, no decode |

The 2 MP decode is *gone*, not merely faster — `image::open(...).to_rgb8()` was
removed and the `image` crate dropped. The `screen-sync-probe` diagnostic
subcommand (added, hidden) reports the real numbers on-device:

```
probe: capture=301.5ms process=1.42ms bytes=3117056 geom=1920x1080 uv_offset=2076672 left=[60,58,55] right=[66,68,68]
```

## Correctness verification (the 3 gates)

1. **`cargo test` green** — 36 unit + 13 integration tests pass (host x86,
   `--release --locked`), clippy clean. New tests: `Nv12Layout::resolve` against
   the measured 3 117 056 geometry, NV12 edge sampling, YUV→RGB round-trip,
   size-mismatch → base fallback, and the rewritten screen_sync capture tests
   (now synthesize NV12, no `image` crate).

2. **Cost measured on device (above):** process dropped from a ~2 MP PNG decode
   (hundreds of ms) to **~2 ms**; capture wall roughly halved; no 2 MP decode.

3. **Still reflects the screen (sampling correct + tracks content):**
   - Real gamescope frame vs PNG ground truth (ffmpeg, same frame): NV12 right
     edge `[65,67,67]` vs PNG `[69,68,67]` (±4); left `[60,57,55]` vs `[47,44,43]`
     — both neutral gray, correct hue (the ~13 left delta is grid-sample vs
     full-average, not a layout error; a wrong plane offset would give garbage,
     not ±4).
   - Synthetic frames through the sampler: red buffer → `[254,0,0]`, blue buffer
     → `[0,0,254]` (exact).
   - **End-to-end on the real LED hardware** (`multi_index` = blue,green,red):
     RED frame → `rgb:l1 multi_intensity = 0 0 253` (pure red), BLUE frame →
     `253 0 0` (pure blue). The LED changes with content and matches the sample.

## QA method / notes

- Cross-built arm64 in the project's aarch64 podman container (qemu). Live QA
  ran the binary **manually** (`… run` / `screen-sync-probe`) with the service
  stopped — the sanctioned no-drift path; **`/usr/bin/armada-rgb` was not
  overwritten**. The fix ships the proper way: this PR → `develop` → image
  rebuild (`construir-imagen-rp6`) → OTA.
- Follow-up (OS repo `~/code/armada`, separate PR): the drop-in comment in
  `armada-rgb.service.d/10-screen-sync-user.conf` still says "write the PNG" —
  it now writes NV12; a one-word doc fix.
