//! Exercise deployment shells with temporary config/volumes and a fake Podman.
//! These tests never use a live container engine, QQ account, or local config.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use qkbot::{admin::settings::ConfigStore, config::Config};

const SECRET_ENV: &[&str] = &[
    "QKBOT_API_KEY",
    "QKBOT_MILKY_TOKEN",
    "QKBOT_ADMIN_TOKEN",
    "QKBOT_LLBOT_TOKEN",
    "QKBOT_NONEBOT_TOKEN",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn shell() -> Command {
    if let Some(path) = std::env::var_os("QKBOT_TEST_SHELL") {
        return Command::new(path);
    }
    #[cfg(windows)]
    {
        let mut directories = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default();
        for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(directory) = std::env::var_os(variable) {
                let directory = PathBuf::from(directory);
                directories.push(directory.join("Git/bin"));
                directories.push(directory.join("Programs/Git/bin"));
            }
        }
        Command::new(
            windows_shell(&directories)
                .expect("Install Git Bash or set QKBOT_TEST_SHELL to the shell executable"),
        )
    }
    #[cfg(not(windows))]
    {
        Command::new("sh")
    }
}

#[cfg(windows)]
fn windows_shell(directories: &[PathBuf]) -> Option<PathBuf> {
    // Windows' bash.exe can be a WSL launcher. Prefer the Git installation
    // associated with git.exe, then native POSIX shells with a sibling sh.exe.
    for directory in directories {
        if directory.join("git.exe").is_file() {
            for candidate in [
                directory.join("bash.exe"),
                directory.join("../bin/bash.exe"),
                directory.join("../usr/bin/bash.exe"),
            ] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    directories
        .iter()
        .find(|directory| {
            directory.join("bash.exe").is_file() && directory.join("sh.exe").is_file()
        })
        .map(|directory| directory.join("bash.exe"))
}

#[cfg(windows)]
#[test]
fn windows_shell_discovery_uses_custom_git_installation_not_wsl_launcher() {
    let temporary = tempfile::tempdir().unwrap();
    let wsl = temporary.path().join("WindowsApps");
    let git_command = temporary.path().join("custom-install/cmd");
    let git_bin = temporary.path().join("custom-install/bin");
    for directory in [&wsl, &git_command, &git_bin] {
        fs::create_dir_all(directory).unwrap();
    }
    fs::write(wsl.join("bash.exe"), []).unwrap();
    fs::write(git_command.join("git.exe"), []).unwrap();
    fs::write(git_bin.join("bash.exe"), []).unwrap();
    assert!(windows_shell(std::slice::from_ref(&wsl)).is_none());
    let found = windows_shell(&[wsl.clone(), git_command]).unwrap();
    assert_eq!(
        fs::canonicalize(found).unwrap(),
        fs::canonicalize(git_bin.join("bash.exe")).unwrap()
    );
    fs::write(git_bin.join("sh.exe"), []).unwrap();
    assert_eq!(
        windows_shell(&[wsl, git_bin.clone()]).unwrap(),
        git_bin.join("bash.exe")
    );
}

fn shell_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn clean_environment(command: &mut Command) {
    for key in SECRET_ENV {
        command.env_remove(key);
    }
}

fn seed(config: &Path, template: &Path, mode: &str) -> Output {
    let mut command = shell();
    clean_environment(&mut command);
    command
        .arg(shell_path(&root().join("deploy/container-entrypoint.sh")))
        .arg("--init-config")
        .env("QKBOT_CONFIG_PATH", shell_path(config))
        .env("QKBOT_CONFIG_TEMPLATE", shell_path(template))
        .env("QKBOT_CONTAINER_NETWORK", mode)
        .output()
        .expect("run the container entrypoint shell")
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "fixture command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn host_and_pod_seed_valid_config_with_persistent_paths() {
    let temporary = tempfile::tempdir().unwrap();
    let template = temporary.path().join("example.toml");
    let source = fs::read_to_string(root().join("config.example.toml")).unwrap();
    fs::write(
        &template,
        source.replace(
            "image_font_path = \"\"",
            "image_font_path = \"fonts/custom.ttf\"",
        ),
    )
    .unwrap();
    for (mode, listen) in [("host", "127.0.0.1:3210"), ("pod", "0.0.0.0:3210")] {
        let config = temporary.path().join(mode).join("data/config.toml");
        success(seed(&config, &template, mode));
        let raw = Config::load_raw(&config).unwrap();
        assert_eq!(raw.web.listen, listen);
        assert_eq!(raw.nonebot.listen, "127.0.0.1:3215");

        // ConfigStore reads environment credentials. Check it in an isolated
        // child so a developer's environment cannot affect this fixture.
        let mut child = Command::new(std::env::current_exe().unwrap());
        clean_environment(&mut child);
        success(
            child
                .args([
                    "container_config_validation_child",
                    "--exact",
                    "--nocapture",
                ])
                .env("QKBOT_DEPLOYMENT_TEST_CONFIG", &config)
                .output()
                .unwrap(),
        );
    }
}

#[test]
fn container_config_validation_child() {
    let Some(path) = std::env::var_os("QKBOT_DEPLOYMENT_TEST_CONFIG") else {
        return;
    };
    let path = PathBuf::from(path);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store = ConfigStore::open(path.clone()).await.unwrap();
        let config = store.effective().await.unwrap();
        config.validate_runtime().unwrap();
        let volume = fs::canonicalize(path.parent().unwrap()).unwrap();
        for value in [
            &config.app.database,
            &config.nonebot.media_dir,
            &config.plugins.native.poke.resource_dir,
            &config.plugins.native.image_font_path,
        ] {
            assert!(Path::new(value).starts_with(&volume));
        }
        assert!(config.llm.api_key.is_empty());
        assert!(
            config
                .milky
                .access_token
                .as_deref()
                .unwrap_or("")
                .is_empty()
        );
        assert!(config.qq_login.token.is_empty());
        assert!(config.nonebot.token.is_empty());
    });
}

#[test]
fn existing_config_survives_restart_and_network_mode_change() {
    let temporary = tempfile::tempdir().unwrap();
    let config = temporary.path().join("data/config.toml");
    let template = root().join("config.example.toml");
    success(seed(&config, &template, "host"));
    let mut original = fs::read(&config).unwrap();
    original.extend_from_slice(b"\n# administrator changes must survive\n");
    fs::write(&config, &original).unwrap();
    success(seed(&config, &temporary.path().join("no-template"), "pod"));
    assert_eq!(fs::read(&config).unwrap(), original);
}

#[test]
fn invalid_initialization_does_not_publish_partial_config() {
    let temporary = tempfile::tempdir().unwrap();
    let config = temporary.path().join("data/config.toml");
    let template = root().join("config.example.toml");
    assert!(!seed(&config, &template, "public").status.success());
    assert!(!config.exists());
    assert!(
        !seed(&config, &temporary.path().join("missing"), "host")
            .status
            .success()
    );
    assert!(!config.exists());
    assert_eq!(fs::read_dir(config.parent().unwrap()).unwrap().count(), 0);
}

struct PodmanFixture {
    directory: tempfile::TempDir,
}

impl PodmanFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let executable = bin.join("podman");
        fs::write(
            &executable,
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$PODMAN_TEST_LOG"
case "$1 $2" in
  "container exists") exit "${PODMAN_TEST_EXISTS:-1}";;
  "inspect --format")
    case "$3" in
      *Config.Labels*) printf '%s\n' "${PODMAN_TEST_OWNER:-managed}";;
      *State.Running*)
        if [ -f "$PODMAN_TEST_MARKER" ]; then
          printf '%s\n' "${PODMAN_TEST_AFTER_RUNNING:-false}"
        else
          printf '%s\n' "${PODMAN_TEST_RUNNING:-false}"
        fi;;
      *State.StartedAt*)
        if [ -f "$PODMAN_TEST_MARKER" ]; then
          printf '%s\n' "${PODMAN_TEST_AFTER_STARTED:-original-start}"
        else
          printf '%s\n' original-start
        fi;;
      *Mounts*) printf '%s\n' actual-persistent-volume;;
      *) exit 90;;
    esac;;
  "volume create")
    [ "$3" = --ignore ] || exit 92
    printf '%s\n' "$4";;
  "volume export")
    printf '%s' fixture-archive > "$4"
    : > "$PODMAN_TEST_MARKER"
    exit "${PODMAN_TEST_EXPORT_STATUS:-0}";;
  "pod exists") exit 0;;
  "run --detach") printf '%s\n' fixture-container;;
  "start qkbot") exit 0;;
  "stop --time") exit 0;;
  *) exit 91;;
esac
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self { directory }
    }

    fn command(&self, action: &str, output: &Path) -> Command {
        let mut command = shell();
        clean_environment(&mut command);
        for key in [
            "QKBOT_IMAGE",
            "QKBOT_CONTAINER",
            "QKBOT_VOLUME",
            "QKBOT_NETWORK",
            "QKBOT_POD",
            "QKBOT_STOP_TIMEOUT",
            "QKBOT_API_SECRET",
            "QKBOT_MILKY_SECRET",
            "QKBOT_LLBOT_SECRET",
            "QKBOT_NONEBOT_SECRET",
        ] {
            command.env_remove(key);
        }
        command
            .args([
                "-c",
                "PATH=\"$(cd \"$1\" && pwd):$PATH\"; export PATH; exec sh \"$2\" \"$3\" \"$4\"",
                "fixture",
            ])
            .arg(shell_path(&self.directory.path().join("bin")))
            .arg(shell_path(&root().join("deploy/podman.sh")))
            .arg(action)
            .arg(shell_path(output))
            .env(
                "PODMAN_TEST_LOG",
                shell_path(&self.directory.path().join("calls")),
            )
            .env(
                "PODMAN_TEST_MARKER",
                shell_path(&self.directory.path().join("exported")),
            );
        command
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.directory.path().join("calls")).unwrap()
    }
}

#[test]
fn backup_uses_actual_stopped_volume_and_never_overwrites_or_publishes_failure() {
    let fixture = PodmanFixture::new();
    let output = fixture.directory.path().join("completed.tar");
    success(
        fixture
            .command("backup", &output)
            .env("QKBOT_VOLUME", "unrelated-volume")
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read(&output).unwrap(), b"fixture-archive");
    assert!(fixture.calls().contains("actual-persistent-volume"));
    assert!(!fixture.calls().contains("unrelated-volume"));
    assert!(
        !fixture
            .command("backup", &output)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(fs::read(&output).unwrap(), b"fixture-archive");

    for variable in [
        "PODMAN_TEST_EXPORT_STATUS",
        "PODMAN_TEST_AFTER_RUNNING",
        "PODMAN_TEST_AFTER_STARTED",
    ] {
        let fixture = PodmanFixture::new();
        let output = fixture.directory.path().join("incomplete.tar");
        let value = match variable {
            "PODMAN_TEST_EXPORT_STATUS" => "1",
            "PODMAN_TEST_AFTER_RUNNING" => "true",
            _ => "restarted-and-stopped",
        };
        assert!(
            !fixture
                .command("backup", &output)
                .env(variable, value)
                .output()
                .unwrap()
                .status
                .success()
        );
        assert!(!output.exists());
        assert!(
            !fs::read_dir(fixture.directory.path())
                .unwrap()
                .any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".qkbot-backup-")
                })
        );
    }
}

#[test]
fn lifecycle_refuses_unowned_containers_and_live_backups() {
    for action in ["start", "stop", "backup"] {
        let fixture = PodmanFixture::new();
        let output = fixture.directory.path().join("archive.tar");
        assert!(
            !fixture
                .command(action, &output)
                .env("PODMAN_TEST_EXISTS", "0")
                .env("PODMAN_TEST_OWNER", "some-other-service")
                .output()
                .unwrap()
                .status
                .success()
        );
        let calls = fixture.calls();
        assert!(!calls.lines().any(|line| line.starts_with("start ")));
        assert!(!calls.lines().any(|line| line.starts_with("stop ")));
        assert!(!calls.lines().any(|line| line.starts_with("volume export ")));
    }
    let fixture = PodmanFixture::new();
    assert!(
        !fixture
            .command("backup", &fixture.directory.path().join("archive.tar"))
            .env("PODMAN_TEST_RUNNING", "true")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(!fixture.calls().contains("volume export"));
}

#[test]
fn host_and_pod_start_keep_dedicated_volume_and_long_shutdown_timeout() {
    for mode in ["host", "pod"] {
        let fixture = PodmanFixture::new();
        success(
            fixture
                .command("start", Path::new(""))
                .env("QKBOT_NETWORK", mode)
                .output()
                .unwrap(),
        );
        let calls = fixture.calls();
        assert!(calls.contains("volume create --ignore qkbot-data"));
        assert!(calls.contains("--volume qkbot-data:/data"));
        assert!(calls.contains("--stop-timeout 600"));
        assert!(calls.contains("--read-only"));
        assert!(calls.contains("--security-opt no-new-privileges"));
        assert!(calls.contains(&format!("QKBOT_CONTAINER_NETWORK={mode}")));
        if mode == "host" {
            assert!(calls.contains("--network host"));
        } else {
            assert!(calls.contains("--pod qkbot"));
            assert!(!calls.contains("--network host"));
        }
    }
}
