//! bgrun — run commands in the background as transient systemd user units.
//!
//! The effect layer: every [`Action`] from the pure parser becomes exactly
//! one subprocess call shape here. End-to-end tests (`tests/end_to_end.rs`)
//! run this binary against PATH shims for systemctl/systemd-run/journalctl.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::io::{self};
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

use bgrun::Action;
use bgrun::ExitCodes;
use bgrun::JobName;
use bgrun::ParseError;
use bgrun::Prefix;
use bgrun::RunSpec;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();

    let prefix = match Prefix::from_env() {
        Ok(prefix) => prefix,
        Err(error) => return fail(error),
    };

    let action = match bgrun::parse(&args) {
        Ok(action) => action,
        Err(error) => return fail(error),
    };

    match action {
        Action::Help => {
            print!("{}", help(&prefix));
            ExitCode::SUCCESS
        }
        Action::Version => {
            println!("bgrun {}", bgrun::VERSION);
            ExitCode::SUCCESS
        }
        Action::Run(spec) => run(&prefix, spec),
        Action::List => list(&prefix),
        Action::Status(name) => status(&prefix, &name),
        Action::Logs {
            name,
            journalctl_opts,
        } => logs(&prefix, &name, &journalctl_opts),
        Action::Watch {
            name,
            journalctl_opts,
        } => watch(&prefix, &name, &journalctl_opts),
        Action::Stop(names) => stop(&prefix, &names),
        Action::Resume(names) => resume(&prefix, &names),
        Action::Remove(names) => remove(&prefix, &names),
        Action::Clean => clean(&prefix),
    }
}

fn fail(error: ParseError) -> ExitCode {
    eprintln!("error: {error}");
    ExitCodes::usage()
}

// ------------------------------------------------------------- commands --

fn run(prefix: &Prefix, spec: RunSpec) -> ExitCode {
    warn_if_not_lingering();

    let name = spec
        .name
        .clone()
        .unwrap_or_else(|| JobName::from_command(&spec.command));
    if spec.persist {
        return persist(prefix, &name, &spec);
    }
    let unit = prefix.unit(&name);

    // No existence pre-check: systemd-run itself rejects a duplicate unit
    // name, so the check and the creation cannot race.
    let output = Command::new("systemd-run")
        .arg("--user")
        .arg(format!("--unit={unit}"))
        .arg("--collect")
        .args(&spec.systemd_opts)
        .args(&spec.command)
        .output();
    match output {
        Ok(output) => {
            // systemd-run's "Running as unit: …" confirmation belongs to the
            // user; we captured it, so hand it back.
            io::stdout().write_all(&output.stdout).ok();
            if output.status.success() {
                return ExitCode::SUCCESS;
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("already exists") {
                eprintln!("error: {unit} already exists (running or failed)");
                eprintln!("  see: bgrun logs {name}   remove: bgrun remove {name}");
                return ExitCodes::failure();
            }
            eprint!("{stderr}");
            ExitCodes::of(&output)
        }
        Err(error) => spawn_failed("systemd-run", &error),
    }
}

/// Back the job with a real unit file so it starts on every boot. Nothing
/// transient can do this — see [`bgrun::unit_file`].
fn persist(prefix: &Prefix, name: &JobName, spec: &RunSpec) -> ExitCode {
    let body = match bgrun::unit_file(name, spec) {
        Ok(body) => body,
        Err(error) => return fail(error),
    };
    let Some(dir) = user_unit_dir() else {
        return fail(ParseError::NoUnitDirectory);
    };
    let unit = prefix.unit(name);
    // A loaded transient unit of the same name wins name resolution over the
    // file we are about to write, and `systemctl enable` calls it
    // "transient or generated". Refuse before writing anything.
    if transient_shadow(&unit) {
        eprintln!("error: {unit} is already running as a transient job");
        eprintln!("  stop it first: bgrun remove {name}");
        return ExitCodes::failure();
    }
    let path = dir.join(&unit);
    if let Err(error) = fs::create_dir_all(&dir).and_then(|()| fs::write(&path, body)) {
        eprintln!("error: cannot write {}: {error}", path.display());
        return ExitCodes::failure();
    }

    let (reloaded, ok) = systemctl_user(&["daemon-reload"]);
    if !ok {
        let _ = fs::remove_file(&path);
        return reloaded;
    }
    // `--now` starts the job immediately, so persisting behaves like a
    // plain launch plus a boot hook.
    let (enabled, ok) = systemctl_user(&["enable", "--now", &unit]);
    if !ok {
        // A unit file systemd refuses would otherwise be retried at every
        // boot, forever, with nobody around to read the failure.
        forget(prefix, name);
        return enabled;
    }
    println!("persisted: {unit} (starts on every boot)");
    enabled
}

fn list(prefix: &Prefix) -> ExitCode {
    let glob = prefix.glob();
    let (code, _) = systemctl_user(&["list-units", &glob, "--all", "--no-pager"]);
    code
}

fn status(prefix: &Prefix, name: &JobName) -> ExitCode {
    let unit = prefix.unit(name);
    let (code, _) = systemctl_user(&["status", &unit]);
    code
}

fn logs(prefix: &Prefix, name: &JobName, journalctl_opts: &[OsString]) -> ExitCode {
    forward(
        Command::new("journalctl")
            .arg("--user")
            .arg("-u")
            .arg(prefix.unit(name))
            .args(journalctl_opts),
    )
}

/// Follow a job's journal until the unit stops running, report how it ended,
/// and exit with its status.
///
/// The outcome comes from systemd's own record of the exit, never from the
/// job's output. A `--collect` unit is unloaded the moment it goes inactive —
/// measured here at under 50 ms, never caught in a terminal state by a 7.8 ms
/// poll — so a unit that is gone by the time we look has to be read from the
/// exit record systemd left in the journal instead.
fn watch(prefix: &Prefix, name: &JobName, journalctl_opts: &[OsString]) -> ExitCode {
    let unit = prefix.unit(name);

    let Some(state) = query_unit_state(&unit) else {
        eprintln!("error: cannot query {unit}: systemctl is not answering");
        return ExitCodes::failure();
    };
    if state.load == "not-found" {
        return not_a_job(&unit, name);
    }
    if !still_running(&state.active) {
        eprintln!("error: {unit} is not running ({})", state.active);
        eprintln!("  bgrun resume {name}   start a stopped job again");
        return ExitCodes::failure();
    }

    // The user's options go to the follower and nowhere else. The queries that
    // decide the outcome build their own argv, so a `-u other-unit` or
    // `--since` here cannot make `watch` report some other job's result.
    let Some(follower) = Follower::start(&unit, journalctl_opts) else {
        return spawn_failed("journalctl", &std::io::Error::other("cannot follow"));
    };

    // ponytail: `systemctl wait` is not in systemd 261, so this is a poll.
    // Costs one fork per tick and up to one tick of latency after the job
    // stops; a `wait` verb would replace the whole loop.
    let ended = match wait_until_ended(&unit) {
        Ok(ended) => ended,
        Err(code) => {
            drop(follower);
            return code;
        }
    };
    drop(follower);

    let outcome = outcome(&unit, &ended);
    println!("{unit}: {}", outcome.detail);
    outcome.code
}

/// Poll until the unit leaves the running states, returning the last state
/// seen — including the one that ended the wait, so the caller does not have
/// to ask again.
///
/// A `systemctl` that fails is not an answer about the job. Treating it as one
/// would report a result for a job that is still running, so a failure only
/// ends the wait after a run of consecutive failures; a job that simply takes
/// a long time is waited out for as long as it takes.
fn wait_until_ended(unit: &str) -> Result<UnitState, ExitCode> {
    const PATIENCE: usize = 5;
    let mut failures = 0;
    loop {
        match query_unit_state(unit) {
            Some(state) => {
                failures = 0;
                if !still_running(&state.active) {
                    return Ok(state);
                }
            }
            None => {
                failures += 1;
                if failures >= PATIENCE {
                    eprintln!("error: lost contact with systemd while waiting for {unit}");
                    return Err(ExitCodes::failure());
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// A unit the manager no longer knows, or one it collected. A transient job
/// that already finished is the second case, and its journal is still there.
fn not_a_job(unit: &str, name: &JobName) -> ExitCode {
    match journal_has_records(unit) {
        Some(true) => {
            eprintln!("error: {unit} has already finished and was collected");
            eprintln!("  its journal is still readable: bgrun logs {name}");
        }
        Some(false) => {
            eprintln!("error: no such job: {unit}");
            eprintln!("  bgrun list              what bgrun is running");
        }
        // A journal that cannot answer has proved nothing either way, so
        // claiming the name was never used would be a guess.
        None => {
            eprintln!("error: cannot read the journal, so cannot tell whether {unit} ran");
            eprintln!("  bgrun list              what bgrun is running");
        }
    }
    ExitCodes::failure()
}

/// The `journalctl --user -u <unit> -f` child, killed and reaped on drop so
/// no follower outlives the command. It stays in bgrun's own process group on
/// purpose: a terminal Ctrl-C signals the whole foreground group, so the
/// interrupt takes the follower down with us instead of orphaning a `-f` that
/// would stream forever.
///
/// ponytail: a signal aimed at bgrun alone (`kill -INT <pid>`, an agent
/// harness) still leaves the follower, because catching it needs a handler
/// and this crate forbids both `unsafe` and new dependencies. Take the
/// follower down by signalling the group, or add `signal-hook` if a
/// single-target interrupt has to be survivable.
struct Follower(std::process::Child);

impl Follower {
    /// `journalctl --user -u <unit> [opts…] --follow`: the caller's options
    /// come first so a catch-up like `-n 200` prints before the tail, and
    /// `--follow` last so the argv reads in the order it is used.
    fn start(unit: &str, journalctl_opts: &[OsString]) -> Option<Self> {
        Command::new("journalctl")
            .arg("--user")
            .arg("-u")
            .arg(unit)
            .args(journalctl_opts)
            .arg("--follow")
            .spawn()
            .ok()
            .map(Self)
    }

    fn stop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The unit states in which a job is still going. `deactivating` counts: the
/// unit is on its way out, and its result is not final until it lands.
fn still_running(active: &str) -> bool {
    matches!(
        active,
        "active" | "activating" | "reloading" | "deactivating"
    )
}

struct UnitState {
    load: String,
    active: String,
}

/// How a job ended, and the exit code `bgrun watch` reports for it.
struct Outcome {
    detail: String,
    code: ExitCode,
}

/// `systemctl --user show <unit> -p LoadState -p ActiveState --value`.
///
/// A unit the manager has never heard of still answers, as `not-found`, so
/// this is only `None` when systemctl itself could not be run or refused.
fn query_unit_state(unit: &str) -> Option<UnitState> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            unit,
            "-p",
            "LoadState",
            "-p",
            "ActiveState",
            "--value",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    Some(UnitState {
        load: lines.next().unwrap_or_default().to_owned(),
        active: lines.next().unwrap_or_default().to_owned(),
    })
}

/// systemd's record of the exit: off the unit while it is still loaded, and
/// off the journal's exit fields once a collected unit is gone. A clean exit
/// has no exit record at all, which is what `success` looks like in both.
fn outcome(unit: &str, ended: &UnitState) -> Outcome {
    if ended.load != "not-found"
        && let Some(result) = unit_result(unit)
    {
        return result;
    }
    journal_result(unit)
}

fn unit_result(unit: &str) -> Option<Outcome> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            unit,
            "-p",
            "Result",
            "-p",
            "ExecMainStatus",
            "--value",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let result = lines.next()?.to_owned();
    let status = lines.next().and_then(|line| line.trim().parse().ok());
    Some(classify(&result, status))
}

/// systemd's `MESSAGE_ID` for "Started <unit>." — the record that opens a
/// run. Everything after it belongs to an older run of the same name, and a
/// name that has run before is the normal case, not an edge one.
///
/// These are systemd's own catalogue IDs, stable across versions, which is
/// what makes it safe to hardcode rather than match on message text. If a
/// systemd ever stopped emitting this one, the scan would fall through to the
/// whole window and could report an older run's failure for a job that has
/// since succeeded — wrong, but in the loud direction rather than a silent
/// success.
const STARTED_RECORD: &str = "MESSAGE_ID=39f53479d3a045ac8e11786248231fbf";

/// The exit fields systemd logged for the run that just ended.
///
/// A unit's journal holds every run of that name, so this reads newest-first
/// and stops at the record that opened the run we waited for: an earlier
/// failure must not be reported for a job that has since succeeded. A clean
/// exit writes no exit fields at all, so finding none within the run is what
/// success looks like.
///
/// The two fields live on different records — `EXIT_STATUS` on "Main process
/// exited, code=…, status=…/…", `UNIT_RESULT` on "Failed with result '…'" —
/// so they are gathered independently and a short bound would drop one.
fn journal_result(unit: &str) -> Outcome {
    let unreadable = Outcome {
        detail: "could not read the job's exit from the journal".to_owned(),
        code: ExitCodes::failure(),
    };
    let Ok(output) = Command::new("journalctl")
        .args([
            "--user",
            "-u",
            unit,
            "SYSLOG_IDENTIFIER=systemd",
            "-r",
            "-n",
            "20",
            "-o",
            "export",
            "--no-pager",
        ])
        .output()
    else {
        return unreadable;
    };
    if !output.status.success() {
        return unreadable;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (result, status) = exit_fields(&text);
    classify(&result, status)
}

/// The `UNIT_RESULT` and `EXIT_STATUS` of the newest run, as `(String,
/// Option<i32>)`. Parsing stays defensive: the filter is a cost measure, not a
/// trust boundary, so a job can put records of its own under that identifier.
fn exit_fields(journal: &str) -> (String, Option<i32>) {
    let mut result = String::new();
    let mut status = None;
    for line in journal.lines() {
        if line == STARTED_RECORD {
            break;
        }
        if result.is_empty()
            && let Some(value) = line.strip_prefix("UNIT_RESULT=")
        {
            result = value.to_owned();
        }
        if status.is_none()
            && let Some(value) = line.strip_prefix("EXIT_STATUS=")
        {
            status = value.trim().parse().ok();
        }
    }
    (result, status)
}

/// The exit code a job's systemd result maps to: its own status for a plain
/// exit, 128 + signal when a signal ended it, and a failure for any other
/// result (timeout, watchdog, resources) that carries no status of its own.
fn classify(result: &str, status: Option<i32>) -> Outcome {
    let coded = |code: u8, detail: String| Outcome {
        detail,
        code: ExitCode::from(code),
    };
    match (result, status) {
        ("exit-code", Some(status)) => coded(
            u8::try_from(status).unwrap_or(bgrun::EXIT_FAILURE),
            format!("exit code {status}"),
        ),
        ("signal" | "core-dump", Some(signal)) => {
            // A signal number a shell could not have raised, from a journal
            // field, so it is bounded rather than trusted into 128+.
            let code = (1..=64).contains(&signal).then(|| 128 + signal);
            match code.and_then(|sum| u8::try_from(sum).ok()) {
                Some(code) => coded(code, format!("killed by signal {signal}")),
                None => Outcome {
                    detail: format!("ended with result '{result}' and status {signal}"),
                    code: ExitCodes::failure(),
                },
            }
        }
        ("", _) | ("success", _) => Outcome {
            detail: "finished successfully".to_owned(),
            code: ExitCode::SUCCESS,
        },
        other => Outcome {
            detail: format!("ended with result '{}'", other.0),
            code: ExitCodes::failure(),
        },
    }
}

/// Whether anything at all was ever logged for a unit, which is what tells a
/// collected job apart from a name that was never used. `None` means
/// journalctl could not answer, which is not evidence that nothing ran.
fn journal_has_records(unit: &str) -> Option<bool> {
    let output = Command::new("journalctl")
        .args(["--user", "-u", unit, "-n", "1", "--no-pager", "-q"])
        .output()
        .ok()?;
    output.status.success().then_some(!output.stdout.is_empty())
}

/// Stop units for now, leaving a persisted job's unit file alone so
/// `resume` — or the next boot — brings it back. A transient job is
/// collected the moment it goes inactive, so for one of those a stop is
/// final and only `remove` is left to say.
fn stop(prefix: &Prefix, names: &[JobName]) -> ExitCode {
    for_each_unit(prefix, names, "stop", "stopped")
}

/// `systemctl start` for whatever `stop` left behind.
fn resume(prefix: &Prefix, names: &[JobName]) -> ExitCode {
    for_each_unit(prefix, names, "start", "resumed")
}

/// `systemctl --user <verb> <unit>` per name, one line of output per name
/// that worked. A unit systemd no longer knows gets the re-add instruction,
/// since only a persisted job survives a stop.
fn for_each_unit(prefix: &Prefix, names: &[JobName], verb: &str, done: &str) -> ExitCode {
    let mut all_ok = true;
    for name in names {
        let unit = prefix.unit(name);
        let (_, ok) = systemctl_user(&[verb, &unit]);
        if ok {
            println!("{done}: {unit}");
        } else if unit_missing(&unit) {
            eprintln!("error: {unit} is not a unit systemd knows");
            eprintln!("  only a --persist job survives a stop; a transient one is");
            eprintln!("  collected as soon as it stops or finishes.");
            eprintln!("  re-add it:  bgrun add {name} -- <cmd>");
        }
        all_ok &= ok;
    }
    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCodes::failure()
    }
}

fn remove(prefix: &Prefix, names: &[JobName]) -> ExitCode {
    for name in names {
        let unit = prefix.unit(name);
        // Stopping a dead unit and resetting an unfailed unit are both
        // no-ops; their exit codes carry no signal here.
        for verb in ["stop", "reset-failed"] {
            let _ = Command::new("systemctl")
                .arg("--user")
                .arg(verb)
                .arg(&unit)
                .output();
        }
        forget(prefix, name);
        println!("removed: {unit}");
    }
    ExitCode::SUCCESS
}

fn clean(prefix: &Prefix) -> ExitCode {
    // Units run with --collect, so successful jobs are already gone; this
    // only clears units that exited non-zero.
    let glob = prefix.glob();
    match Command::new("systemctl")
        .arg("--user")
        .arg("reset-failed")
        .arg(&glob)
        .output()
    {
        Ok(output) if output.status.success() => {
            println!("forgot failed {prefix} units (successful jobs are collected automatically)",);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            ExitCodes::of(&output)
        }
        Err(error) => spawn_failed("systemctl", &error),
    }
}

// ---------------------------------------------------------------- misc --

/// `systemctl --user <args…>` with inherited stdio: its exit code, and
/// whether it succeeded — `ExitCode` itself cannot be compared.
fn systemctl_user(args: &[&str]) -> (ExitCode, bool) {
    match Command::new("systemctl").arg("--user").args(args).status() {
        Ok(status) => (ExitCodes::of_status(&status), status.success()),
        Err(error) => (spawn_failed("systemctl", &error), false),
    }
}

/// Where `systemd --user` looks for unit files: `$XDG_CONFIG_HOME/systemd/user`,
/// falling back to `$HOME/.config/systemd/user`.
fn user_unit_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("systemd").join("user"))
}

/// Whether systemd still has a unit under this name at all. Distinct from
/// [`transient_shadow`], which asks about a unit that *is* loaded.
fn unit_missing(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "show", unit, "--property=LoadState", "--value"])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "not-found")
}

/// Whether a transient unit of this name is currently loaded. Persisting
/// over one is impossible: the manager keeps the name, `systemctl enable`
/// refuses it, and the unit file would only take effect after the transient
/// unit is gone.
fn transient_shadow(unit: &str) -> bool {
    Command::new("systemctl")
        .args([
            "--user",
            "show",
            unit,
            "--property=UnitFileState",
            "--value",
        ])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "transient")
}

/// Disable and delete a persisted job's unit file, so it does not come back
/// at the next boot. A transient job has no file on disk, so this does
/// nothing at all for it and `remove` stays as quiet as it was.
fn forget(prefix: &Prefix, name: &JobName) {
    let Some(dir) = user_unit_dir() else {
        return;
    };
    let unit = prefix.unit(name);
    let path = dir.join(&unit);
    if !path.exists() {
        return;
    }
    let _ = systemctl_user(&["disable", &unit]);
    match fs::remove_file(&path) {
        Ok(()) => {
            let _ = systemctl_user(&["daemon-reload"]);
        }
        Err(error) => eprintln!("warning: cannot delete {}: {error}", path.display()),
    }
}

/// Best-effort warning: transient user units die with the user's last
/// session unless lingering is enabled. Never blocks starting a job.
fn warn_if_not_lingering() {
    let Ok(user) = std::env::var("USER") else {
        return;
    };
    let Ok(output) = Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger", "--value"])
        .output()
    else {
        return;
    };
    if String::from_utf8_lossy(&output.stdout).trim() == "no" {
        eprintln!(
            "warning: lingering is disabled for '{user}' — jobs die when your last session logs out"
        );
        eprintln!("  enable it once: loginctl enable-linger {user}");
    }
}

fn help(prefix: &Prefix) -> String {
    format!(
        "bgrun — run commands in the background as systemd user units

Usage:
  bgrun help | -h | --help
  bgrun -V | --version
  bgrun [--flags] [overrides] -- <cmd>      run in bg; name auto-derived
  bgrun add [NAME] [flags] [overrides] -- <cmd>   same, with a name
  bgrun list
  bgrun status <name>
  bgrun logs <name> [journalctl opts]
  bgrun watch <name> [journalctl opts]      stream the job, then exit with its status
  bgrun stop <name> [name...]                stop for now (--persist only resumes)
  bgrun resume <name> [name...]              start a stopped --persist job again
  bgrun remove <name> [name...]              stop + forget, for good
  bgrun clean                                forget failed {prefix}-* units

The command must come after '--'. The job name defaults to the command's
basename. Everything between [NAME] and '--' is passed straight to
systemd-run, so you can override unit properties like WorkingDirectory:

  bgrun -- discord-overlay
  bgrun -- sleep 100
  bgrun add build -p WorkingDirectory=$HOME/project -- make
  bgrun add dl --working-directory=/tmp -pMemoryMax=1G -- wget URL
  bgrun add backup -- rsync -a ~/src/ /mnt/backup/

Flags (either form, in front of the overrides):
  -r, --restart    restart the command when it exits non-zero
                   (systemd's own limit still applies: 5 starts per 10s)
  -b, --persist    write a unit file and enable it, so the job also runs at
                   every boot. Only -p KEY=VALUE overrides can be persisted.
"
    )
}

/// Run a subprocess with inherited stdio and propagate its exit code.
fn forward(command: &mut Command) -> ExitCode {
    match command.status() {
        Ok(status) => ExitCodes::of_status(&status),
        Err(error) => spawn_failed(&command.get_program().to_string_lossy(), &error),
    }
}

fn spawn_failed(program: &str, error: &std::io::Error) -> ExitCode {
    eprintln!("error: cannot run {program}: {error}");
    ExitCodes::failure()
}
