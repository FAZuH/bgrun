//! bgrun core: argument parsing and unit naming.
//!
//! Pure logic only — no process spawning, no I/O. The binary in `main.rs`
//! turns parsed [`Action`]s into subprocess calls; end-to-end tests cover
//! that layer against PATH shims. Everything here is unit-tested inline.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt;
use std::path::Path;
use std::process::ExitCode;
use std::process::ExitStatus;
use std::process::Output;

/// Exit code for usage errors (bad arguments, unknown command).
pub const EXIT_USAGE: u8 = 2;
/// Exit code for runtime failures.
pub const EXIT_FAILURE: u8 = 1;

/// Named exit codes so `main` never hand-rolls `ExitCode::from`.
pub struct ExitCodes;

impl ExitCodes {
    pub fn failure() -> ExitCode {
        ExitCode::from(EXIT_FAILURE)
    }

    pub fn usage() -> ExitCode {
        ExitCode::from(EXIT_USAGE)
    }

    /// Propagate a subprocess exit code; signal death maps to failure.
    pub fn of_status(status: &ExitStatus) -> ExitCode {
        status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or_else(Self::failure, ExitCode::from)
    }

    pub fn of(output: &Output) -> ExitCode {
        Self::of_status(&output.status)
    }
}

// ---------------------------------------------------------------- naming --

/// Validated unit-name prefix (`bgrun` by default, `$BGRUN_PREFIX` override).
///
/// Restricted to `[A-Za-z0-9_-]+` so that `{prefix}-*.service` stays a plain
/// glob for `systemctl list-units` / `reset-failed`, and so unit names built
/// from it are always valid systemd unit names. Parse, don't validate: an
/// invalid prefix cannot be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefix(String);

impl Prefix {
    /// Parse a raw prefix value.
    pub fn parse(raw: &str) -> Result<Self, ParseError> {
        let ok = !raw.is_empty()
            && raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if ok {
            Ok(Self(raw.to_owned()))
        } else {
            Err(ParseError::InvalidPrefix {
                value: raw.to_owned(),
            })
        }
    }

    /// Read from the environment, falling back to `bgrun`.
    pub fn from_env() -> Result<Self, ParseError> {
        match std::env::var("BGRUN_PREFIX") {
            Ok(raw) => Self::parse(&raw),
            Err(_) => Self::parse("bgrun"),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Full systemd unit name for a job.
    pub fn unit(&self, job: &JobName) -> String {
        format!("{}-{job}.service", self.0)
    }

    /// Glob pattern matching every unit this prefix owns.
    pub fn glob(&self) -> String {
        format!("{}-*.service", self.0)
    }
}

impl fmt::Display for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Sanitized job name: every character outside `[A-Za-z0-9:_.-]` becomes `-`,
/// an empty result becomes `job`. Total constructor — callers never have to
/// re-validate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobName(String);

impl JobName {
    pub fn parse(raw: &OsStr) -> Self {
        let cleaned: String = raw
            .to_string_lossy()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        Self(if cleaned.is_empty() {
            "job".to_owned()
        } else {
            cleaned
        })
    }

    /// Derive the job name from the command's first token
    /// (`/usr/bin/make` → `make`), falling back to `job`.
    pub fn from_command(command: &[OsString]) -> Self {
        command
            .first()
            .and_then(|argv0| Path::new(argv0).file_name())
            .map_or_else(|| Self("job".to_owned()), Self::parse)
    }
}

impl fmt::Display for JobName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// -------------------------------------------------------------- parsing --

/// A job to launch: optional explicit name, verbatim systemd-run property
/// overrides, and the command itself.
#[derive(Debug, PartialEq, Eq)]
pub struct RunSpec {
    pub name: Option<JobName>,
    pub systemd_opts: Vec<OsString>,
    pub command: Vec<OsString>,
}

/// Parsed command line — the closed vocabulary of everything bgrun can do.
/// Adding a subcommand means adding a variant here; the dispatch in `main`
/// then fails to compile until it handles it.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Help,
    Run(RunSpec),
    List,
    Status(JobName),
    Logs {
        name: JobName,
        journalctl_opts: Vec<OsString>,
    },
    /// Stop and forget units. `stop` and `remove` are aliases (both stop,
    /// reset the failure record, and confirm).
    Remove(Vec<JobName>),
    Clean,
}

/// Everything that can go wrong before we shell out.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    UnknownCommand { command: String },
    MissingSeparator,
    MissingCommand,
    MissingArgument { action: String },
    UnexpectedArgument { action: String },
    InvalidPrefix { value: String },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCommand { command } => write!(
                f,
                "unknown command '{command}' (run 'bgrun help' for usage)"
            ),
            Self::MissingSeparator => write!(
                f,
                "missing '--' before the command (usage: bgrun add [NAME] [opts] -- <cmd> [args])"
            ),
            Self::MissingCommand => write!(f, "missing command after '--'"),
            Self::MissingArgument { action } => write!(f, "{action}: missing job name"),
            Self::UnexpectedArgument { action } => {
                write!(f, "{action}: unexpected extra arguments")
            }
            Self::InvalidPrefix { value } => write!(
                f,
                "invalid BGRUN_PREFIX {value:?}: use only ASCII letters, digits, '-' and '_'"
            ),
        }
    }
}

/// Parse `argv` (without the program name) into an [`Action`].
pub fn parse(args: &[OsString]) -> Result<Action, ParseError> {
    let Some(first) = args.first() else {
        return Ok(Action::Help);
    };
    let first = first.to_string_lossy();
    let rest = &args[1..];

    match &*first {
        "help" | "-h" | "--help" => Ok(Action::Help),
        "--" => run_spec(None, &[], rest),
        "add" => parse_add(rest),
        "list" => expect_no_args("list", rest).map(|()| Action::List),
        "clean" => expect_no_args("clean", rest).map(|()| Action::Clean),
        "status" => match rest {
            [name] => Ok(Action::Status(JobName::parse(name))),
            [] => Err(ParseError::MissingArgument {
                action: first.to_string(),
            }),
            [..] => Err(ParseError::UnexpectedArgument {
                action: first.to_string(),
            }),
        },
        "logs" => {
            let Some(name) = rest.first() else {
                return Err(ParseError::MissingArgument {
                    action: first.to_string(),
                });
            };
            Ok(Action::Logs {
                name: JobName::parse(name),
                journalctl_opts: rest[1..].to_vec(),
            })
        }
        "stop" | "remove" => {
            if rest.is_empty() {
                return Err(ParseError::MissingArgument {
                    action: first.to_string(),
                });
            }
            Ok(Action::Remove(
                rest.iter().map(|n| JobName::parse(n)).collect(),
            ))
        }
        other => Err(ParseError::UnknownCommand {
            command: other.to_owned(),
        }),
    }
}

/// `add [NAME] [systemd-run overrides…] -- <command…>`
fn parse_add(args: &[OsString]) -> Result<Action, ParseError> {
    let mut name = None;
    let mut start = 0;

    // Optional NAME: the first token, unless it looks like an option or is
    // the separator itself.
    if let Some(first) = args
        .first()
        .filter(|first| first.as_os_str() != "--" && !first.to_string_lossy().starts_with('-'))
    {
        name = Some(JobName::parse(first));
        start = 1;
    }

    // Overrides run verbatim up to the mandatory `--`.
    let Some(offset) = args[start..].iter().position(|a| a.as_os_str() == "--") else {
        return Err(ParseError::MissingSeparator);
    };
    let sep = start + offset;
    let command = &args[sep + 1..];
    run_spec(name, &args[start..sep], command)
}

/// Shared tail for the two launch forms (`-- cmd…` and `add … -- cmd…`).
fn run_spec(
    name: Option<JobName>,
    systemd_opts: &[OsString],
    command: &[OsString],
) -> Result<Action, ParseError> {
    if command.is_empty() {
        return Err(ParseError::MissingCommand);
    }
    Ok(Action::Run(RunSpec {
        name,
        systemd_opts: systemd_opts.to_vec(),
        command: command.to_vec(),
    }))
}

fn expect_no_args(action: &str, rest: &[OsString]) -> Result<(), ParseError> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(ParseError::UnexpectedArgument {
            action: action.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn s(name: &str) -> JobName {
        JobName::parse(OsStr::new(name))
    }

    // -- JobName ------------------------------------------------------------

    #[test]
    fn job_name_sanitizes_disallowed_characters() {
        assert_eq!(s("my job!"), s("my-job-"));
        assert_eq!(s("a/b\tc"), s("a-b-c"));
        assert_eq!(s("héllo"), s("h-llo"));
        assert_eq!(s("web:worker_2.config"), s("web:worker_2.config"));
    }

    #[test]
    fn job_name_empty_falls_back_to_job() {
        assert_eq!(s(""), s("job"));
        assert_eq!(s(""), s("job"));
    }

    #[test]
    fn job_name_from_command_uses_basename() {
        assert_eq!(
            JobName::from_command(&os(&["/usr/bin/make", "-j4"])),
            s("make")
        );
        assert_eq!(JobName::from_command(&os(&["./run.sh"])), s("run.sh"));
        assert_eq!(JobName::from_command(&os(&["/"])), s("job"));
        assert_eq!(JobName::from_command(&os(&[])), s("job"));
    }

    // -- Prefix -------------------------------------------------------------

    #[test]
    fn prefix_accepts_plain_names() {
        assert!(Prefix::parse("bgrun").is_ok());
        assert!(Prefix::parse("my_jobs-2").is_ok());
    }

    #[test]
    fn prefix_rejects_empty_and_metacharacters() {
        assert!(Prefix::parse("").is_err());
        assert!(Prefix::parse("bg[run]").is_err());
        assert!(Prefix::parse("bg*").is_err());
        assert!(Prefix::parse("bg run").is_err());
    }

    #[test]
    fn prefix_builds_unit_and_glob() {
        let p = Prefix::parse("jobs").unwrap();
        assert_eq!(p.unit(&s("dl")), "jobs-dl.service");
        assert_eq!(p.glob(), "jobs-*.service");
    }

    // -- parse --------------------------------------------------------------

    #[test]
    fn no_arguments_is_help() {
        assert_eq!(parse(&[]), Ok(Action::Help));
    }

    #[test]
    fn help_flags() {
        for flag in ["help", "-h", "--help"] {
            assert_eq!(parse(&os(&[flag])), Ok(Action::Help), "flag: {flag}");
        }
    }

    #[test]
    fn bare_run_derives_name() {
        let action = parse(&os(&["--", "sleep", "5"])).unwrap();
        let Action::Run(spec) = action else {
            panic!("expected Run, got {action:?}");
        };
        assert_eq!(spec.name, None);
        assert!(spec.systemd_opts.is_empty());
        assert_eq!(spec.command, os(&["sleep", "5"]));
    }

    #[test]
    fn bare_run_without_command_is_an_error() {
        assert_eq!(parse(&os(&["--"])), Err(ParseError::MissingCommand));
    }

    #[test]
    fn add_with_name_and_opts() {
        let action = parse(&os(&[
            "add",
            "build",
            "-p",
            "MemoryMax=1G",
            "--",
            "make",
            "-j2",
        ]))
        .unwrap();
        let Action::Run(spec) = action else {
            panic!("expected Run");
        };
        assert_eq!(spec.name, Some(s("build")));
        assert_eq!(spec.systemd_opts, os(&["-p", "MemoryMax=1G"]));
        assert_eq!(spec.command, os(&["make", "-j2"]));
    }

    #[test]
    fn add_name_derived_when_first_token_is_a_flag() {
        let action = parse(&os(&["add", "-p", "X=1", "--", "make"])).unwrap();
        let Action::Run(spec) = action else {
            panic!("expected Run");
        };
        assert_eq!(spec.name, None);
        assert_eq!(spec.systemd_opts, os(&["-p", "X=1"]));
    }

    #[test]
    fn add_separator_is_the_first_double_dash() {
        let action = parse(&os(&["add", "j", "--", "echo", "--", "x"])).unwrap();
        let Action::Run(spec) = action else {
            panic!("expected Run");
        };
        assert_eq!(spec.command, os(&["echo", "--", "x"]));
    }

    #[test]
    fn add_immediate_separator_has_no_name() {
        let action = parse(&os(&["add", "--", "foo", "bar"])).unwrap();
        let Action::Run(spec) = action else {
            panic!("expected Run");
        };
        assert_eq!(spec.name, None);
        assert_eq!(spec.command, os(&["foo", "bar"]));
    }

    #[test]
    fn add_requires_separator() {
        assert_eq!(parse(&os(&["add"])), Err(ParseError::MissingSeparator));
        assert_eq!(parse(&os(&["add", "x"])), Err(ParseError::MissingSeparator));
        assert_eq!(
            parse(&os(&["add", "-p", "X=1"])),
            Err(ParseError::MissingSeparator)
        );
    }

    #[test]
    fn add_requires_command_after_separator() {
        assert_eq!(
            parse(&os(&["add", "x", "--"])),
            Err(ParseError::MissingCommand)
        );
    }

    #[test]
    fn list_and_clean_take_no_arguments() {
        assert_eq!(parse(&os(&["list"])), Ok(Action::List));
        assert_eq!(parse(&os(&["clean"])), Ok(Action::Clean));
        assert_eq!(
            parse(&os(&["list", "extra"])),
            Err(ParseError::UnexpectedArgument {
                action: "list".into()
            })
        );
    }

    #[test]
    fn status_takes_exactly_one_name() {
        assert_eq!(parse(&os(&["status", "web"])), Ok(Action::Status(s("web"))));
        assert_eq!(
            parse(&os(&["status"])),
            Err(ParseError::MissingArgument {
                action: "status".into()
            })
        );
        assert_eq!(
            parse(&os(&["status", "a", "b"])),
            Err(ParseError::UnexpectedArgument {
                action: "status".into()
            })
        );
    }

    #[test]
    fn logs_forwards_journalctl_options() {
        let action = parse(&os(&["logs", "web", "-n", "50", "--follow"])).unwrap();
        let Action::Logs {
            name,
            journalctl_opts,
        } = action
        else {
            panic!("expected Logs");
        };
        assert_eq!(name, s("web"));
        assert_eq!(journalctl_opts, os(&["-n", "50", "--follow"]));
    }

    #[test]
    fn stop_and_remove_accept_multiple_names() {
        assert_eq!(
            parse(&os(&["stop", "a", "b"])),
            Ok(Action::Remove(vec![s("a"), s("b")]))
        );
        assert_eq!(
            parse(&os(&["remove", "a"])),
            Ok(Action::Remove(vec![s("a")]))
        );
        assert_eq!(
            parse(&os(&["stop"])),
            Err(ParseError::MissingArgument {
                action: "stop".into()
            })
        );
    }

    #[test]
    fn unknown_command() {
        assert_eq!(
            parse(&os(&["wat"])),
            Err(ParseError::UnknownCommand {
                command: "wat".into()
            })
        );
    }

    // -- error wording ------------------------------------------------------

    #[test]
    fn parse_errors_are_actionable() {
        assert_eq!(
            ParseError::UnknownCommand {
                command: "wat".into()
            }
            .to_string(),
            "unknown command 'wat' (run 'bgrun help' for usage)"
        );
        assert_eq!(
            ParseError::MissingSeparator.to_string(),
            "missing '--' before the command (usage: bgrun add [NAME] [opts] -- <cmd> [args])"
        );
        assert_eq!(
            ParseError::InvalidPrefix {
                value: "a[b".into()
            }
            .to_string(),
            "invalid BGRUN_PREFIX \"a[b\": use only ASCII letters, digits, '-' and '_'"
        );
    }
}
