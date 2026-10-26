//! pnpm's content-addressable store: finding the stores projects use, so the
//! space they hold shows up as rows of their own.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Ecosystem label (and row name) for pnpm store rows.
pub const STORE_ECOSYSTEM: &str = "pnpm store";

/// Extract `storeDir` from the contents of a `node_modules/.modules.yaml`.
///
/// pnpm has written this file both as YAML (`storeDir: /p`) and as JSON
/// (`"storeDir": "/p",`), so rather than pull in a parser this finds the one
/// line that names the key and decodes its (possibly quoted) value. A
/// relative path is resolved against `base`, the `node_modules` directory
/// holding the file.
pub fn parse_store_dir(contents: &str, base: &Path) -> Option<PathBuf> {
    contents.lines().find_map(|line| {
        let rest = line.trim().trim_start_matches('"');
        let value = rest.strip_prefix("storeDir")?;
        let value = value
            .trim_start_matches('"')
            .trim_start()
            .strip_prefix(':')?
            .trim();
        let value = if let Some(quoted) = value.strip_prefix('"') {
            unquote_double(quoted)?
        } else if let Some(quoted) = value.strip_prefix('\'') {
            unquote_single(quoted)?
        } else {
            value.trim_end_matches(',').to_string()
        };
        if value.is_empty() {
            return None;
        }
        Some(base.join(value))
    })
}

/// Decode a JSON (or YAML double-quoted) string body up to its closing quote:
/// the Windows form `"C:\\Users\\…"` must lose its doubled backslashes.
/// `None` for a malformed escape or a missing closing quote.
fn unquote_double(body: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                c @ ('"' | '\\' | '/') => out.push(c),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let unit = hex4(&mut chars)?;
                    let units = if (0xd800..0xdc00).contains(&unit) {
                        // A surrogate pair spells one character in two escapes.
                        if chars.next()? != '\\' || chars.next()? != 'u' {
                            return None;
                        }
                        vec![unit, hex4(&mut chars)?]
                    } else {
                        vec![unit]
                    };
                    out.push(char::decode_utf16(units).next()?.ok()?);
                }
                _ => return None,
            },
            c => out.push(c),
        }
    }
    None
}

/// The four hex digits of a `\u` escape.
fn hex4(chars: &mut std::str::Chars) -> Option<u16> {
    let digits: String = chars.take(4).collect();
    // `from_str_radix` would also take a leading `+`.
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(&digits, 16).ok()
}

/// Decode a YAML single-quoted string body up to its closing quote, where a
/// doubled quote stands for one.
fn unquote_single(body: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            out.push(c);
        } else if chars.peek() == Some(&'\'') {
            chars.next();
            out.push('\'');
        } else {
            return Some(out);
        }
    }
    None
}

/// The store the pnpm project owning `node_modules` installed from, if it was
/// installed by pnpm.
pub fn store_dir_for(node_modules: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(node_modules.join(".modules.yaml")).ok()?;
    parse_store_dir(&contents, node_modules)
}

/// The versioned stores (`v3`, `v10`, …) under pnpm's default store location
/// for this platform, whether or not any project in the scan uses them.
pub fn default_store_dirs() -> Vec<PathBuf> {
    default_store_root_with(|name| std::env::var_os(name))
        .map(|root| version_dirs(&root))
        .unwrap_or_default()
}

/// The `v<N>` directories directly inside `base`, sorted.
fn version_dirs(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| is_version_dir(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

/// The pnpm major version whose store layout `store` is, read from its
/// `v<N>` directory name. pnpm 7 to 9 shared the `v3` layout, so that maps to 9.
pub fn store_major(store: &Path) -> Option<u32> {
    let name = store.file_name()?.to_str()?;
    if !is_version_dir(name) {
        return None;
    }
    match name[1..].parse::<u32>().ok()? {
        3 => Some(9),
        n if n >= 10 => Some(n),
        _ => None,
    }
}

/// Whether a pnpm of major version `installed` reads the store layout that
/// pnpm `store_major` writes.
fn reads_layout(installed: u32, store_major: u32) -> bool {
    installed == store_major || (store_major == 9 && (7..=9).contains(&installed))
}

/// The command that prunes `store`: the installed pnpm if it reads that
/// store's layout, otherwise the matching pnpm via `npx`, since a pnpm of
/// another version would look in a different `v<N>` directory. `None` for a
/// layout no pnpm writes, which no pnpm would find.
fn prune_command(store: &Path, installed_major: Option<u32>) -> Option<(String, Vec<String>)> {
    let major = store_major(store)?;
    // `--store-dir` takes the store root; pnpm appends its own `v<N>`.
    let store_dir = store.parent().unwrap_or(store);
    let mut args: Vec<String> = Vec::new();
    let program = match installed_major {
        Some(i) if reads_layout(i, major) => "pnpm",
        _ => {
            args.push("--yes".to_string());
            args.push(format!("pnpm@{major}"));
            "npx"
        }
    };
    args.extend(["store", "prune", "--store-dir"].map(String::from));
    args.push(store_dir.display().to_string());
    Some((program.to_string(), args))
}

/// Run `pnpm store prune` on `store`, removing packages no project references
/// anymore. On failure the error names the command so it can be run by hand.
pub fn prune(store: &Path) -> Result<(), String> {
    let Some((program, args)) = prune_command(store, installed_major()) else {
        let layout = store.file_name().unwrap_or(store.as_os_str());
        return Err(format!(
            "no pnpm version uses store layout {}, so none can prune it",
            layout.to_string_lossy()
        ));
    };
    let manual = manual_hint(&program, &args, shell_line);
    if !store.is_dir() {
        return Err(format!("store not found; {manual}"));
    }
    let output =
        run(&program, &args).map_err(|e| format!("could not start {program}: {e}; {manual}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let reason = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(str::trim)
        .unwrap_or("pnpm store prune failed");
    Err(format!("{reason}; {manual}"))
}

/// Major version of the pnpm on `PATH`, if there is one.
fn installed_major() -> Option<u32> {
    let output = run("pnpm", &["--version".to_string()]).ok()?;
    if !output.status.success() {
        return None;
    }
    parse_major(&String::from_utf8_lossy(&output.stdout))
}

/// The major version in `pnpm --version` output such as `10.4.1`.
fn parse_major(version: &str) -> Option<u32> {
    version.trim().split('.').next()?.parse().ok()
}

/// Run `program` with `args`, without stdin, and collect its output.
fn run(program: &str, args: &[String]) -> std::io::Result<Output> {
    let mut last = None;
    for name in launch_names(program) {
        match command(&name).args(args).output() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => last = Some(e),
            result => return result,
        }
    }
    Err(last.unwrap_or_else(|| std::io::ErrorKind::NotFound.into()))
}

/// The executable names to try for `program`, in order. On Windows `Command`
/// only finds `.exe` files on `PATH`, but npx and an npm- or corepack-installed
/// pnpm are `.cmd` shims. Arguments to those go through `cmd.exe`; Rust
/// escapes them safely from 1.77.2 on, hence the crate's `rust-version`.
/// A command for `program` that starts in [`launch_dir`] with no stdin.
fn command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(launch_dir()).stdin(Stdio::null());
    cmd
}

/// Where pnpm and npx run. Not the directory dda was started in: there a
/// project's `node_modules/.bin/pnpm`, `.npmrc` or `.pnpmfile.cjs` could
/// stand in for the real pnpm or change what it does.
fn launch_dir() -> PathBuf {
    launch_dir_with(|name| std::env::var_os(name))
}

/// The user's home directory, or the filesystem root without one.
fn launch_dir_with(var: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    var(home)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| PathBuf::from(std::path::MAIN_SEPARATOR_STR))
}

fn launch_names(program: &str) -> Vec<String> {
    let mut names = vec![program.to_string()];
    if cfg!(windows) {
        names.push(format!("{program}.cmd"));
    }
    names
}

/// How to run `program args…` by hand, as a line built by `line`. If the
/// store path can't be put into one safely (it comes from the scanned tree,
/// and pasting it must not run anything), a `<store>` placeholder stands in.
fn manual_hint(
    program: &str,
    args: &[String],
    line: impl Fn(&str, &[String]) -> Option<String>,
) -> String {
    if let Some(line) = line(program, args) {
        return format!("run manually: {line}");
    }
    let mut args = args.to_vec();
    if let Some(at) = args.iter().position(|a| a == "--store-dir") {
        args.truncate(at + 1);
        args.push("<store>".to_string());
    }
    match line(program, &args) {
        Some(line) => format!(
            "run manually: {line} (put in the store's path; a shell would expand parts of it)"
        ),
        None => format!("run `{program} store prune` by hand"),
    }
}

/// A command line to copy into the platform's shell, quoting arguments that
/// need it, or `None` if quoting can't keep an argument from being expanded.
fn shell_line(program: &str, args: &[String]) -> Option<String> {
    if cfg!(windows) {
        windows_line(program, args)
    } else {
        Some(posix_line(program, args))
    }
}

/// `program args…` for a POSIX shell.
fn posix_line(program: &str, args: &[String]) -> String {
    // Quote everything but plainly safe words, so no glob, separator,
    // expansion or empty argument can change what the line does.
    quoted_line(program, args, "@%+=:,./_-", |arg| {
        format!("'{}'", arg.replace('\'', r"'\''"))
    })
}

/// `program args…` for cmd.exe or PowerShell: both read "…" as one word.
/// cmd also splits words at `,`, `;` and `=`. `None` if an argument holds
/// what either expands even inside "…".
fn windows_line(program: &str, args: &[String]) -> Option<String> {
    if args
        .iter()
        .any(|a| a.chars().any(expands_in_windows_quotes))
    {
        return None;
    }
    Some(quoted_line(program, args, "@+:./_-\\", |arg| {
        format!("\"{arg}\"")
    }))
}

/// cmd expands `%VAR%` and `!VAR!` inside "…", PowerShell `$` and `` ` ``,
/// and PowerShell also ends a string at typographic double quotes.
fn expands_in_windows_quotes(c: char) -> bool {
    matches!(
        c,
        '"' | '%' | '!' | '$' | '`' | '\u{201C}' | '\u{201D}' | '\u{201E}'
    ) || c.is_control()
}

/// `program` followed by `args`, each quoted with `quote` unless it is
/// non-empty and made of ASCII letters, digits and `safe` only.
fn quoted_line(
    program: &str,
    args: &[String],
    safe: &str,
    quote: impl Fn(&str) -> String,
) -> String {
    let mut line = program.to_string();
    for arg in args {
        line.push(' ');
        let plain = !arg.is_empty()
            && arg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || safe.contains(c));
        if plain {
            line.push_str(arg);
        } else {
            line.push_str(&quote(arg));
        }
    }
    line
}

/// Whether projects' `node_modules` symlink into `store`: pnpm 11 keeps
/// packages under the store's `links/` and only links them into each project.
pub fn projects_link_into(store: &Path) -> bool {
    store.join("links").is_dir()
}

/// Whether `store` is laid out like a pnpm store: a layout pnpm writes (`v3`
/// or `v10`+) whose `files/` holds two-hex-digit content buckets, plus for
/// `v10`+ an `index.db` or an `index/` of buckets. The walk tests every
/// directory and `.modules.yaml` files name any path, so source folders such
/// as `app/api/v1/files` or a hostile `storeDir: /` must never become a
/// deletable row.
pub fn looks_like_store(store: &Path) -> bool {
    let Some(major) = store_major(store) else {
        return false;
    };
    holds_only_buckets(&store.join("files"))
        && (major < 10
            || store.join("index.db").is_file()
            || holds_only_buckets(&store.join("index")))
}

/// Whether `dir` has two-hex-digit bucket directories and no others. Loose
/// and hidden files are fine: pnpm leaves temp files in `files/`.
fn holds_only_buckets(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut buckets = 0;
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
        buckets += 1;
    }
    buckets > 0
}

/// Whether `name` looks like a store layout version directory (`v10`).
fn is_version_dir(name: &str) -> bool {
    name.strip_prefix('v')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// pnpm's default store location on this platform, reading environment
/// variables through `var`.
#[cfg(target_os = "macos")]
fn default_store_root_with(var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    Some(PathBuf::from(var("HOME")?).join("Library/pnpm/store"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn default_store_root_with(var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let data = var("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(data.join("pnpm/store"))
}

#[cfg(windows)]
fn default_store_root_with(var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    Some(
        PathBuf::from(var("LOCALAPPDATA")?)
            .join("pnpm")
            .join("store"),
    )
}

#[cfg(not(any(unix, windows)))]
fn default_store_root_with(_var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_store_dir_yaml_style() {
        let yaml =
            "layoutVersion: 5\nstoreDir: /Users/x/Library/pnpm/store/v10\nvirtualStoreDir: .pnpm\n";
        assert_eq!(
            parse_store_dir(yaml, Path::new("/p/node_modules")),
            Some(PathBuf::from("/Users/x/Library/pnpm/store/v10"))
        );
        let quoted = "storeDir: '/Users/x/store/v3'\n";
        assert_eq!(
            parse_store_dir(quoted, Path::new("/p/node_modules")),
            Some(PathBuf::from("/Users/x/store/v3"))
        );
    }

    #[test]
    fn parse_store_dir_json_style() {
        let json = "{\n  \"layoutVersion\": 5,\n  \"storeDir\": \"/Users/x/Library/pnpm/store/v11\",\n  \"virtualStoreDir\": \".pnpm\"\n}\n";
        assert_eq!(
            parse_store_dir(json, Path::new("/p/node_modules")),
            Some(PathBuf::from("/Users/x/Library/pnpm/store/v11"))
        );
    }

    #[test]
    fn parse_store_dir_decodes_windows_json_paths() {
        let json = "{\n  \"storeDir\": \"C:\\\\Users\\\\me\\\\AppData\\\\Local\\\\pnpm\\\\store\\\\v10\",\n}\n";
        assert_eq!(
            parse_store_dir(json, Path::new("/p/node_modules")),
            Some(Path::new("/p/node_modules").join(r"C:\Users\me\AppData\Local\pnpm\store\v10"))
        );
    }

    #[test]
    fn parse_store_dir_decodes_quoted_escapes() {
        let base = Path::new("/p");
        let dq = "\"storeDir\": \"/s/a\\\"b\\/c\\u00e9\\ud83d\\ude00\",\n";
        assert_eq!(
            parse_store_dir(dq, base),
            Some(PathBuf::from("/s/a\"b/c\u{e9}\u{1f600}"))
        );
        // YAML single quotes escape a quote by doubling it.
        assert_eq!(
            parse_store_dir("storeDir: '/s/it''s'\n", base),
            Some(PathBuf::from("/s/it's"))
        );
        // A broken escape is not a path to trust.
        assert_eq!(parse_store_dir("storeDir: \"/s/\\u12\"\n", base), None);
        assert_eq!(parse_store_dir("storeDir: \"/s/open\n", base), None);
    }

    #[test]
    fn parse_store_dir_relative_is_resolved_against_base() {
        assert_eq!(
            parse_store_dir(
                "storeDir: ../.pnpm-store/v10\n",
                Path::new("/p/node_modules")
            ),
            Some(PathBuf::from("/p/node_modules/../.pnpm-store/v10"))
        );
    }

    #[test]
    fn parse_store_dir_missing_returns_none() {
        assert_eq!(parse_store_dir("layoutVersion: 5\n", Path::new("/p")), None);
        assert_eq!(parse_store_dir("storeDir:\n", Path::new("/p")), None);
        assert_eq!(parse_store_dir("\u{0}\u{1}garbage", Path::new("/p")), None);
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn store_major_maps_layout_to_pnpm_version() {
        assert_eq!(store_major(Path::new("/s/store/v10")), Some(10));
        assert_eq!(store_major(Path::new("/s/store/v11")), Some(11));
        // pnpm 7 to 9 all wrote the v3 layout.
        assert_eq!(store_major(Path::new("/s/store/v3")), Some(9));
        assert_eq!(store_major(Path::new("/x/custom")), None);
    }

    #[test]
    fn prune_command_uses_matching_pnpm() {
        let store = Path::new("/s/store/v10");
        assert_eq!(
            prune_command(store, Some(10)),
            Some((
                "pnpm".to_string(),
                args(&["store", "prune", "--store-dir", "/s/store"])
            ))
        );
        let via_npx = Some((
            "npx".to_string(),
            args(&[
                "--yes",
                "pnpm@10",
                "store",
                "prune",
                "--store-dir",
                "/s/store",
            ]),
        ));
        assert_eq!(prune_command(store, Some(11)), via_npx);
        assert_eq!(prune_command(store, None), via_npx);
        // pnpm 8 still reads the v3 layout, so no download is needed.
        assert_eq!(
            prune_command(Path::new("/s/store/v3"), Some(8)).map(|c| c.0),
            Some("pnpm".to_string())
        );
    }

    #[test]
    fn unknown_store_layouts_have_no_prune_command() {
        // No pnpm writes these, so none would look in the right place.
        for store in ["/s/store/v1", "/s/store/v5", "/s/store/v9", "/x/custom"] {
            assert_eq!(prune_command(Path::new(store), Some(10)), None, "{store}");
        }
    }

    #[test]
    fn pruning_an_unknown_layout_fails_instead_of_pruning_elsewhere() {
        let tmp = std::env::temp_dir().join(format!("dda-pnpm-{}-unknown", std::process::id()));
        let store = tmp.join("v5");
        std::fs::create_dir_all(store.join("files")).unwrap();
        let err = prune(&store).unwrap_err();
        assert!(err.contains("v5"), "{err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn launch_tries_cmd_shims_on_windows() {
        // npx is only ever `npx.cmd` on Windows, and so is an npm- or
        // corepack-installed pnpm; `Command` alone only finds `.exe` files.
        let names = launch_names("npx");
        assert_eq!(names[0], "npx");
        assert_eq!(names.contains(&"npx.cmd".to_string()), cfg!(windows));
    }

    #[test]
    fn shell_line_quotes_anything_a_shell_would_interpret() {
        let line = posix_line(
            "pnpm",
            &args(&[
                "store",
                "--store-dir",
                "/a b/it's",
                "$HOME",
                "*",
                "x;y",
                "a&b",
                "~",
                "",
                "C:\\Users",
                "pnpm@10",
            ]),
        );
        assert_eq!(
            line,
            r"pnpm store --store-dir '/a b/it'\''s' '$HOME' '*' 'x;y' 'a&b' '~' '' 'C:\Users' pnpm@10"
        );
    }

    #[test]
    fn windows_lines_use_double_quotes() {
        // cmd.exe and PowerShell both take "…"; single quotes are no quoting
        // to cmd, and it splits words at `,`, `;` and `=` too.
        let line = windows_line(
            "pnpm.cmd",
            &args(&[
                "store",
                "--store-dir",
                r"C:\Users\Jo Doe\AppData\Local\pnpm\store\v10",
                r"C:\plain\v10",
                "a,b",
                "x=y",
                "x;y",
                "a&b",
                "(x)",
                "",
                "pnpm@10",
            ]),
        );
        assert_eq!(
            line.as_deref(),
            Some(
                r#"pnpm.cmd store --store-dir "C:\Users\Jo Doe\AppData\Local\pnpm\store\v10" C:\plain\v10 "a,b" "x=y" "x;y" "a&b" "(x)" "" pnpm@10"#
            )
        );
    }

    #[test]
    fn windows_lines_are_withheld_when_quotes_cannot_protect_them() {
        // Inside "…" cmd still expands %VAR% and !VAR!, PowerShell $(…),
        // $var and `-escapes, and PowerShell also ends a string at “ ” „.
        for path in [
            r"C:\%PATH%\v10",
            r"C:\!x!\v10",
            r"C:\$(calc)\v10",
            r"C:\a`b\v10",
            "C:\\a\"b\\v10",
            "C:\\a\u{201d}; calc; \u{201c}\\v10",
            "C:\\a\nb\\v10",
        ] {
            let line = windows_line("pnpm", &args(&["store", "prune", "--store-dir", path]));
            assert_eq!(line, None, "{path}");
        }
    }

    #[test]
    fn manual_hint_leaves_out_paths_a_shell_would_expand() {
        let plain = args(&["store", "prune", "--store-dir", r"C:\s\v10"]);
        assert_eq!(
            manual_hint("pnpm", &plain, windows_line),
            r"run manually: pnpm store prune --store-dir C:\s\v10"
        );
        let hostile = args(&["store", "prune", "--store-dir", r"C:\$(calc)\v10"]);
        assert_eq!(
            manual_hint("pnpm", &hostile, windows_line),
            r#"run manually: pnpm store prune --store-dir "<store>" (put in the store's path; a shell would expand parts of it)"#
        );
    }

    #[test]
    fn shell_line_quotes_for_the_platform_shell() {
        let args = args(&["--store-dir", "/a b"]);
        let expected = if cfg!(windows) {
            windows_line("pnpm", &args)
        } else {
            Some(posix_line("pnpm", &args))
        };
        assert_eq!(shell_line("pnpm", &args), expected);
    }

    #[test]
    fn pnpm_version_output_gives_the_major() {
        assert_eq!(parse_major("10.4.1\n"), Some(10));
        assert_eq!(parse_major("11.0.0-rc.1"), Some(11));
        assert_eq!(parse_major("  9\n"), Some(9));
        assert_eq!(parse_major(""), None);
        assert_eq!(parse_major("pnpm: command not found"), None);
    }

    #[test]
    fn only_version_dirs_count_as_default_stores() {
        let tmp = std::env::temp_dir().join(format!("dda-pnpm-{}-defaults", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        for dir in ["v3", "v10", "tmp", "v10-old"] {
            std::fs::create_dir_all(tmp.join(dir)).unwrap();
        }
        std::fs::write(tmp.join("v11"), b"not a dir").unwrap();
        assert_eq!(version_dirs(&tmp), vec![tmp.join("v10"), tmp.join("v3")]);
        assert!(version_dirs(&tmp.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// An environment lookup over fixed variables.
    fn env(vars: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars = vars.to_vec();
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn default_store_root_is_under_library() {
        assert_eq!(
            default_store_root_with(env(&[("HOME", "/Users/me")])),
            Some(PathBuf::from("/Users/me/Library/pnpm/store"))
        );
        assert_eq!(default_store_root_with(env(&[])), None);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn default_store_root_follows_xdg_data_home() {
        let home = ("HOME", "/home/me");
        assert_eq!(
            default_store_root_with(env(&[home, ("XDG_DATA_HOME", "/data")])),
            Some(PathBuf::from("/data/pnpm/store"))
        );
        // Empty or unset falls back to the XDG default under HOME.
        for vars in [vec![home, ("XDG_DATA_HOME", "")], vec![home]] {
            assert_eq!(
                default_store_root_with(env(&vars)),
                Some(PathBuf::from("/home/me/.local/share/pnpm/store"))
            );
        }
        assert_eq!(default_store_root_with(env(&[])), None);
    }

    #[cfg(windows)]
    #[test]
    fn default_store_root_is_under_local_app_data() {
        assert_eq!(
            default_store_root_with(env(&[("LOCALAPPDATA", r"C:\Users\me\AppData\Local")])),
            Some(PathBuf::from(r"C:\Users\me\AppData\Local\pnpm\store"))
        );
        assert_eq!(default_store_root_with(env(&[])), None);
    }

    #[test]
    fn prune_failure_names_the_command_to_run() {
        let err = prune(Path::new("/definitely/not/a/store/v10")).unwrap_err();
        assert!(
            err.contains("store prune --store-dir /definitely/not/a/store"),
            "{err}"
        );
    }

    #[test]
    fn store_layout_needs_version_name_and_files() {
        let tmp = std::env::temp_dir().join(format!("dda-pnpm-{}-layout", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mk = |p: &str| std::fs::create_dir_all(tmp.join(p)).unwrap();
        let touch = |p: &str| std::fs::write(tmp.join(p), b"").unwrap();
        // pnpm 10: hex buckets under both `files/` and `index/`.
        mk("v10/files/00");
        mk("v10/files/ff");
        mk("v10/index/0a");
        mk("v10/projects");
        // pnpm 11: `index.db`; stray files and hidden entries in `files/`.
        mk("v11/files/3c");
        touch("v11/files/stream942634");
        touch("v11/files/.DS_Store");
        touch("v11/index.db");
        mk("v11/links");
        // pnpm 7-9 (`v3`): only `files/`, with two-hex-digit buckets.
        mk("v3/files/0a");
        mk("v3/tmp");
        // Look-alikes: a user folder with `files/` and a bare version dir.
        mk("v2/files/photos");
        mk("v12");
        mk("other/files/00");
        mk("other/index/00");
        assert!(looks_like_store(&tmp.join("v10")));
        assert!(looks_like_store(&tmp.join("v11")));
        assert!(looks_like_store(&tmp.join("v3")));
        assert!(!looks_like_store(&tmp.join("v2")));
        assert!(!looks_like_store(&tmp.join("v12")));
        assert!(!looks_like_store(&tmp.join("other")));
        assert!(!looks_like_store(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn source_folders_laid_out_like_a_store_are_not_stores() {
        // API routes such as `app/api/v1/files` share names with a store.
        let tmp = std::env::temp_dir().join(format!("dda-pnpm-{}-lookalike", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mk = |p: &str| std::fs::create_dir_all(tmp.join(p)).unwrap();
        let touch = |p: &str| std::fs::write(tmp.join(p), b"").unwrap();
        // A version pnpm never wrote, even with hex buckets.
        mk("v1/files/00");
        mk("v1/projects");
        mk("v4/files/00");
        touch("v4/index.db");
        // `v3` whose `files/` holds source, not buckets.
        mk("v3/files/[id]");
        touch("v3/files/route.ts");
        mk("v3/links");
        // A bucket next to another directory.
        mk("v3b/v3/files/00");
        mk("v3b/v3/files/docs");
        // `v10` with buckets in `files/` but none in `index/`, or no index.
        mk("v10/files/00");
        mk("v10/index/[id]");
        touch("v10/index/route.ts");
        mk("v11/files/ab");
        mk("v11/projects");
        mk("v11/links");
        // Empty `files/` and `index/`.
        mk("v12/files");
        mk("v12/index");
        for dir in ["v1", "v4", "v3", "v3b/v3", "v10", "v11", "v12"] {
            assert!(!looks_like_store(&tmp.join(dir)), "{dir}");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn version_dirs_are_recognised() {
        assert!(is_version_dir("v10"));
        assert!(is_version_dir("v3"));
        assert!(!is_version_dir("v"));
        assert!(!is_version_dir("tmp"));
        assert!(!is_version_dir("v10-old"));
    }

    #[test]
    fn store_dir_for_reads_modules_yaml() {
        let tmp = std::env::temp_dir().join(format!("dda-pnpm-{}-modyaml", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        assert_eq!(store_dir_for(&tmp), None);
        // A real absolute path, so it also carries a drive on Windows.
        let store = tmp.join("store").join("v10");
        let yaml = format!("storeDir: {}\n", store.display());
        std::fs::write(tmp.join(".modules.yaml"), yaml).unwrap();
        assert_eq!(store_dir_for(&tmp), Some(store));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn launch_dir_is_the_home_directory() {
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let home = std::env::temp_dir();
        let with_home = |name: &str| (name == home_var).then(|| home.clone().into_os_string());
        assert_eq!(launch_dir_with(with_home), home);
    }

    #[test]
    fn launch_dir_falls_back_to_the_root() {
        let root = PathBuf::from(std::path::MAIN_SEPARATOR_STR);
        assert_eq!(launch_dir_with(|_| None), root);
        assert_eq!(launch_dir_with(|_| Some(OsString::new())), root);
        let missing = |_: &str| Some(OsString::from("/definitely/not/a/home"));
        assert_eq!(launch_dir_with(missing), root);
    }

    #[test]
    fn commands_do_not_run_in_the_current_directory() {
        let cmd = command("pnpm");
        assert_eq!(cmd.get_current_dir(), Some(launch_dir().as_path()));
    }
}
