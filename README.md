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

## Usage

```sh
bgrun -- make -j8                # run in background, name auto-derived ("make")
bgrun add dl -- wget URL         # named job "dl"
bgrun list                       # all bgrun-* units
bgrun logs dl                    # journalctl for the job
bgrun stop dl                    # stop for now, keep the job
bgrun resume dl                  # start it again
bgrun remove dl                  # stop and forget it for good
```

The command must come after `--`. The job name defaults to the command's
basename. Everything between the name and `--` goes straight to `systemd-run`,
so unit properties can be overridden:

```sh
bgrun add build -p WorkingDirectory=$HOME/project -- make
bgrun add dl --working-directory=/tmp -pMemoryMax=1G -- wget URL
```

Two flags of bgrun's own work in either form, in front of the overrides:

```sh
bgrun --restart -- ./server               # or -r: retry whenever it exits non-zero
bgrun add api --persist -- ./server       # or -b: also runs at every boot
bgrun -b -- ./sync.sh ~/data              # name derived, no `add` needed
```

Run `bgrun help` for the full command list.

## Notes

- **Lingering** — user units die with your last session. If you launch jobs
  over SSH and log out, enable lingering once per machine:
  `loginctl enable-linger $USER`. `bgrun add` warns when it is off.
- Successful jobs disappear on their own (transient `--collect` units);
  `bgrun clean` only clears units that exited non-zero.
- `bgrun stop` is temporary and `bgrun resume` undoes it, but only for a
  job added with `--persist`: a transient unit is collected the moment it
  stops, so stopping one is final. `bgrun remove` is the only way to be sure.
- `--restart` is `Restart=on-failure`, so systemd's own start limit still
  applies: 5 starts per 10s, after which the job gives up and is collected.
- `--persist` is the one thing that is not transient — a transient unit
  cannot be enabled, so bgrun writes a unit file under the systemd user unit
  directory (`~/.config/systemd/user`) and enables it. Only `-p KEY=VALUE`
  overrides can be written to a unit file. The job then starts at every boot,
  which is exactly why `bgrun remove` is what deletes that file again. A name
  already taken by a running transient job is refused: stop it first.
- The unit prefix is `bgrun`, override with `BGRUN_PREFIX` (letters, digits,
  `-` and `_` only).

## Agent skill

[bgrun ships an agent skill](skills/bgrun/SKILL.md) so a coding assistant
reaches for `bgrun` instead of a bare `&`:

```sh
npx skills add FAZuH/bgrun
```

## Docs

- [Changelog conventions](docs/dev/commit-changelog.md) — commit types and how the release changelog is generated

## License

[MIT](LICENSE)
