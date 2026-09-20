#!/bin/bash
# Samples gamescope's CPU utime+stime (in clock ticks) over a wall-clock
# window, for A/B comparing "idle" vs "pipewire consumer attached" cost.
# No bc dependency (immutable /usr on the RP6) -- awk does the arithmetic.
set -euo pipefail
PID=$(pgrep -f '^/usr/bin/gamescope ' | head -1)
if [ -z "$PID" ]; then echo "gamescope not found"; exit 1; fi
HZ=$(getconf CLK_TCK)
read_ticks() { awk '{print $14+$15}' "/proc/$PID/stat"; }
T0=$(date +%s.%N)
S0=$(read_ticks)
sleep "$1"
T1=$(date +%s.%N)
S1=$(read_ticks)
awk -v t0="$T0" -v t1="$T1" -v s0="$S0" -v s1="$S1" -v hz="$HZ" -v pid="$PID" '
BEGIN {
  dt = t1 - t0
  dticks = s1 - s0
  cpu_sec = dticks / hz
  pct = 100 * cpu_sec / dt
  printf "pid=%s wall=%.2fs cpu=%.3fs (%d ticks @ %sHz) => %.1f%% of one core\n", pid, dt, cpu_sec, dticks, hz, pct
}'
