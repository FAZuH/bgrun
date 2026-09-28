//! End-to-end tests: the real binary against PATH shims for the systemd
//! tools. Each shim appends its argv to a log file the assertions read, so
//! tests pin down the exact subprocess call shapes bgrun produces.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

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

    /// Make a shim fail for one verb only, so the steps around it still run.
    fn fail_verb(&self, name: &str, verb: &str, message: &str) {
        self.shim(
            name,
            &format!(
                "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\ncase \" $* \" in *' {verb} '*) echo '{message}' >&2; exit 1;; esac\n"
            ),
        );
    }

    /// Make `systemctl show <unit> -p UnitFileState` report `state`.
    fn unit_state(&self, state: &str) {
        self.shim(
            "systemctl",
            &format!(
                "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\ncase \"$*\" in *UnitFileState*) echo {state}; exit 0;; esac\n"
            ),
        );
    }

    /// Make `systemctl show` report a unit the manager no longer has, so both
    /// the existence check and the result query see it as gone.
    fn unit_collected(&self) {
        self.shim(
            "systemctl",
            "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\n\
             case \"$*\" in\n\
             *LoadState*) echo not-found; echo inactive; exit 0;;\n\
             *Result*) echo success; echo 0; exit 0;;\n\
             esac\n",
        );
    }

    /// A journal that records the runs of one name the way journald does: a
    /// `Started` record opening each run, and for a run that did not succeed
    /// the two exit fields on two separate records. `runs` is written
    /// newest-first, because that is the order bgrun has to read them in.
    fn journal_runs(&self, runs: &[(&str, Option<&str>, Option<&str>)]) {
        // The record body is written with `echo` so the shim needs nothing from
        // PATH. `runs` is given newest-first, and the shim replays it in that
        // order ONLY when bgrun asked for it with `-r` — otherwise it replays
        // oldest-first, the way journalctl does without that flag. A test that
        // needs the newest run therefore fails if `-r` is ever dropped, rather
        // than passing because the shim handed over the right order anyway.
        let mut records: Vec<String> = Vec::new();
        for (message, result, status) in runs {
            // systemd's "Started <unit>." opens a run and carries this
            // MESSAGE_ID; bgrun stops reading at it.
            if message.starts_with("Started") {
                records.push("MESSAGE_ID=39f53479d3a045ac8e11786248231fbf".to_owned());
            }
            records.push(format!("MESSAGE={message}"));
            if let Some(code) = result {
                records.push("MESSAGE_ID=d9b373ed55a64feb8242e02dbe79a49c".to_owned());
                records.push(format!("UNIT_RESULT={code}"));
            }
            if let Some(exit) = status {
                records.push("MESSAGE_ID=98e322203f7a4ed290d09fe03c09fe15".to_owned());
                records.push(format!("EXIT_STATUS={exit}"));
            }
        }
        // One file per record, with the traversal order baked into the shim:
        // forward without `-r`, reversed with it. Pure shell builtins, because
        // a test's PATH holds the shim directory and nothing else — no `sort`,
        // no `cut`, no `cat`.
        let store = self.root.join("journal.rows");
        fs::create_dir_all(&store).unwrap();
        for (index, record) in records.iter().enumerate() {
            // The trailing newline matters: `read` fails at EOF without one,
            // and the shim emits a row only when `read` succeeds.
            fs::write(store.join(index.to_string()), format!("{record}\n")).unwrap();
        }
        let forward: Vec<String> = (0..records.len()).map(|i| i.to_string()).collect();
        let reversed: Vec<String> = (0..records.len()).rev().map(|i| i.to_string()).collect();
        let emit = |order: &[String]| {
            let mut body = String::new();
            for index in order {
                body.push_str(&format!(
                    "if read -r line < {store}/{index}; then echo \"$line\"; fi\n",
                    store = store.display()
                ));
            }
            body
        };
        self.shim(
            "journalctl",
            &format!(
                "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\n\
                 case \"$*\" in\n\
                 *SYSLOG_IDENTIFIER*)\n\
                 case \" $* \" in\n\
                 *' -r '*){}exit 0;;\n\
                 *){}exit 0;;\n\
                 esac\n\
                 exit 0;;\n\
                 esac\n",
                // `runs` is newest-first, so the stored order IS the `-r`
                // order; without the flag journalctl replays it oldest-first.
                emit(&forward),
                emit(&reversed)
            ),
        );
    }

    /// Make every `journalctl` query fail, the way a broken journal socket does.
    fn journal_broken(&self) {
        self.shim(
            "journalctl",
            "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\nexit 1\n",
        );
    }

    /// A job's result reaches us from whichever source still holds it, so the
    /// shims have to move through the same states a real job does: running,
    /// then ended — either still loaded (persisted) or already collected
    /// (transient, which is what `--collect` does the moment a job stops).
    fn job_that_ends(&self, name: &str, result: &str, status: &str, collected: bool) -> Output {
        self.running_then_ended(result, status, collected);
        self.journal_exit(result, status);
        self.run(&["watch", name])
    }

    /// A unit that answers `active` to the first lookup and `inactive` to every
    /// one after, which is the whole life of a job as far as a shim can see it.
    /// The state lives in a file, so it is read and written with shell
    /// builtins: a test's PATH holds the shim directory and nothing else, so
    /// `cat` and friends are not there.
    fn running_then_ended(&self, result: &str, status: &str, collected: bool) {
        let counter = self.root.join("polls");
        let load = if collected { "not-found" } else { "loaded" };
        self.shim(
            "systemctl",
            &format!(
                "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\n\
                 case \"$*\" in\n\
                 *LoadState*)\n\
                   if [ -s {counter} ]; then echo {load}; echo inactive\n\
                   else echo 1 > {counter}; echo loaded; echo active; fi\n\
                   exit 0;;\n\
                 *Result*) echo {result}; echo {status}; exit 0;;\n\
                 esac\n",
                counter = counter.display()
            ),
        );
    }

    /// A single-run journal, for the tests that do not care about history. The
    /// two fields sit on separate records, the way systemd writes them.
    fn journal_exit(&self, result: &str, status: &str) {
        self.journal_runs(&[
            ("Failed with result.", Some(result), None),
            ("Main process exited.", None, Some(status)),
            ("Started.", None, None),
        ]);
    }

    /// A follower that records its own pid and then blocks, so a test can
    /// interrupt `watch` while a real `journalctl -f` child is alive. It needs
    /// a real sleep loop, so it uses an absolute `sleep` — a test's PATH holds
    /// the shim directory and nothing else.
    fn follower_that_hangs(&self) -> PathBuf {
        let pidfile = self.root.join("follower.pid");
        self.shim(
            "journalctl",
            &format!(
                "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\n\
                 echo $$ > {pidfile}\n\
                 while :; do /usr/bin/sleep 1; done\n",
                pidfile = pidfile.display()
            ),
        );
        pidfile
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

    /// An executable inside the sandbox's bin, which is all a test's `PATH`
    /// contains: a bare command name a persisted job can resolve to an
    /// absolute path. Returns that path for the `ExecStart=` assertion.
    fn job_tool(&self, name: &str) -> PathBuf {
        self.shim(name, "exit 0\n");
        self.root.join("bin").join(name)
    }

    /// Where a persisted job's unit file lands: bgrun resolves
    /// `$XDG_CONFIG_HOME/systemd/user`, which points into the sandbox.
    fn unit_file(&self, name: &str) -> PathBuf {
        self.root.join("systemd").join("user").join(name)
    }

    fn write_unit_file(&self, name: &str) {
        let path = self.unit_file(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[Service]\nExecStart=true\n").unwrap();
    }

    fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bgrun"));
        command
            .args(args)
            .env("PATH", self.root.join("bin"))
            .env("BGRUN_TEST_LOG", &self.log)
            .env_remove("BGRUN_PREFIX")
            .env("XDG_CONFIG_HOME", &self.root)
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

#[test]
fn restart_becomes_a_systemd_property_on_the_transient_unit() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["add", "--restart", "--", "sleep", "5"]);

    assert_eq!(code(&output), 0);
    assert!(
        sandbox
            .calls_containing(
                "systemd-run --user --unit=bgrun-sleep.service --collect -p Restart=on-failure sleep 5"
            )
            .len()
            == 1,
        "calls: {:?}",
        sandbox.calls()
    );
}

#[test]
fn bare_run_accepts_the_same_flags() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--restart", "-p", "MemoryMax=1G", "--", "sleep", "5"]);

    assert_eq!(code(&output), 0);
    assert!(
        sandbox
            .calls_containing(
                "systemd-run --user --unit=bgrun-sleep.service --collect -p Restart=on-failure -p MemoryMax=1G sleep 5"
            )
            .len()
            == 1,
        "calls: {:?}",
        sandbox.calls()
    );
}

// ---------------------------------------------------------- persistence --

#[test]
fn persist_writes_an_enabled_unit_file_instead_of_running_systemd_run() {
    let sandbox = Sandbox::new();
    let tool = sandbox.job_tool("bgrun-job-tool");
    let output = sandbox.run(&[
        "add",
        "build",
        "--persist",
        "-p",
        "MemoryMax=1G",
        "--",
        "bgrun-job-tool",
    ]);

    assert_eq!(code(&output), 0);
    let body = fs::read_to_string(sandbox.unit_file("bgrun-build.service"))
        .expect("unit file was written");
    assert!(
        body.contains(&format!("ExecStart=\"{}\"\n", tool.display())),
        "{body}"
    );
    assert!(body.contains("MemoryMax=1G\n"), "{body}");
    assert!(body.contains("WantedBy=default.target\n"), "{body}");

    let calls = sandbox.calls();
    let position = |needle: &str| {
        calls
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing call containing {needle:?} in {calls:?}"))
    };
    assert!(
        position("systemctl --user daemon-reload")
            < position("systemctl --user enable --now bgrun-build.service")
    );
    assert!(
        calls.iter().all(|line| !line.contains("systemd-run")),
        "a persisted job is a unit file, not a transient unit: {calls:?}"
    );
    assert!(stdout(&output).contains("persisted: bgrun-build.service"));
}

#[test]
fn persist_refuses_to_shadow_a_running_transient_job() {
    let sandbox = Sandbox::new();
    sandbox.unit_state("transient");
    sandbox.job_tool("bgrun-job-tool");

    let output = sandbox.run(&["add", "api", "--persist", "--", "bgrun-job-tool"]);

    assert_eq!(code(&output), 1);
    let err = stderr(&output);
    assert!(
        err.contains("bgrun-api.service is already running as a transient job"),
        "stderr: {err}"
    );
    assert!(err.contains("bgrun remove api"), "stderr: {err}");
    assert!(!sandbox.unit_file("bgrun-api.service").exists());
    assert!(sandbox.calls_containing("enable").is_empty());
}

#[test]
fn persist_may_replace_an_already_persisted_job() {
    let sandbox = Sandbox::new();
    let tool = sandbox.job_tool("bgrun-job-tool");
    sandbox.write_unit_file("bgrun-api.service");
    sandbox.unit_state("enabled");

    let output = sandbox.run(&["add", "api", "--persist", "--", "bgrun-job-tool", "-j8"]);

    assert_eq!(code(&output), 0);
    let body = fs::read_to_string(sandbox.unit_file("bgrun-api.service")).expect("rewritten");
    assert!(
        body.contains(&format!("ExecStart=\"{}\" \"-j8\"\n", tool.display())),
        "{body}"
    );
}

#[test]
fn persist_refuses_a_command_the_unit_manager_could_not_run() {
    // The sandbox's PATH is only the shim dir, so this name is in nobody's
    // PATH: a unit file would fail with 203/EXEC at every boot instead.
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["add", "api", "--persist", "--", "bgrun-absent-tool"]);

    assert_eq!(code(&output), 2);
    assert!(
        stderr(&output).contains("not an executable in PATH"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(
        !sandbox
            .unit_file("bgrun-bgrun-absent-tool.service")
            .exists()
    );
    assert!(
        sandbox.calls_containing("systemctl").is_empty(),
        "nothing may be wired in when the command cannot run: {:?}",
        sandbox.calls()
    );
}

#[test]
fn persist_rejects_an_override_it_cannot_write_before_touching_disk() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["add", "--persist", "--working-directory=/tmp", "--", "make"]);

    assert_eq!(code(&output), 2);
    assert!(
        stderr(&output).contains("only -p KEY=VALUE overrides"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(!sandbox.unit_file("bgrun-make.service").exists());
    assert!(sandbox.calls_containing("systemctl").is_empty());
    assert!(sandbox.calls_containing("systemd-run").is_empty());
}

#[test]
fn a_unit_systemd_refuses_is_not_left_wired_into_every_boot() {
    let sandbox = Sandbox::new();
    sandbox.fail_verb("systemctl", "enable", "Failed to prepare unit");
    sandbox.job_tool("bgrun-job-tool");

    let output = sandbox.run(&["add", "build", "--persist", "--", "bgrun-job-tool"]);

    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("Failed to prepare unit"));
    assert!(
        !sandbox.unit_file("bgrun-build.service").exists(),
        "a refused unit file must not be left behind to fail at every boot"
    );
    assert!(
        sandbox
            .calls_containing("systemctl --user disable bgrun-build.service")
            .len()
            == 1
    );
}

#[test]
fn remove_deletes_the_unit_file_of_a_persisted_job() {
    let sandbox = Sandbox::new();
    sandbox.write_unit_file("bgrun-build.service");

    let output = sandbox.run(&["remove", "build"]);

    assert_eq!(code(&output), 0);
    assert!(!sandbox.unit_file("bgrun-build.service").exists());
    assert!(
        sandbox
            .calls_containing("systemctl --user disable bgrun-build.service")
            .len()
            == 1
    );
}

#[test]
fn remove_says_nothing_about_transient_jobs() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["remove", "a"]);

    assert_eq!(code(&output), 0);
    let calls = sandbox.calls();
    assert_eq!(
        calls.len(),
        2,
        "a transient job has no unit file to disable: {calls:?}"
    );
    assert!(
        calls[0].ends_with("systemctl --user stop bgrun-a.service"),
        "{calls:?}"
    );
    assert!(
        calls[1].ends_with("systemctl --user reset-failed bgrun-a.service"),
        "{calls:?}"
    );
}

#[test]
fn stop_keeps_the_job_and_resume_starts_it_again() {
    let sandbox = Sandbox::new();
    sandbox.write_unit_file("bgrun-build.service");

    let output = sandbox.run(&["stop", "build"]);

    assert_eq!(code(&output), 0);
    assert!(stdout(&output).contains("stopped: bgrun-build.service"));
    assert!(
        sandbox.unit_file("bgrun-build.service").exists(),
        "stop must not forget a persisted job"
    );
    let calls = sandbox.calls();
    assert_eq!(calls.len(), 1, "stop is one systemctl call: {calls:?}");
    assert!(
        calls[0].ends_with("systemctl --user stop bgrun-build.service"),
        "{calls:?}"
    );

    let output = sandbox.run(&["resume", "build"]);

    assert_eq!(code(&output), 0);
    assert!(stdout(&output).contains("resumed: bgrun-build.service"));
    assert!(
        sandbox
            .calls_containing("systemctl --user start bgrun-build.service")
            .len()
            == 1
    );
}

#[test]
fn a_stop_systemd_refuses_is_reported_as_a_failure() {
    let sandbox = Sandbox::new();
    // One named unit fails; the loop must still reach the next name.
    sandbox.shim(
        "systemctl",
        "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\ncase \"$*\" in *bgrun-a.service) echo 'Failed to stop' >&2; exit 1;; esac\n",
    );

    let output = sandbox.run(&["stop", "a", "b"]);

    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("Failed to stop"));
    assert!(!stdout(&output).contains("stopped: bgrun-a.service"));
    assert!(
        stdout(&output).contains("stopped: bgrun-b.service"),
        "one bad name must not skip the rest"
    );
}

// ------------------------------------------------------------- watch --

#[test]
fn watch_reports_a_clean_exit() {
    let sandbox = Sandbox::new();
    let output = sandbox.job_that_ends("build", "success", "0", false);

    assert_eq!(code(&output), 0);
    let out = stdout(&output);
    assert!(out.contains("bgrun-build.service"), "{out}");
    assert!(out.contains("finished successfully"), "{out}");
    assert!(
        sandbox
            .calls_containing("journalctl --user -u bgrun-build.service --follow")
            .len()
            == 1,
        "watch is the tail: {:?}",
        sandbox.calls()
    );
}

#[test]
fn watch_exits_with_the_jobs_own_exit_code() {
    let sandbox = Sandbox::new();
    // A persisted unit stays loaded, so its ExecMainStatus is the outcome.
    let output = sandbox.job_that_ends("build", "exit-code", "3", false);

    assert_eq!(code(&output), 3, "watch must propagate the job's status");
    assert!(
        stdout(&output).contains("exit code 3"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn watch_reads_a_collected_jobs_outcome_from_the_journal() {
    let sandbox = Sandbox::new();
    // A transient `--collect` unit is unloaded the moment the job ends, so the
    // unit cannot answer and the journal's exit record has to.
    let output = sandbox.job_that_ends("dl", "exit-code", "3", true);

    assert_eq!(code(&output), 3);
    assert!(
        stdout(&output).contains("exit code 3"),
        "{}",
        stdout(&output)
    );
    assert!(
        sandbox
            .calls_containing("journalctl --user -u bgrun-dl.service SYSLOG_IDENTIFIER=systemd")
            .len()
            == 1,
        "a collected unit must be read from the journal: {:?}",
        sandbox.calls()
    );
}

#[test]
fn watch_maps_a_signal_to_128_plus_the_signal() {
    let sandbox = Sandbox::new();
    let output = sandbox.job_that_ends("api", "signal", "9", true);

    assert_eq!(code(&output), 137, "SIGKILL must map to 128+9");
    assert!(
        stdout(&output).contains("killed by signal 9"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn the_exit_query_asks_for_the_newest_records_and_bounds_itself() {
    let sandbox = Sandbox::new();
    sandbox.job_that_ends("dl", "exit-code", "3", true);

    // The journal shim replays oldest-first unless bgrun passes `-r`, so the
    // tests that need the newest run only pass because of this flag. Assert it
    // directly too, so dropping `-r` cannot pass unnoticed.
    let query = sandbox
        .calls_containing("SYSLOG_IDENTIFIER=systemd")
        .into_iter()
        .next()
        .expect("a collected unit must be read from the journal");
    assert!(query.contains(" -r "), "must read newest-first: {query}");
    assert!(
        query.contains(" -n 20 "),
        "must bound the read to the current run: {query}"
    );
}

#[test]
fn watch_reports_the_newest_run_not_an_older_failure() {
    let sandbox = Sandbox::new();
    // Newest-first, as `-r` delivers: this run succeeded (no exit fields), the
    // run before it exited 3. Reading oldest-first would report that stale 3.
    sandbox.running_then_ended("success", "0", true);
    // Newest-first: this run opened at the top `Started` and succeeded, so it
    // left no exit fields. The run below it exited 3. Reading the whole unit's
    // history would report that stale 3.
    sandbox.journal_runs(&[
        ("Started bgrun-build.service.", None, None),
        (
            "bgrun-build.service: Failed with result 'exit-code'.",
            Some("exit-code"),
            None,
        ),
        (
            "bgrun-build.service: Main process exited, code=exited, status=3/NOTIMPLEMENTED",
            None,
            Some("3"),
        ),
        ("Started bgrun-build.service.", None, None),
    ]);
    let output = sandbox.run(&["watch", "build"]);

    assert_eq!(
        code(&output),
        0,
        "the newest run succeeded: {}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("finished successfully"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn watch_reads_a_failure_that_is_newer_than_an_older_success() {
    let sandbox = Sandbox::new();
    // The other order: the latest run exited 3, the one before it succeeded and
    // so left no exit fields behind. Only the records above the newest
    // `Started` may be read, and here they are the only ones with any.
    sandbox.running_then_ended("success", "0", true);
    // Newest-first. The current run failed with 3 and opens at the middle
    // `Started`; the run before it exited 9, and its records sit below that
    // marker, so reading past it would report the stale 9.
    sandbox.journal_runs(&[
        (
            "bgrun-build.service: Failed with result 'exit-code'.",
            Some("exit-code"),
            None,
        ),
        (
            "bgrun-build.service: Main process exited, code=exited, status=3/NOTIMPLEMENTED",
            None,
            Some("3"),
        ),
        ("Started bgrun-build.service.", None, None),
        (
            "bgrun-build.service: Failed with result 'exit-code'.",
            Some("exit-code"),
            None,
        ),
        (
            "bgrun-build.service: Main process exited, code=exited, status=9/NOTIMPLEMENTED",
            None,
            Some("9"),
        ),
        ("Started bgrun-build.service.", None, None),
    ]);
    let output = sandbox.run(&["watch", "build"]);

    assert_eq!(
        code(&output),
        3,
        "the newest run's status, not the older 9: {}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("exit code 3"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_broken_journal_is_an_error_not_a_clean_exit() {
    let sandbox = Sandbox::new();
    // A collected unit can only be resolved from the journal, so a journalctl
    // that fails must not be read as "the job exited 0".
    sandbox.journal_broken();
    sandbox.running_then_ended("success", "0", true);
    let output = sandbox.run(&["watch", "dl"]);

    assert_eq!(code(&output), 1, "a journal failure is not a success");
    let out = stdout(&output);
    // No success wording, and the detail must name the journal as the thing
    // that could not be read — the previous code also exited 1, but claimed
    // the job had finished successfully.
    assert!(
        !out.contains("finished successfully"),
        "a journalctl failure must never report a result: {out}"
    );
    assert!(
        !out.contains("exit code"),
        "a journalctl failure must not invent a status: {out}"
    );
    assert!(
        out.contains("journal"),
        "the detail must name the journal as unreadable: {out}"
    );
}

#[test]
fn losing_systemd_mid_wait_is_an_error_not_an_ended_job() {
    let sandbox = Sandbox::new();
    // Every lookup after the first fails, as a D-Bus hiccup would. Reading that
    // as "no longer running" would report a result for a job still running.
    sandbox.shim(
        "systemctl",
        "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\n\
         case \"$*\" in\n\
         *LoadState*)\n\
           if [ -e \"$BGRUN_TEST_LOG.seen\" ]; then exit 1; fi\n\
           : > \"$BGRUN_TEST_LOG.seen\"; echo loaded; echo active; exit 0;;\n\
         *Result*) echo success; echo 0; exit 0;;\n\
         esac\n",
    );

    let output = sandbox.run(&["watch", "web"]);

    assert_eq!(code(&output), 1, "losing systemd is not a result");
    let err = stderr(&output);
    assert!(err.contains("lost contact with systemd"), "stderr: {err}");
    assert!(
        !stdout(&output).contains("finished successfully"),
        "must not claim the job ended: {}",
        stdout(&output)
    );
}

#[test]
fn watch_refuses_a_name_that_is_not_a_job() {
    let sandbox = Sandbox::new();
    sandbox.unit_collected();
    // No journal records either, so the name was never used.
    sandbox.shim("journalctl", "exit 0\n");

    let output = sandbox.run(&["watch", "nope"]);

    assert_eq!(code(&output), 1);
    let err = stderr(&output);
    assert!(
        err.contains("no such job: bgrun-nope.service"),
        "stderr: {err}"
    );
    assert!(err.contains("bgrun list"), "stderr: {err}");
    assert!(
        sandbox.calls_containing("--follow").is_empty(),
        "nothing may follow a job that does not exist: {:?}",
        sandbox.calls()
    );
}

#[test]
fn a_collected_job_points_at_its_journal_instead_of_hanging() {
    let sandbox = Sandbox::new();
    sandbox.unit_collected();
    // A job that ran and was collected still has a readable journal.
    sandbox.shim("journalctl", "echo 'some earlier output'\n");

    let output = sandbox.run(&["watch", "done"]);

    assert_eq!(code(&output), 1);
    let err = stderr(&output);
    assert!(
        err.contains("has already finished and was collected"),
        "stderr: {err}"
    );
    assert!(err.contains("bgrun logs done"), "stderr: {err}");
    assert!(
        sandbox.calls_containing("--follow").is_empty(),
        "{:?}",
        sandbox.calls()
    );
}

#[test]
fn watch_refuses_a_job_that_is_not_running() {
    let sandbox = Sandbox::new();
    sandbox.shim(
        "systemctl",
        "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\n\
         case \"$*\" in\n\
         *LoadState*) echo loaded; echo inactive; exit 0;;\n\
         esac\n",
    );

    let output = sandbox.run(&["watch", "web"]);

    assert_eq!(code(&output), 1);
    let err = stderr(&output);
    assert!(
        err.contains("bgrun-web.service is not running (inactive)"),
        "stderr: {err}"
    );
    assert!(err.contains("bgrun resume web"), "stderr: {err}");
    assert!(
        sandbox.calls_containing("--follow").is_empty(),
        "a stopped job must not be followed: {:?}",
        sandbox.calls()
    );
}

#[test]
fn a_broken_journal_does_not_make_a_job_look_never_used() {
    let sandbox = Sandbox::new();
    sandbox.unit_collected();
    // journalctl fails with empty stdout, which the old check read as "this
    // name was never used" — a different, wrong answer.
    sandbox.journal_broken();

    let output = sandbox.run(&["watch", "nope"]);

    let err = stderr(&output);
    assert!(
        !err.contains("no such job"),
        "a journal that cannot answer proves nothing: stderr: {err}"
    );
    assert!(
        err.contains("cannot read the journal"),
        "must say the journal was unreadable, not guess: stderr: {err}"
    );
}

/// An interrupt must produce no result. It also has to take the follower with
/// it, which this pins only for a signal aimed at the process group — the
/// terminal case. A signal aimed at bgrun's pid alone still leaves the
/// follower, and that ceiling is documented on `Follower`.
#[test]
fn interrupting_watch_prints_no_result_and_takes_the_follower_with_it() {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let sandbox = Sandbox::new();
    let pidfile = sandbox.follower_that_hangs();
    // The unit never leaves `active`, so the only way out is the interrupt.
    sandbox.shim(
        "systemctl",
        "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\n\
         case \"$*\" in\n\
         *LoadState*) echo loaded; echo active; exit 0;;\n\
         *Result*) echo success; echo 0; exit 0;;\n\
         esac\n",
    );

    // Its own process group, so the interrupt reaches bgrun and the follower
    // together, exactly as a terminal Ctrl-C does.
    let mut child = Command::new(env!("CARGO_BIN_EXE_bgrun"))
        .args(["watch", "web"])
        .env("PATH", sandbox.root.join("bin"))
        .env("BGRUN_TEST_LOG", &sandbox.log)
        .env_remove("BGRUN_PREFIX")
        .env("XDG_CONFIG_HOME", &sandbox.root)
        .env("USER", "tester")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("failed to run bgrun");

    // Wait for the follower to be up, so the interrupt cannot land before it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let follower = loop {
        if let Some(text) = std::fs::read_to_string(&pidfile).ok()
            && !text.trim().is_empty()
        {
            break text.trim().to_owned();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "follower never started; calls: {:?}",
            fs::read_to_string(&sandbox.log).unwrap_or_default()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let follower: i32 = follower.parse().expect("follower pid");

    Command::new("kill")
        .args(["-INT", "--", &format!("-{}", child.id())])
        .status()
        .expect("kill failed");

    let status = child.wait().expect("bgrun did not exit");
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("no stdout")
        .read_to_string(&mut stdout)
        .expect("no stdout");

    // bgrun must be gone, and the follower with it.
    assert!(
        !status.success(),
        "an interrupt is not a result: stdout: {stdout}"
    );
    assert!(
        !stdout.contains("bgrun-web.service:"),
        "an interrupted job must not be reported as ended: {stdout}"
    );

    // The follower is a child of the test, so it is reparented to init and
    // reaped there; allow for that rather than racing it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let alive = PathBuf::from(format!("/proc/{follower}"));
    while alive.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !alive.exists(),
        "journalctl follower {follower} outlived the interrupt"
    );
}

#[test]
fn a_job_that_runs_longer_than_a_few_polls_is_still_waited_for() {
    let sandbox = Sandbox::new();
    // Running for six ticks: the wait must not be bounded by how long the job
    // takes, only by consecutive query failures.
    let polls = sandbox.root.join("polls");
    sandbox.shim(
        "systemctl",
        &format!(
            "printf '%s\\n' \"$0 $*\" >> \"$BGRUN_TEST_LOG\"\n\
             case \"$*\" in\n\
             *LoadState*)\n\
               n=0\n\
               if [ -s {polls} ]; then read n < {polls}; fi\n\
               echo $((n + 1)) > {polls}\n\
               if [ \"$n\" -ge 5 ]; then echo not-found; echo inactive\n\
               else echo loaded; echo active; fi\n\
               exit 0;;\n\
             *Result*) echo success; echo 0; exit 0;;\n\
             esac\n",
            polls = polls.display()
        ),
    );
    sandbox.journal_exit("exit-code", "4");

    let output = sandbox.run(&["watch", "slow"]);

    assert_eq!(
        code(&output),
        4,
        "a slow job is waited out: {}",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("exit code 4"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn watch_takes_exactly_one_name() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["watch", "a", "b"]);

    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("unexpected extra arguments"));
    assert!(
        sandbox.calls().is_empty(),
        "nothing may run: {:?}",
        sandbox.calls()
    );
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
    for expected in [
        "add",
        "list",
        "status",
        "logs",
        "watch",
        "stop",
        "resume",
        "remove",
        "clean",
        "--restart",
        "--persist",
    ] {
        assert!(out.contains(expected), "help omits {expected}:\n{out}");
    }
    assert!(sandbox.calls().is_empty());
}

#[test]
fn version_prints_the_manifest_version_and_shells_out_to_nothing() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--version"]);

    assert_eq!(code(&output), 0);
    assert_eq!(
        stdout(&output).trim(),
        format!("bgrun {}", env!("CARGO_PKG_VERSION"))
    );
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
