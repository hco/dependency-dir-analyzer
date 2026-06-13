# dependency-dir-analyzer

A small, fast cross-platform CLI that scans a directory tree for **dependency
directories** — the folders package managers and build tools generate and that
can usually be regenerated from source (`node_modules`, `target`, `.venv`,
`vendor`, `.gradle`, …) — and tells you how much disk space they use.

Great for answering "where did all my disk space go?" and for finding
reclaimable space across a folder full of projects.

The binary is called **`dda`**.

## Example

```console
$ dda ~/code
1.2 GB    Node.js          /home/me/code/web-app/node_modules
420 MB    Rust / Maven     /home/me/code/cli-tool/target
180 MB    Python (venv)    /home/me/code/ml/.venv
 36 MB    Go / PHP / Ruby  /home/me/code/api/vendor

1.84 GB across 4 directories (52831 files)
```

## Usage

```text
Usage: dda [OPTIONS] [PATH]

Arguments:
  [PATH]  Root directory to scan [default: .]

Options:
  -f, --format <FORMAT>    Output format [default: table] [possible values: table, json]
  -m, --min-size <SIZE>    Only report directories at least this large (e.g. 10MB, 1.5GB, 500K)
  -t, --top <TOP>          Show at most this many results (largest first); 0 = no limit [default: 0]
      --follow-symlinks    Follow symbolic links while scanning
      --no-summary         Do not print the summary footer (table format only)
  -h, --help               Print help
  -V, --version            Print version
```

Examples:

```bash
dda                       # scan the current directory
dda ~/code --top 10       # the 10 biggest dependency dirs under ~/code
dda . --min-size 100MB    # only directories ≥ 100 MB
dda . --format json | jq  # machine-readable output
```

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
