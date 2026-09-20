#!/usr/bin/bash

set -euxo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
PACKAGE_DIR="${PWD}"

source ../toolchain.env

rm -rf out
mkdir -p out

podman run --rm \
  --volume "${PACKAGE_DIR}:/work:Z" \
  --workdir /work \
  --platform linux/aarch64 \
  "${BUILDER_IMAGE}" \
  bash -euxo pipefail -c '
    cat >/etc/rpm/macros.armada <<EOF
%_buildhost armada-builder
%packager Armada
%vendor Armada
EOF

    NAME=armada-rgb

    dnf -y install --skip-unavailable \
      rpm-build rpmdevtools dnf-plugins-core \
      util-linux tar gzip
    dnf -y builddep "${NAME}.spec"
    rpmdev-setuptree

    # C1/M4 regression guard. Context: the screen_sync capture in
    # src/effects.rs (run_screenshot_command) shells out through the
    # command  runuser -u USER --  on purpose, NOT the login-shell forms
    # (su -, runuser -l), because on this Fedora base the plain runuser PAM
    # service does NOT chain in pam_systemd (no postlogin/system-auth
    # include) -- so it never registers a new systemd-logind session. At
    # the 3s screen_sync capture cadence, a PAM/logind session per call
    # floods logind badly enough to starve out the real Game Mode session:
    # that flood, from the login-shell forms, was the actual root cause of
    # a device reboot-loop this fix replaced. /etc/pam.d/runuser ships from
    # the util-linux RPM installed above (same fc44 build as the device) --
    # if a future Fedora base ever adds pam_systemd there, this assumption
    # breaks silently and the flood comes back under a different command
    # name. Catch that here, at build time, instead of on a device
    # reboot-loop. Fail loud, not skip, if the file is ever missing too --
    # an unverifiable assumption is worse than a silently-passing one.
    test -f /etc/pam.d/runuser || {
      echo "FATAL: /etc/pam.d/runuser is missing from this build base," >&2
      echo "cannot verify the C1 pam_systemd regression guard for" >&2
      echo "run_screenshot_command in src/effects.rs. Fix the check or" >&2
      echo "the base image before building." >&2
      exit 1
    }
    if grep -q pam_systemd /etc/pam.d/runuser; then
      echo "FATAL: /etc/pam.d/runuser now includes pam_systemd on this" >&2
      echo "build base. The screen_sync capture in src/effects.rs" >&2
      echo "(run_screenshot_command) relies on plain  runuser -u USER --" >&2
      echo "NOT opening a PAM/logind session. With pam_systemd added," >&2
      echo "every capture would register a new logind session every" >&2
      echo "~3s and flood it, same as the login-shell bug this replaced." >&2
      echo "See the fix/screensync-runuser change." >&2
      exit 1
    fi

    cp "${NAME}.spec" ~/rpmbuild/SPECS/
    tar -C / \
      --exclude=work/out \
      --exclude=work/target \
      -czf ~/rpmbuild/SOURCES/${NAME}.tar.gz \
      work

    rpmbuild -bb ~/rpmbuild/SPECS/${NAME}.spec

    cp ~/rpmbuild/RPMS/aarch64/*.rpm /work/out/
  '

echo "built: ${PACKAGE_DIR}/out"
