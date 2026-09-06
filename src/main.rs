//! bgrun — run commands in the background as transient systemd user units.
//!
//! The effect layer: every [`Action`] from the pure parser becomes exactly
//! one subprocess call shape here. End-to-end tests (`tests/end_to_end.rs`)
//! run this binary against PATH shims for systemctl/systemd-run/journalctl.

use std::ffi::OsString;
use std::io::Write;
use std::io::{self};
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
        .unwrap_or_else(|| JobName::from_command(&spec.command));
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

fn list(prefix: &Prefix) -> ExitCode {
    forward(
        Command::new("systemctl")
            .arg("--user")
            .arg("list-units")
            .arg(prefix.glob())
            .arg("--all")
            .arg("--no-pager"),
    )
}

fn status(prefix: &Prefix, name: &JobName) -> ExitCode {
    forward(
        Command::new("systemctl")
            .arg("--user")
            .arg("status")
            .arg(prefix.unit(name)),
    )
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
        println!("removed: {unit}");
    }
    ExitCode::SUCCESS
}

fn clean(prefix: &Prefix) -> ExitCode {
    // Units run with --collect, so successful jobs are already gone; this
    // only clears units that exited non-zero.
    match Command::new("systemctl")
        .arg("--user")
        .arg("reset-failed")
        .arg(prefix.glob())
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
        "bgrun — run commands in the background as transient systemd user units

Usage:
  bgrun -- <command> [args...]               run in bg; name auto-derived
  bgrun add [NAME] [overrides] -- <cmd>      run in bg, optional name
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

Notes:
  - Units are transient (--collect): finished jobs disappear on their own;
    'clean' only clears units that exited non-zero.
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
