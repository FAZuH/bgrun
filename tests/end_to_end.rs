//! End-to-end tests: the real binary against PATH shims for the systemd
//! tools. Each shim appends its argv to a log file the assertions read, so
//! tests pin down the exact subprocess call shapes bgrun produces.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

const SYSTEMD_RUN_ALREADY_EXISTS: &str =
    "Failed to start transient service unit: Unit bgrun-dup.service already exists.";

/// A throwaway directory with executable shims for every systemd tool.
struct Sandbox {
    root: PathBuf,
    log: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "bgrun-e2e-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(root.join("bin")).unwrap();
        let sandbox = Self {
            log: root.join("calls.log"),
            root,
        };
        fs::write(&sandbox.log, "").unwrap();
        sandbox.shim(
            "systemctl",
            "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\n",
        );
        sandbox.shim(
            "journalctl",
            "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\n",
        );
        sandbox.shim(
            "systemd-run",
            "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\necho 'Running as unit: test.service.'\n",
        );
        sandbox.shim(
            "loginctl",
            "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\necho yes\n",
        );
        sandbox
    }

    fn shim(&self, name: &str, body: &str) {
        let path = self.root.join("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }

    /// Make a shim fail the way `systemd-run` does for a duplicate unit.
    fn fail_duplicate(&self) {
        self.shim(
            "systemd-run",
            &format!(
                "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\necho '{SYSTEMD_RUN_ALREADY_EXISTS}' >&2\nexit 1\n"
            ),
        );
    }

    /// Make a shim fail with an arbitrary message.
    fn fail_with(&self, name: &str, message: &str) {
        self.shim(
            name,
            &format!(
                "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\necho '{message}' >&2\nexit 1\n"
            ),
        );
    }

    /// Make `loginctl` report that lingering is off.
    fn linger_disabled(&self) {
        self.shim(
            "loginctl",
            "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\necho no\n",
        );
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }

    fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bgrun"));
        command
            .args(args)
            .env("PATH", self.root.join("bin"))
            .env("BGRUN_TEST_LOG", &self.log)
            .env_remove("BGRUN_PREFIX")
            .env("USER", "tester");
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().expect("failed to run bgrun")
    }

    /// Every shim invocation, one `"<program> <argv…>"` per line.
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn calls_containing(&self, needle: &str) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|line| line.contains(needle))
            .collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("shim was killed by a signal")
}

// ----------------------------------------------------------------- run --

#[test]
fn run_passes_exact_argv_to_systemd_run() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--", "sleep", "5"]);

    assert_eq!(code(&output), 0);
    assert!(stdout(&output).contains("Running as unit"));
    assert!(
        sandbox
            .calls_containing("systemd-run --user --unit=bgrun-sleep.service --collect sleep 5")
            .len()
            == 1,
        "calls: {:?}",
        sandbox.calls()
    );
    assert!(
        !stderr(&output).contains("warning"),
        "no linger warning when enabled"
    );
}

#[test]
fn run_derives_name_from_command_basename() {
    let sandbox = Sandbox::new();
    sandbox.run(&["--", "/usr/bin/make", "-j4"]);
    assert!(sandbox.calls_containing("--unit=bgrun-make.service").len() == 1);
}

#[test]
fn run_warns_when_lingering_is_disabled() {
    let sandbox = Sandbox::new();
    sandbox.linger_disabled();

    let output = sandbox.run(&["--", "sleep", "5"]);

    assert_eq!(code(&output), 0);
    let err = stderr(&output);
    assert!(err.contains("lingering is disabled"), "stderr: {err}");
    assert!(err.contains("loginctl enable-linger"), "stderr: {err}");
    assert!(
        sandbox
            .calls_containing("loginctl show-user tester --property=Linger --value")
            .len()
            == 1
    );
}

#[test]
fn duplicate_name_prints_friendly_hint_without_racing() {
    let sandbox = Sandbox::new();
    sandbox.fail_duplicate();

    let output = sandbox.run(&["add", "dup", "--", "sleep", "5"]);

    assert_eq!(code(&output), 1);
    let err = stderr(&output);
    assert!(
        err.contains("bgrun-dup.service already exists (running or failed)"),
        "stderr: {err}"
    );
    assert!(err.contains("bgrun logs dup"), "stderr: {err}");
    assert!(err.contains("bgrun remove dup"), "stderr: {err}");
}

#[test]
fn other_systemd_run_failures_pass_through() {
    let sandbox = Sandbox::new();
    sandbox.fail_with("systemd-run", "some other systemd error");

    let output = sandbox.run(&["--", "sleep", "5"]);

    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("some other systemd error"));
    assert!(!stderr(&output).contains("already exists (running or failed)"));
}

#[test]
fn add_forwards_overrides_verbatim() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["add", "build", "-p", "WorkingDirectory=/tmp", "--", "make"]);

    assert_eq!(code(&output), 0);
    assert!(sandbox
        .calls_containing(
            "systemd-run --user --unit=bgrun-build.service --collect -p WorkingDirectory=/tmp make"
        )
        .len() == 1);
}

// ------------------------------------------------------- introspection --

#[test]
fn list_queries_all_units_with_the_prefix_glob() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["list"]);

    assert_eq!(code(&output), 0);
    assert!(
        sandbox
            .calls_containing("systemctl --user list-units bgrun-*.service --all --no-pager")
            .len()
            == 1
    );
}

#[test]
fn status_targets_the_unit() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["status", "web"]);

    assert_eq!(code(&output), 0);
    assert!(
        sandbox
            .calls_containing("systemctl --user status bgrun-web.service")
            .len()
            == 1
    );
}

#[test]
fn logs_forwards_journalctl_options() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["logs", "web", "-n", "50"]);

    assert_eq!(code(&output), 0);
    assert!(
        sandbox
            .calls_containing("journalctl --user -u bgrun-web.service -n 50")
            .len()
            == 1
    );
}

#[test]
fn remove_stops_and_resets_each_name_in_order() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["remove", "a", "b"]);

    assert_eq!(code(&output), 0);
    let calls = sandbox.calls();
    let position = |needle: &str| {
        calls
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing call containing {needle:?} in {calls:?}"))
    };
    assert!(
        position("systemctl --user stop bgrun-a.service")
            < position("systemctl --user reset-failed bgrun-a.service")
    );
    assert!(
        position("systemctl --user reset-failed bgrun-a.service")
            < position("systemctl --user stop bgrun-b.service")
    );
    assert_eq!(stdout(&output).matches("removed: bgrun-").count(), 2);
}

#[test]
fn clean_resets_the_prefix_glob() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["clean"]);

    assert_eq!(code(&output), 0);
    assert!(
        sandbox
            .calls_containing("systemctl --user reset-failed bgrun-*.service")
            .len()
            == 1
    );
    assert!(stdout(&output).contains("failed bgrun units"));
    assert!(stdout(&output).contains("collected automatically"));
}

// ----------------------------------------------------------- prefixes --

#[test]
fn custom_prefix_changes_unit_names() {
    let sandbox = Sandbox::new();
    let output = sandbox.run_with(
        &["add", "dl", "--", "wget", "x"],
        &[("BGRUN_PREFIX", "jobs")],
    );

    assert_eq!(code(&output), 0);
    assert!(sandbox.calls_containing("--unit=jobs-dl.service").len() == 1);

    sandbox.run_with(&["clean"], &[("BGRUN_PREFIX", "jobs")]);
    assert!(
        sandbox
            .calls_containing("systemctl --user reset-failed jobs-*.service")
            .len()
            == 1
    );
}

#[test]
fn invalid_prefix_is_rejected() {
    let sandbox = Sandbox::new();
    let output = sandbox.run_with(&["list"], &[("BGRUN_PREFIX", "bg[run]")]);

    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("invalid BGRUN_PREFIX"));
    assert!(
        sandbox.calls().is_empty(),
        "nothing may run with a bad prefix"
    );
}

// ------------------------------------------------------- usage errors --

#[test]
fn help_flag_exits_successfully() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--help"]);

    assert_eq!(code(&output), 0);
    let out = stdout(&output);
    assert!(out.contains("Usage:"));
    // Claude review #1 and #3: the help text itself must not lie.
    assert!(out.contains("loginctl enable-linger"));
    assert!(out.contains("only clears units that exited non-zero"));
    assert!(sandbox.calls().is_empty());
}

#[test]
fn unknown_command_exits_with_usage_code() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["wat"]);

    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("unknown command 'wat'"));
}

#[test]
fn missing_separator_exits_with_usage_code() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["add", "x"]);

    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("missing '--'"));
}

#[test]
fn empty_invocation_shows_usage() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&[]);

    assert_eq!(code(&output), 0);
    assert!(stdout(&output).contains("Usage:"));
}
