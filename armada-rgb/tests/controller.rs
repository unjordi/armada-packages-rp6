use armada_rgb::{
    ChannelBackend, ColorCorrection, Controller, LightingBackend, LightingConfig, MulticolorBackend,
};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    config: PathBuf,
    leds: PathBuf,
    model: PathBuf,
    profiles: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce: u128 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root: PathBuf = std::env::temp_dir().join(format!(
            "rgb-test-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let leds: PathBuf = root.join("leds");
        fs::create_dir_all(&leds).unwrap();

        let fixture = Self {
            config: root.join("etc/rgb.json"),
            leds,
            model: root.join("model"),
            profiles: root.join("profiles.json"),
            root,
        };
        fs::write(&fixture.model, b"Test Device\0").unwrap();
        fixture
    }

    fn controller(&self, targets: &[String]) -> Controller {
        let backend: MulticolorBackend =
            MulticolorBackend::new(self.leds.clone(), targets.to_vec());
        Controller::new(self.config.clone(), LightingBackend::Multicolor(backend))
    }

    fn target(&self, name: &str, order: &str, maximum: &str) {
        let path: PathBuf = self.leds.join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("multi_index"), order).unwrap();
        fs::write(path.join("max_brightness"), maximum).unwrap();
        fs::write(path.join("multi_intensity"), "unchanged\n").unwrap();
        fs::write(path.join("brightness"), "unchanged\n").unwrap();
    }

    fn channel_target(&self, name: &str, maximum: &str) {
        let path: PathBuf = self.leds.join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("max_brightness"), maximum).unwrap();
        fs::write(path.join("brightness"), "unchanged\n").unwrap();
    }

    /// A stand-in for /usr/libexec/armada/device-env that prints `lines`.
    fn device_env(&self, lines: &str) -> PathBuf {
        let path: PathBuf = self.root.join("device-env");
        fs::write(
            &path,
            format!("#!/bin/sh\ncat <<'EOF'\nARMADA_DEVICE_ID=test\n{lines}\nEOF\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn value(&self, target: &str, attribute: &str) -> String {
        fs::read_to_string(self.leds.join(target).join(attribute))
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .into()
    }

    fn command(&self, backend: &str, targets: &[&str], correction: Option<&str>) -> Command {
        let correction: Option<ColorCorrection> = correction.map(|value| value.parse().unwrap());
        let catalog = serde_json::json!({
            "version": 1,
            "profiles": [{
                "models": ["Test Device"],
                "backend": {
                    "type": backend,
                    "targets": targets,
                },
                "correction": correction,
            }],
        });
        fs::write(&self.profiles, serde_json::to_vec(&catalog).unwrap()).unwrap();

        let mut command = Command::new(env!("CARGO_BIN_EXE_armada-rgb"));
        command
            .env("ARMADA_RGB_CONFIG_PATH", &self.config)
            .env("ARMADA_RGB_SYSFS_ROOT", &self.leds)
            .env("ARMADA_RGB_MODEL_PATH", &self.model)
            .env("ARMADA_RGB_PROFILES_PATH", &self.profiles);
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn enabled(color: &str, brightness: u8) -> LightingConfig {
    LightingConfig {
        version: 1,
        enabled: true,
        brightness,
        color: color.into(),
        correction: None,
        ..LightingConfig::default()
    }
}

#[test]
fn defaults_and_config_round_trip() {
    let fixture: Fixture = Fixture::new();
    fixture.target("rgb:l1", "blue green red", "255");
    let controller: Controller = fixture.controller(&["rgb:l1".into()]);

    let defaults: LightingConfig = controller.get().unwrap();
    assert!(!defaults.enabled);
    assert_eq!(defaults.brightness, 25);

    let saved: LightingConfig = controller.set(enabled("abcdef", 40)).unwrap();
    assert_eq!(saved.color, "ABCDEF");
    assert_eq!(controller.get().unwrap(), saved);
    assert!(!fixture.config.with_extension("tmp").exists());
}

#[test]
fn thor_bgr_targets_do_not_touch_other_leds() {
    let fixture: Fixture = Fixture::new();
    let targets: Vec<String> = ["l1", "l2", "l3", "l4", "r1", "r2", "r3", "r4"]
        .map(|name| format!("rgb:{name}"))
        .into();
    for target in &targets {
        fixture.target(target, "blue green red", "255");
    }
    fixture.target("power-led", "red green blue", "511");
    let controller: Controller = fixture.controller(&targets);

    controller.set(enabled("FF8000", 25)).unwrap();

    for target in targets {
        assert_eq!(fixture.value(&target, "multi_intensity"), "0 55 255");
        assert_eq!(fixture.value(&target, "brightness"), "64");
    }
    assert_eq!(fixture.value("power-led", "brightness"), "unchanged");
}

#[test]
fn odin3_channel_targets_do_not_touch_other_leds() {
    let fixture: Fixture = Fixture::new();
    let targets: Vec<String> = [
        "red=l:r1",
        "red=r:r1",
        "green=l:g1",
        "green=r:g1",
        "blue=l:b1",
        "blue=r:b1",
    ]
    .map(str::to_owned)
    .into();
    for target in ["l:r1", "r:r1", "l:g1", "r:g1", "l:b1", "r:b1"] {
        fixture.channel_target(target, "255");
    }
    fixture.channel_target("power-led", "255");
    let backend: ChannelBackend = ChannelBackend::new(fixture.leds.clone(), targets);
    let controller: Controller =
        Controller::new(fixture.config.clone(), LightingBackend::Channels(backend));

    controller.set(enabled("FF8000", 25)).unwrap();

    for target in ["l:r1", "r:r1"] {
        assert_eq!(fixture.value(target, "brightness"), "64");
    }
    for target in ["l:g1", "r:g1"] {
        assert_eq!(fixture.value(target, "brightness"), "14");
    }
    for target in ["l:b1", "r:b1"] {
        assert_eq!(fixture.value(target, "brightness"), "0");
    }
    assert_eq!(fixture.value("power-led", "brightness"), "unchanged");

    controller.off().unwrap();
    for target in ["l:r1", "r:r1", "l:g1", "r:g1", "l:b1", "r:b1"] {
        assert_eq!(fixture.value(target, "brightness"), "0");
    }
}

#[test]
fn cli_supports_channel_backend() {
    let fixture: Fixture = Fixture::new();
    for target in ["l:r1", "l:g1", "l:b1"] {
        fixture.channel_target(target, "255");
    }
    let output: std::process::Output = fixture
        .command(
            "channels",
            &["red=l:r1", "green=l:g1", "blue=l:b1"],
            Some("always:0,20,20"),
        )
        .args(["set", "--color", "00ffff", "--brightness", "100"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(fixture.value("l:r1", "brightness"), "0");
    assert_eq!(fixture.value("l:g1", "brightness"), "154");
    assert_eq!(fixture.value("l:b1", "brightness"), "154");
}

#[test]
fn correction_preserves_the_user_color() {
    let fixture: Fixture = Fixture::new();
    fixture.target("rgb:sticks", "red green blue", "255");
    let run = |color: &str, correction: Option<&str>| {
        let mut command: Command =
            fixture.command("multicolor", &["rgb:sticks"], Some("red:0,20,20"));
        command.args(["set", "--color", color, "--brightness", "100"]);
        if let Some(correction) = correction {
            command.args(["--correction", correction]);
        }
        command.output().unwrap()
    };

    let output: std::process::Output = run("FFFF00", None);
    let config: LightingConfig = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success());
    assert_eq!(config.color, "FFFF00");
    assert_eq!(config.correction.unwrap().green, 20);
    assert_eq!(fixture.value("rgb:sticks", "multi_intensity"), "255 154 0");

    let output: std::process::Output = run("00FFFF", Some("red:10,30,40"));
    assert!(output.status.success());
    assert_eq!(fixture.value("rgb:sticks", "multi_intensity"), "0 255 255");

    let output: std::process::Output = run("FFFFFF", None);
    let config: LightingConfig = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success());
    assert_eq!(config.correction.unwrap().green, 30);
}

#[test]
fn target_errors_happen_before_writes() {
    let fixture: Fixture = Fixture::new();
    fixture.target("rgb:l1", "blue green red", "255");
    let targets: Vec<String> = vec!["rgb:l1".into(), "rgb:missing".into()];
    let controller: Controller = fixture.controller(&targets);

    assert!(controller.set(enabled("FFFFFF", 25)).is_err());
    assert_eq!(fixture.value("rgb:l1", "brightness"), "unchanged");
    assert!(!fixture.config.exists());
}

#[test]
fn preflight_rejects_an_unwritable_target() {
    let fixture: Fixture = Fixture::new();
    fixture.target("rgb:l1", "blue green red", "255");
    fixture.target("rgb:r1", "blue green red", "255");
    fs::set_permissions(
        fixture.leds.join("rgb:r1/multi_intensity"),
        fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    let targets: Vec<String> = vec!["rgb:l1".into(), "rgb:r1".into()];
    let controller: Controller = fixture.controller(&targets);

    assert!(controller.set(enabled("FF0000", 10)).is_err());
    assert_eq!(fixture.value("rgb:l1", "brightness"), "unchanged");
    assert!(!fixture.config.exists());
}

#[test]
fn partial_writes_leave_targets_off() {
    let fixture: Fixture = Fixture::new();
    fixture.target("rgb:l1", "blue green red", "255");
    fixture.target("rgb:r1", "blue green red", "255");
    let brightness: PathBuf = fixture.leds.join("rgb:r1/brightness");
    fs::remove_file(&brightness).unwrap();
    symlink("/dev/full", brightness).unwrap();
    let targets: Vec<String> = vec!["rgb:l1".into(), "rgb:r1".into()];
    let controller: Controller = fixture.controller(&targets);

    assert!(controller.set(enabled("FF0000", 10)).is_err());
    assert_eq!(fixture.value("rgb:l1", "brightness"), "0");
    assert!(!fixture.config.exists());
}

#[test]
fn off_is_persistent_and_only_needs_brightness() {
    let fixture: Fixture = Fixture::new();
    fixture.target("rgb:l1", "blue green red", "100");
    let controller: Controller = fixture.controller(&["rgb:l1".into()]);
    controller.set(enabled("00FF00", 30)).unwrap();

    fs::write(fixture.leds.join("rgb:l1/multi_index"), "not rgb").unwrap();
    fs::write(fixture.leds.join("rgb:l1/max_brightness"), "zero").unwrap();
    let config: LightingConfig = controller.off().unwrap();

    assert!(!config.enabled);
    assert!(!controller.get().unwrap().enabled);
    assert_eq!(fixture.value("rgb:l1", "brightness"), "0");
}

#[test]
fn unsupported_devices_are_ignored_during_boot() {
    let fixture: Fixture = Fixture::new();
    fixture.target("power-led", "red green blue", "511");
    let controller: Controller = Controller::new(
        fixture.config.clone(),
        LightingBackend::Unsupported("not configured".into()),
    );

    assert_eq!(controller.apply().unwrap(), Some("not configured".into()));
    assert!(controller.set(enabled("FFFFFF", 25)).is_err());
    assert_eq!(fixture.value("power-led", "brightness"), "unchanged");
}

#[test]
fn cli_sets_gets_and_turns_off() {
    let fixture: Fixture = Fixture::new();
    fixture.target("rgb:l1", "blue green red", "255");
    let run = |args: &[&str]| {
        fixture
            .command("multicolor", &["rgb:l1"], None)
            .args(args)
            .output()
            .unwrap()
    };

    let output: std::process::Output = run(&["set", "--color", "aa00ff", "--brightness", "7"]);
    assert!(output.status.success());
    let config: LightingConfig = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(config.color, "AA00FF");
    assert!(config.enabled);

    let output: std::process::Output = run(&["off"]);
    assert!(output.status.success());
    let config: LightingConfig = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!config.enabled);

    let output: std::process::Output = run(&["get"]);
    let config: LightingConfig = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!config.enabled);

    let output: std::process::Output = run(&["set", "--color", "FFFFFF", "--brightness", "101"]);
    assert!(!output.status.success());
}

#[test]
fn sync_brightness_cli_toggle_persists_and_run_scales_on_top_of_static() {
    let fixture: Fixture = Fixture::new();
    fixture.target("rgb:l1", "blue green red", "255");

    let backlight_root: PathBuf = fixture.root.join("backlight");
    fs::create_dir_all(backlight_root.join("panel")).unwrap();
    fs::write(backlight_root.join("panel/brightness"), "50\n").unwrap();
    fs::write(backlight_root.join("panel/max_brightness"), "100\n").unwrap();

    // The LED-vs-screen factor reaches the daemon the way it does on the
    // device: through the OS device-env helper, not a hard-coded constant.
    let device_env: PathBuf = fixture.device_env("ARMADA_RGB_SYNC_SCALE=0.88");
    let with_backlight_env = |command: &mut Command| {
        command
            .env_remove("ARMADA_RGB_SYNC_SCALE")
            .env("ARMADA_DEVICE_ENV", &device_env)
            .env("ARMADA_RGB_BACKLIGHT_ROOT", &backlight_root)
            .env("ARMADA_RGB_BACKLIGHT_NAME", "panel");
    };

    // A plain static color+brightness — proves sync_brightness composes with
    // it (not a competing effect): color must stay exactly what was set.
    let mut command: Command = fixture.command("multicolor", &["rgb:l1"], None);
    let output: std::process::Output = command
        .args(["set", "--color", "00FF00", "--brightness", "80"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let mut command: Command = fixture.command("multicolor", &["rgb:l1"], None);
    let output: std::process::Output = command.args(["sync-brightness", "on"]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let config: LightingConfig = serde_json::from_slice(&output.stdout).unwrap();
    assert!(config.sync_brightness);
    assert_eq!(config.color, "00FF00", "sync-brightness on must not touch color");
    assert_eq!(config.effect, armada_rgb::Effect::Static, "must not touch effect either");

    let mut command: Command = fixture.command("multicolor", &["rgb:l1"], None);
    with_backlight_env(&mut command);
    let mut daemon: std::process::Child = command.args(["run"]).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let _ = daemon.kill();
    let _ = daemon.wait();

    // Color untouched (full-scale gamma of pure green); only the separate
    // `brightness` attribute is scaled by the 50% backlight fixture.
    assert_eq!(fixture.value("rgb:l1", "multi_intensity"), "0 255 0");
    assert_eq!(fixture.value("rgb:l1", "brightness"), "89"); // QG-5: scale(round(80*50%*0.88)=35, 255) = 89 (LED -12% under panel)

    // Turning it back off restores the unscaled brightness on the next tick.
    let mut command: Command = fixture.command("multicolor", &["rgb:l1"], None);
    let output: std::process::Output = command.args(["sync-brightness", "off"]).output().unwrap();
    assert!(output.status.success());

    let mut command: Command = fixture.command("multicolor", &["rgb:l1"], None);
    with_backlight_env(&mut command);
    let mut daemon: std::process::Child = command.args(["run"]).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let _ = daemon.kill();
    let _ = daemon.wait();

    assert_eq!(fixture.value("rgb:l1", "brightness"), "204"); // scale(80, 255), unscaled
}

/// Run the daemon briefly with a static 80% green, sync-brightness on and a
/// 50% backlight; return (LED brightness written, daemon stderr).
fn run_synced_static(device_env: Option<&str>) -> (String, String) {
    let fixture: Fixture = Fixture::new();
    fixture.target("rgb:l1", "blue green red", "255");
    let backlight_root: PathBuf = fixture.root.join("backlight");
    fs::create_dir_all(backlight_root.join("panel")).unwrap();
    fs::write(backlight_root.join("panel/brightness"), "50\n").unwrap();
    fs::write(backlight_root.join("panel/max_brightness"), "100\n").unwrap();

    for args in [
        &["set", "--color", "00FF00", "--brightness", "80"][..],
        &["sync-brightness", "on"][..],
    ] {
        let output = fixture
            .command("multicolor", &["rgb:l1"], None)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let helper: PathBuf = match device_env {
        Some(lines) => fixture.device_env(lines),
        None => fixture.root.join("no-such-device-env"),
    };
    let mut command: Command = fixture.command("multicolor", &["rgb:l1"], None);
    let mut daemon: std::process::Child = command
        .env_remove("ARMADA_RGB_SYNC_SCALE")
        .env("ARMADA_DEVICE_ENV", &helper)
        .env("ARMADA_RGB_BACKLIGHT_ROOT", &backlight_root)
        .env("ARMADA_RGB_BACKLIGHT_NAME", "panel")
        .arg("run")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let _ = daemon.kill();
    let output = daemon.wait_with_output().unwrap();
    (
        fixture.value("rgb:l1", "brightness"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn sync_scale_comes_from_device_env() {
    // 0.88: round(80 * 50% * 0.88) = 35 -> scale(35, 255) = 89.
    let (brightness, _) = run_synced_static(Some("ARMADA_RGB_SYNC_SCALE=0.88"));
    assert_eq!(brightness, "89");
    // Any other device value is honored: round(80 * 50% * 0.5) = 20 -> 51.
    let (brightness, _) = run_synced_static(Some("ARMADA_RGB_SYNC_SCALE=0.5"));
    assert_eq!(brightness, "51");
}

#[test]
fn sync_scale_defaults_to_unscaled_without_device_key() {
    // No helper, or a helper that leaves the key unset: factor 1.0, so
    // round(80 * 50%) = 40 -> scale(40, 255) = 102, and no warning.
    for device_env in [None, Some("ARMADA_RGB_SYNC_SCALE=''")] {
        let (brightness, stderr) = run_synced_static(device_env);
        assert_eq!(brightness, "102", "{device_env:?}");
        assert!(
            !stderr.contains("ARMADA_RGB_SYNC_SCALE"),
            "{device_env:?}: {stderr}"
        );
    }
}

#[test]
fn invalid_sync_scale_falls_back_to_default_with_warning() {
    for value in ["0", "3", "bright"] {
        let (brightness, stderr) =
            run_synced_static(Some(&format!("ARMADA_RGB_SYNC_SCALE={value}")));
        assert_eq!(brightness, "102", "{value}");
        assert!(
            stderr.contains("[armada-rgb] ARMADA_RGB_SYNC_SCALE="),
            "{value}: {stderr}"
        );
    }
}

#[test]
fn cli_reports_profile_support() {
    let fixture: Fixture = Fixture::new();
    let output: std::process::Output = fixture
        .command("multicolor", &["rgb:l1"], None)
        .arg("supported")
        .output()
        .unwrap();
    assert!(output.status.success());

    fs::write(&fixture.model, b"Unknown Device\0").unwrap();
    let output: std::process::Output = fixture
        .command("multicolor", &["rgb:l1"], None)
        .arg("supported")
        .output()
        .unwrap();
    assert!(!output.status.success());
}
