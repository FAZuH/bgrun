---
name: bgrun
description: "Run a command in the background as a systemd user unit with the bgrun CLI, then list, check, tail the logs of, or stop it later. Use whenever a command must outlive the shell or the agent turn — long builds, downloads, servers, watchers, dev daemons — or when the user says run it in the background, detached, as a daemon, in a service, until reboot, restart it if it fails, mentions nohup, disown, trailing ampersand, systemd-run, background job, or asks about a job bgrun started earlier."
---

# bgrun

`bgrun` runs a command as a transient systemd user unit, so the job survives
your shell exiting and leaves a name you can check, tail, and stop later.

Prefer it over `cmd &` (dies with the shell), `nohup` (no status, no cleanup),
and hand-written `systemd-run` (no naming convention, nothing to remember).
For a command you are going to wait on anyway, just run it.

This skill is documentation, not the binary. If `bgrun` is missing, install it
first: `cargo install --git https://github.com/FAZuH/bgrun`.

## The `--` is not optional

The first argument is the subcommand, so this is a **usage error**, not a
background job:

```sh
bgrun make -j8        # error: unknown command 'make'
bgrun -- make -j8     # correct
```

## Command forms

| command | what it does |
|---|---|
| `bgrun [--flags] [overrides] -- <cmd>` | run in the background, name from the command's basename |
| `bgrun add [NAME] [flags] [overrides] -- <cmd>` | same, with an explicit name |
| `bgrun list` | every `bgrun-*` unit, running or failed |
| `bgrun status <name>` | systemd status for one job |
| `bgrun logs <name> [journalctl opts]` | its journal, e.g. `--follow`, `-n 200` |
| `bgrun stop` / `bgrun remove <name>...` | stop and forget (aliases) |
| `bgrun clean` | forget units that exited non-zero |

`add` is only there to name the job. Reach for the bare form unless the
user has a name in mind.

## Naming

`bgrun add` takes the first token as the name when it does not start with `-`;
otherwise the name is the command's basename. Characters outside
`[A-Za-z0-9:_.-]` become `-`. Names are global to the prefix: a second job with
a taken name fails — for a transient job the error points at `bgrun logs` and
`bgrun remove` — so do not invent a second name to work around it.

Give a name anything you will want to type later. `bgrun add build -- make -j8`
beats a derived `bgrun-make` you have to remember.

## Overrides

Everything between the name and `--` is handed to `systemd-run` untouched:

```sh
bgrun add build -p WorkingDirectory=$HOME/project -- make
bgrun add dl --working-directory=/tmp -pMemoryMax=1G -- wget URL
```

`BGRUN_PREFIX` renames every unit at once (`bgrun-e2e-download` instead of
`bgrun-dl`); useful when two projects would otherwise fight over a name.

## Long-lived jobs

Two flags, in front of the overrides, in either form — `-r` and `-b` are the
short forms:

```sh
bgrun -r -- ./server
bgrun add sync -b -- ./sync.sh ~/data
```

`--restart` sets `Restart=on-failure`, so a crash is retried. systemd's own
start limit still applies — 5 starts per 10 seconds, then it gives up and
`--collect` removes the unit, so check `bgrun logs` rather than assuming a
retry loop is still running.

`--persist` writes a real unit file and enables it, so the job also starts at
every boot. Four consequences worth respecting:

- It needs **lingering** (`loginctl enable-linger $USER`) to start at boot;
  without it the job starts at your next login instead.
- `bgrun remove <name>` is what deletes the unit file. Until you do, the job
  returns at every boot. Never `--persist` something you cannot name later.
- Only `-p KEY=VALUE` overrides can be written to a unit file;
  `--working-directory=` and friends are rejected instead of silently dropped.
- If the name is already taken by a running transient job, bgrun refuses and
  tells the user to `bgrun remove` it first. It cannot adopt a running job,
  so do not try to work around this by renaming — the user has to decide
  whether the running job may be stopped.

## After launching

`bgrun add` returning 0 means systemd accepted the unit, not that the command
is doing what you wanted. For anything long enough to matter, follow up with
`bgrun status <name>`, or `bgrun logs <name> -n 50` once, before you report
the job as running.

Clean up when the job is done: `bgrun remove <name>`. Successful transient
jobs delete themselves, but a failed one lingers as a `failed` unit until
`bgrun clean` or `bgrun remove`.

## Gotchas

- **Lingering** — user units die with your last session. Over SSH, enable it
  once per machine; `bgrun add` warns when it is off.
- A job that exits 0 disappears from `bgrun list` immediately (transient units
  are created with `--collect`). An empty list does not mean nothing ran.
- Two jobs cannot share a name; the second `add` fails rather than replacing
  the first.
- `bgrun logs` takes journalctl options after the name, not bgrun options.
