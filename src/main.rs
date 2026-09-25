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
        Action::Run(spec) => run(&prefix, spec),
        Action::List => list(&prefix),
        Action::Status(name) => status(&prefix, &name),
        Action::Logs {
            name,
            journalctl_opts,
        } => logs(&prefix, &name, &journalctl_opts),
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
  bgrun [--flags] [overrides] -- <cmd>      run in bg; name auto-derived
  bgrun add [NAME] [flags] [overrides] -- <cmd>   same, with a name
  bgrun list
  bgrun status <name>
  bgrun logs <name> [journalctl opts]
  bgrun stop <name> [name...]                stop + forget units
  bgrun remove <name> [name...]              alias of stop
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
  --restart    restart the command when it exits non-zero
               (systemd's own limit still applies: 5 starts per 10s)
  --persist    write a unit file and enable it, so the job also runs at
               every boot. Only -p KEY=VALUE overrides can be persisted,
               and the name must not be taken by a running transient job.

Notes:
  - Units are transient (--collect): finished jobs disappear on their own;
    'clean' only clears units that exited non-zero.
  - '--persist' is the exception: it writes a real unit file under the
    systemd user unit directory, and 'bgrun remove' is what deletes it.
    Without that the job returns at every boot.
  - Jobs survive logout only if lingering is enabled:
      loginctl enable-linger $USER
    'bgrun add' warns when it is off.
  - Logs stay in the journal: view with 'bgrun logs <name>'.
  - Prefix is '{prefix}' (override with BGRUN_PREFIX; letters, digits,
    '-' and '_' only).
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
