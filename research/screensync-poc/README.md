# screensync-poc — research prototype, NOT for production

Minimal C consumer of gamescope's own (private, non-standard) PipeWire video
stream. Built to answer one question empirically: *is gamescope's native
"continuous capture" path (the `gamescope_pipewire` Wayland protocol + the
"gamescope" PipeWire `Video/Source` node) a viable, lower-cost replacement for
the polling `gamescopectl screenshot` approach `armada-rgb`'s `screen_sync`
effect uses?*

**Verdict: no — do not build on this.** See
`$JUEGOS/.claude/projects/rp6-joystick-leds/ambilight-realtime-2026-09-20.md`
for the full writeup with evidence. Short version: gamescope's screenshot
compute shader (`vulkan_screenshot` in `rendervulkan.cpp`) dispatches sized to
the compositor's **native output resolution**, not to the destination
texture's size — confirmed in gamescope's own upstream source
(`OpenGamingCollective/gamescope`). That dispatch is identical whether it's
triggered once (a `take_screenshot`/`gamescopectl` call) or continuously via
`paint_pipewire()`, which fires on **every vblank** (up to 60-120Hz) for as
long as a PipeWire consumer is attached to the "gamescope" node. A continuous
consumer therefore pays the SAME per-frame GPU cost as a `gamescopectl`
screenshot, just 60-400x more often. This is a structural regression versus
the already-cheap (post nv12 fix) periodic-poll baseline, not an improvement.

## What's here

- `client.c` — a hand-rolled libpipewire consumer that:
  - Binds the "gamescope" `Video/Source` PipeWire node by name
    (`PW_KEY_TARGET_OBJECT=gamescope`), confirming it needs **no Wayland
    session at all** — just `XDG_RUNTIME_DIR` pointed at the PipeWire socket
    (world-RW on the device, so root can connect directly, no `su - armada`
    Wayland-env dance needed for this specific path).
  - Negotiates BGRx/MemFd frames and attempts (buggy, unresolved — see the
    writeup) gamescope's private `SPA_FORMAT_VIDEO_requested_size` vendor
    extension to request a downscaled capture target.
  - Prints per-frame timing + a left/right edge-average color (the same
    signal `screen_sync` needs) so behaviour is directly inspectable.
- `sample_cpu.sh` — samples a PID's utime+stime over a wall-clock window (no
  `bc` dependency — the RP6's `/usr` is immutable and doesn't have it), used
  to A/B gamescope's own CPU cost with/without a PipeWire consumer attached.

## How to build (aarch64 cross, mirrors `armada-rgb/build.sh`'s podman pattern)

```bash
cd research/screensync-poc
source ../../toolchain.env   # BUILDER_IMAGE (pinned Fedora digest)
podman run --rm --platform linux/aarch64 --volume "$PWD:/work:Z" --workdir /work "$BUILDER_IMAGE" \
  bash -euxo pipefail -c '
    dnf -y install --skip-unavailable gcc pkgconf-pkg-config pipewire-devel
    gcc -O2 -o screensync-poc client.c $(pkg-config --cflags --libs libpipewire-0.3) -lm
  '
scp screensync-poc root@rp6:/var/tmp/
scp sample_cpu.sh root@rp6:/var/tmp/
```

## How to run

```bash
# full native-res streaming (req_w=0 skips the buggy downscale request):
ssh root@rp6 'XDG_RUNTIME_DIR=/run/user/1000 /var/tmp/screensync-poc 0 0 200'

# attempt a downscaled request (currently never converges past the first,
# full-res negotiation in testing — worth fixing only if a FUTURE use case
# needs real full-quality continuous capture, e.g. screen recording, where
# gamescope's own reference tool `src/Apps/gamescopestream.cpp` is a much
# better starting point than this file — it doesn't use requested_size
# either, for what that's worth):
ssh root@rp6 'XDG_RUNTIME_DIR=/run/user/1000 /var/tmp/screensync-poc 24 40 200'

# A/B the CPU cost of gamescope itself, with vs without a consumer attached:
ssh root@rp6 '/var/tmp/sample_cpu.sh 6'   # idle baseline
ssh root@rp6 'XDG_RUNTIME_DIR=/run/user/1000 /var/tmp/screensync-poc 0 0 600 & sleep 1; /var/tmp/sample_cpu.sh 6; wait'
```

## Operational warning (read before running any of this on a live device)

During this research, repeated rapid `su - armada -c "..."` SSH round-trips
(used to reach the PipeWire/Wayland user session as root) coincided with
`gamescope-session-plus@steam.service` getting stuck mid-stop (SIGKILL sent,
"Processes still around after final SIGKILL", `Failed with result 'timeout'`)
and the freshly-restarted `gamescope` process going `<defunct>` within
seconds, repeatedly. A full `systemctl reboot` was needed to recover. This
may be pre-existing device fragility unrelated to PipeWire specifically (the
existing `rp6-perfil.md` already flags `systemctl --user` over SSH as
unstable on this device) — but the correlation with session churn is strong
enough to flag. See the writeup's "Operational caution" section.
