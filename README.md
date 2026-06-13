# dependency-dir-analyzer

A small, fast cross-platform CLI that scans a directory tree for **dependency
directories** — the folders package managers and build tools generate and that
can usually be regenerated from source (`node_modules`, `target`, `.venv`,
`vendor`, `.gradle`, …) — and shows how much disk space they use and when each
was last touched.

Great for answering "where did all my disk space go?" and for finding
reclaimable space across a folder full of projects.

The binary is called **`dda`**.

## Features

- **Interactive, live-updating TUI** — results stream in and re-sort as the
  scan runs; you don't wait for it to finish. Scroll, re-sort, and quit.
- **Size _and_ last-modified time** — stale directories (good cleanup
  candidates) are colour-coded by age, so you can tell a six-month-old
  `node_modules` from one you built this morning.
- **Fast** — a parallel filesystem walk (via `rayon`) that sizes each matched
  subtree once and never descends into it twice.
- **Scriptable output** — `--format json` or `--format csv` for piping into
  other tools.
- **Shell completions** for bash, zsh, fish, PowerShell and elvish.

## Example

On a terminal, `dda` opens an interactive view that fills in as it scans:

```text
┌ dependency-dir-analyzer ──────────────────────────────────────────────────┐
│ dda   ~/code   ⠹ scanning                                                  │
├────────────────────────────────────────────────────────────────────────────┤
│ Size       ▏          Files  Modified  Ecosystem    Path                    │
│ 1.2 GB     ████████   48210  2d ago    Node.js      web-app/node_modules    │
│ 420 MB     ███        1182   5mo ago   Rust / Maven cli-tool/target         │
│ 180 MB     █          640    1y ago    Python (venv).venv                   │
│ ⠹ sizing…             —      —         Go / PHP …   api/vendor              │
├────────────────────────────────────────────────────────────────────────────┤
│ 1.84 GB   3 shown · 3 sized · 4 found   sort: size                          │
│ ↑/↓ move   s sort   g/G top/bottom   q quit                                 │
└────────────────────────────────────────────────────────────────────────────┘
```

Piped or redirected, it prints a plain aligned table instead:

```console
$ dda ~/code | cat
1.2 GB    2d ago   Node.js          /home/me/code/web-app/node_modules
420 MB    5mo ago  Rust / Maven     /home/me/code/cli-tool/target
180 MB    1y ago   Python (venv)    /home/me/code/ml/.venv
 36 MB    3w ago   Go / PHP / Ruby  /home/me/code/api/vendor

1.84 GB across 4 directories (52831 files)
```

## Usage

```text
Usage: dda [OPTIONS] [PATH]
       dda completions <SHELL>

Arguments:
  [PATH]  Root directory to scan [default: .]

Options:
  -f, --format <FORMAT>    Output format [default: table] [possible values: table, json, csv]
  -m, --min-size <SIZE>    Only report directories at least this large (e.g. 10MB, 1.5GB, 500K)
  -t, --top <TOP>          Show at most this many results (largest first); 0 = no limit [default: 0]
      --follow-symlinks    Follow symbolic links while scanning
      --plain              Print a plain static table instead of the interactive TUI
      --no-summary         Do not print the summary footer (non-interactive output)
  -h, --help               Print help
  -V, --version            Print version
```

The interactive TUI is used automatically when the default `table` format
writes to a terminal. It falls back to the plain table when the output is piped
or redirected, or when you pass `--plain`. `--format json` and `--format csv`
are always non-interactive.

### TUI keys

| Key            | Action                                   |
| -------------- | ---------------------------------------- |
| `↑` / `↓`, `j` / `k` | Move the selection                  |
| `PgUp` / `PgDn`      | Jump a page                         |
| `g` / `G`            | Jump to the top / bottom            |
| `s`                  | Cycle sort (size → modified → name → files) |
| `q` / `Esc`          | Quit                                |

Examples:

```bash
dda                       # interactively scan the current directory
dda ~/code --top 10       # the 10 biggest dependency dirs under ~/code
dda . --min-size 100MB    # only directories ≥ 100 MB
dda . --format json | jq  # machine-readable JSON
dda . --format csv > out.csv  # machine-readable CSV
```

## Shell completions

Generate a completion script with `dda completions <shell>` and install it
where your shell looks for completions:

```bash
# bash
dda completions bash | sudo tee /etc/bash_completion.d/dda > /dev/null

# zsh (somewhere on your $fpath)
dda completions zsh > "${fpath[1]}/_dda"

# fish
dda completions fish > ~/.config/fish/completions/dda.fish
```

`powershell` and `elvish` are supported too.

## Detected directories

`node_modules`, `bower_components`, `.next`, `.nuxt`, `.svelte-kit`, `.angular`,
`target`, `.venv`, `venv`, `__pycache__`, `.tox`, `.mypy_cache`, `.pytest_cache`,
`vendor`, `.bundle`, `.gradle`, `.dart_tool`, `elm-stuff`, `Pods`, `.terraform`,
`DerivedData`, `.cargo`, `.cache`.

When a directory matches, its whole subtree is sized as one unit and the scanner
does not descend further, so nested matches are never double-counted.

## Install

### From a release

Download the archive for your platform from the
[Releases](../../releases) page, extract it, and put `dda` on your `PATH`.

### From source

```bash
cargo install --path .
# or, with mise:
mise run install
```

## Development

This project uses [mise](https://mise.jdx.dev) to manage the Rust toolchain and
as a task runner.

```bash
mise install      # install the pinned Rust toolchain
mise run build    # build
mise run test     # run tests
mise run ci       # fmt-check + lint + test + release build (mirrors CI)
mise tasks        # list all tasks
```

See [CLAUDE.md](CLAUDE.md) for a fuller tour of the codebase.

## License

MIT — see [LICENSE](LICENSE).
