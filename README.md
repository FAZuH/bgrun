<div align="center">

# bgrun

**Minimal background job runner for Linux — run any command as a transient systemd user unit.**

</div>

<hr>

<div align="center">
● <a href="#installation">Installation</a> ﻿ ● <a href="#usage">Usage</a> ﻿ ● <a href="#notes">Notes</a> ﻿ ● <a href="#docs">Docs</a> ﻿ ● <a href="#license">License</a>
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
bgrun remove dl                  # stop and forget the unit
```

The command must come after `--`. The job name defaults to the command's
basename. Everything between the name and `--` goes straight to `systemd-run`,
so unit properties can be overridden:

```sh
bgrun add build -p WorkingDirectory=$HOME/project -- make
bgrun add dl --working-directory=/tmp -pMemoryMax=1G -- wget URL
```

Run `bgrun help` for the full command list.

## Notes

- **Lingering** — user units die with your last session. If you launch jobs
  over SSH and log out, enable lingering once per machine:
  `loginctl enable-linger $USER`. `bgrun add` warns when it is off.
- Successful jobs disappear on their own (transient `--collect` units);
  `bgrun clean` only clears units that exited non-zero.
- The unit prefix is `bgrun`, override with `BGRUN_PREFIX` (letters, digits,
  `-` and `_` only).

## Docs

- [Changelog conventions](docs/dev/commit-changelog.md) — commit types and how the release changelog is generated

## License

[MIT](LICENSE)
