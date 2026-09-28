<div align="center">

# bgrun

**Minimal background job runner for Linux — run any command as a systemd user unit.**

</div>

<hr>

<div align="center">
● <a href="#installation">Installation</a> ﻿ ● <a href="#usage">Usage</a> ﻿ ● <a href="#notes">Notes</a> ﻿ ● <a href="#agent-skill">Agent skill</a> ﻿ ● <a href="#docs">Docs</a> ﻿ ● <a href="#license">License</a>
</div>

## Installation

```sh
cargo install --git https://github.com/FAZuH/bgrun
```

Or download a prebuilt Linux binary from [Releases](https://github.com/FAZuH/bgrun/releases), drop it on your `PATH`.

Requires a Linux system with systemd (user session, `systemd-run`, `journalctl`).

### Agent skill

[bgrun ships an agent skill](skills/bgrun/SKILL.md) so a coding assistant
reaches for `bgrun` instead of a bare `&`:

```sh
npx skills add FAZuH/bgrun
```

## Usage

```sh
bgrun -- make -j8                # run in background, name auto-derived ("make")
bgrun add dl -- wget URL         # named job "dl"
bgrun list                       # all bgrun-* units
bgrun logs dl                    # journalctl for the job
bgrun watch dl                   # stream it, then exit with the job's status
bgrun stop dl                    # stop for now, keep the job
bgrun resume dl                  # start it again
bgrun remove dl                  # stop and forget it for good
```

Run `bgrun help` for the full command list.

## Notes

- **Lingering** — user units die with your last session. If you launch jobs
  over SSH and log out, enable lingering once per machine:
  `loginctl enable-linger $USER`. `bgrun add` warns when it is off.
- Successful jobs disappear on their own (transient `--collect` units);
  `bgrun clean` only clears units that exited non-zero.
- `bgrun stop` is temporary and `bgrun resume` undoes it, for any job. A job
  without `--persist` is paused by saving its own unit definition into
  `~/.config/systemd/user` *before* it stops, so `resume` starts the same
  command, working directory and properties again — `--restart` included. If
  that definition cannot be saved, the stop is refused and the job keeps
  running rather than becoming unresumable — and a stop that then fails takes
  the saved definition back with it, so a job that is still running is never
  left resumable. Two limits: a pause does not
  survive a reboot (that is `--persist`), and it cannot help a job that ended
  some other way — one that finished, or that a failed dependency or a
  hand-run `systemctl --user stop` halted — because its definition is already
  gone. Those need `bgrun add` again.
- `--restart` is `Restart=on-failure`, so systemd's own start limit still
  applies: 5 starts per 10s, after which the job gives up and is collected.
- `--persist` is the one thing that is not transient — a transient unit
  cannot be enabled, so bgrun writes a unit file under the systemd user unit
  directory (`~/.config/systemd/user`) and enables it. Only `-p KEY=VALUE`
  overrides can be written to a unit file. The job then starts at every boot,
  which is exactly why `bgrun remove` is what deletes that file again — and the
  definition a paused job saved, so a job leaves nothing behind either way. A
  name already taken by a running transient job is refused: stop it first. The
  command is resolved to an absolute path exactly as `systemd-run` does,
  because a unit file only searches systemd's own list for a bare name — a
  command that is in nobody's `PATH` is refused instead of failing at boot.
- The unit prefix is `bgrun`, override with `BGRUN_PREFIX` (letters, digits,
  `-` and `_` only).

## Docs

- [Changelog conventions](docs/dev/commit-changelog.md) — commit types and how the release changelog is generated

## License

[MIT](LICENSE)
