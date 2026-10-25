# Freigebbar-Größe & pnpm-Stores Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** dda zeigt standardmäßig, was Löschen wirklich freigibt (Clone-/Hardlink-bewusst), umschaltbar auf belegte Größe, und listet pnpm-Stores als Zeilen, die per `pnpm store prune` (oder komplett) geleert werden.

**Architecture:** Ein neues Modul `usage` misst pro Datei allozierte Bytes, Inode/Linkzahl und (macOS) Clone-ID/-Refcount und akkumuliert pro Ordner `allocated`/`freeable`. Ein Modul `pnpm` findet Stores (`.modules.yaml` + Standard-Ort) und führt Prune aus. Scanner, Report und TUI tragen beide Werte; ein `SizeMode` wählt, welcher angezeigt/sortiert/gefiltert wird.

**Tech Stack:** Rust 1.88 (mise), ratatui/crossterm, rayon, walkdir, `libc` (neu, nur `cfg(unix)`).

**Spec:** `docs/superpowers/specs/2026-10-25-freigebbar-design.md`

## Global Constraints

- `mise run ci` (fmt-check, clippy `-D warnings`, test, release build) bleibt grün.
- Neue Abhängigkeit nur `libc` als `[target.'cfg(unix)'.dependencies]`; keine YAML-Crate.
- Plattformcode hinter `cfg`; Linux/Windows müssen weiter kompilieren (Windows: freeable = allocated = `len()`).
- Snapshots werden ignoriert: **kein** `ATTR_CMNEXT_PRIVATESIZE`.
- `ATTR_CMNEXT_CLONE_REFCNT = 0x0000_1000` selbst definieren (fehlt in `libc`).
- Unit-Tests neben dem Code (`#[cfg(test)] mod tests`).
- Commit-Messages knapp, das „Warum“, ohne Claude-Erwähnung, mit Session-Zeile laut Attribution-Reminder.
- Löschbezogene Zahlen (Auswahl-Chip, Bestätigungsdialog, „reclaimed“) sind immer `freeable`; Anzeige, Sortierung, Filter, Summen folgen dem aktiven `SizeMode`.

## Review Focus

1. `--follow-symlinks`: dieselbe Inode über Symlink zweimal erreicht → nur einmal zählen (Task 1 Test `follow_symlinks_counts_inode_once`).
2. Volume ohne Clone-Tracking / `getattrlist`-Fehler (exFAT, Netzlaufwerk) → kein Panic, Datei gilt als exklusiv (Task 1 Test `missing_clone_info_is_exclusive`).
3. `.modules.yaml` fehlt, ist kaputt oder `storeDir` relativ → kein Store bzw. relativ zum `node_modules` aufgelöst, kein Absturz (Task 4 Tests `parse_store_dir_*`).
4. pnpm nicht installiert / Prune schlägt fehl → Warnung inkl. manuellem Befehl, Store-Zeile bleibt, andere Löschungen laufen durch (Task 5 Test `prune_failure_keeps_row_and_warns`).
5. Derselbe Store über verschiedene Pfadschreibweisen (Symlink, Trailing Slash) → genau eine Zeile (Task 4 Test `stores_are_deduplicated_by_canonical_path`).

---

### Task 1: `usage`-Modul – Datei-Infos und Ordner-Akkumulator

**Files:**
- Create: `src/usage.rs`
- Modify: `src/main.rs` (`mod usage;`), `Cargo.toml` (libc für unix)

**Interfaces:**
- Produces:
  - `pub struct CloneInfo { pub id: u64, pub refcnt: u32 }`
  - `pub struct FileInfo { pub allocated: u64, pub dev: u64, pub ino: u64, pub nlink: u64, pub clone: Option<CloneInfo> }`
  - `pub fn file_info(path: &Path, meta: &std::fs::Metadata) -> FileInfo` (unix: `blocks()*512`, `dev`, `ino`, `nlink`; macOS zusätzlich `clone_info(path)`; windows: `allocated = len`, `nlink = 1`, `dev = ino = 0`)
  - `#[derive(Default)] pub struct UsageAccumulator` mit `pub fn new(dedupe_all_inodes: bool) -> Self`, `pub fn add(&mut self, f: FileInfo)`, `pub fn finish(self) -> Usage`
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)] pub struct Usage { pub allocated: u64, pub freeable: u64 }`
  - `pub fn has_local_snapshots() -> bool` (macOS: `tmutil listlocalsnapshotdates /` liefert ≥1 Datumszeile; sonst `false`)

Regeln für `add`/`finish` (bestimmen die Tests nicht allein, daher hier fest):
- `nlink > 1` (oder `dedupe_all_inodes`): Key `(dev, ino)`; erstes Auftreten zählt `allocated`, weitere nicht. In `finish` ist die Inode „ganz drin“, wenn gesehene Links ≥ `nlink`; nur dann geht sie in die Clone-Regel unten, sonst trägt sie 0 zu `freeable` bei.
- Clone-Regel je (ganz enthaltener) Inode: `clone` `None` oder `refcnt ≤ 1` → `freeable += allocated`. Sonst Familie `id`: Mitglieder zählen, `refcnt` merken, `alloc = max`. In `finish`: Mitglieder ≥ `refcnt` → `freeable += alloc` (einmal pro Familie).
- `allocated` zählt jede Inode einmal (Clones jeweils voll, wie `du`).

- [ ] **Step 1: Tests schreiben** (in `src/usage.rs`)

```rust
fn f(alloc: u64, ino: u64, nlink: u64, clone: Option<(u64, u32)>) -> FileInfo { /* baut FileInfo, dev = 1 */ }

#[test] fn exclusive_files_are_fully_freeable() // 2× f(4096, 1|2, 1, None) → Usage { allocated: 8192, freeable: 8192 }
#[test] fn hardlink_inside_dir_counts_once_and_is_freeable() // f(4096,7,2,None) zweimal → { 4096, 4096 }
#[test] fn hardlink_to_outside_is_not_freeable() // f(4096,7,2,None) einmal → { 4096, 0 }
#[test] fn clone_shared_with_outside_is_not_freeable() // f(4096,1,1,Some((99,2))) → { 4096, 0 }
#[test] fn clone_family_fully_inside_counts_once() // f(4096,1,1,Some((99,2))) + f(4096,2,1,Some((99,2))) → { 8192, 4096 }
#[test] fn missing_clone_info_is_exclusive() // clone None, nlink 1 → voll freeable
#[test] fn follow_symlinks_counts_inode_once() // new(true): f(4096,5,1,None) zweimal → { 4096, 4096 }
#[cfg(unix)] #[test] fn file_info_reads_real_hardlinks() // tmp: Datei + std::fs::hard_link; beide file_info → nlink == 2, gleiche ino
#[cfg(target_os = "macos")] #[test] fn file_info_reads_apfs_clones() // tmp: Datei (≥64 KiB), `cp -c` Kopie; beide clone.is_some(), gleiche id, refcnt == 2 (Test überspringt, wenn clone None – z. B. kein APFS-TMPDIR)
```

- [ ] **Step 2:** `mise run test` → FAIL (Modul/Typen fehlen).
- [ ] **Step 3:** Implementieren. macOS-`clone_info(path) -> Option<CloneInfo>`: `libc::getattrlist` mit `attrlist { bitmapcount: ATTR_BIT_MAP_COUNT, commonattr: ATTR_CMN_RETURNED_ATTRS, forkattr: ATTR_CMNEXT_CLONEID | ATTR_CMNEXT_CLONE_REFCNT, .. }`, Optionen `FSOPT_ATTR_CMN_EXTENDED | FSOPT_NOFOLLOW`. Puffer-Layout (packed): `u32 len`, `attribute_set_t returned`, `u64 cloneid`, `u32 refcnt`. Nur wenn `returned.forkattr` beide Bits enthält → `Some`. `Cargo.toml`: `[target.'cfg(unix)'.dependencies] libc = "<aktuelle Version via cargo add>"`.
- [ ] **Step 4:** `mise run test` → PASS; `mise run lint` sauber.
- [ ] **Step 5: Commit** „Measure allocated and clone-aware freeable bytes per file“.

### Task 2: Scanner trägt `allocated`/`freeable`; `SizeMode`; Batch-Ausgaben

**Files:**
- Modify: `src/scanner.rs`, `src/state.rs`, `src/report.rs`, `src/cli.rs`, `src/main.rs`, `src/tui.rs` (nur mechanisch kompilierbar halten)

**Interfaces:**
- Consumes: `usage::{file_info, UsageAccumulator, Usage}`
- Produces:
  - `state::SizeMode { #[default] Freeable, Allocated }` mit `Serialize, Deserialize, clap::ValueEnum`, `toggle()`, `label()` → `"freeable"` / `"on disk"`, `column_title()` → `"Freeable"` / `"On disk"`
  - `scanner::DirKind { Dependency, PnpmStore }` (serde `snake_case`)
  - `DependencyDir` Felder: `path, ecosystem, name, kind: DirKind, allocated_bytes: u64, freeable_bytes: u64, file_count, modified` (ersetzt `size_bytes`)
  - `impl DependencyDir { pub fn bytes(&self, mode: SizeMode) -> u64; pub fn mostly_shared(&self) -> bool /* allocated > 0 && freeable * 2 < allocated */ }`
  - `scanner::sort_by_size(results: &mut [DependencyDir], mode: SizeMode)`
  - `pub fn scan(root, opts) -> Vec<DependencyDir>` sortiert nach `SizeMode::Freeable`
  - `Report` Felder: `total_allocated_bytes`, `total_freeable_bytes` (statt `total_size_bytes`)
  - `report::write_table(w, report, show_summary, mode: SizeMode)`, `print_table(report, show_summary, mode)`
  - CLI: `--size-mode <freeable|allocated>` → `Cli::size_mode: Option<SizeMode>` (None = gespeicherter Wert im TUI, `Freeable` im Batch)

- [ ] **Step 1: Tests**
  - scanner: `finds_dependency_dirs_and_does_not_recurse_into_them` anpassen (Felder), neu `hardlinked_twin_dirs_are_not_freeable` (unix: zwei `node_modules` mit gehardlinkter Datei 64 KiB → jeweils `freeable_bytes == 0`, `allocated_bytes > 0`).
  - state: `size_mode_toggles_and_round_trips` (toggle zweimal = identisch; ViewState mit `size_mode: Allocated` JSON-Roundtrip; fehlendes Feld → `Freeable`).
  - report: JSON-Test erwartet `total_allocated_bytes`, `total_freeable_bytes`, `directories[0].freeable_bytes`, `kind == "dependency"`; CSV-Header exakt `path,ecosystem,name,kind,allocated_bytes,freeable_bytes,file_count,modified_unix,modified_rfc3339`; Tabelle mit `SizeMode::Allocated` zeigt die allocated-Zahl, Summary `"… freeable across N directories (… on disk, M files)"` bei Freeable.
- [ ] **Step 2:** `mise run test` → FAIL.
- [ ] **Step 3:** Implementieren. `directory_stats` nutzt `UsageAccumulator::new(follow_symlinks)` und `file_info`; `main` filtert `--min-size` über `d.bytes(mode)` und sortiert mit `sort_by_size(&mut dirs, mode)` vor `--top`. In `tui.rs` vorerst alle `size_bytes` → `bytes(SizeMode::Freeable)` bzw. Testhelfer setzen beide Felder.
- [ ] **Step 4:** `mise run test`, `mise run lint` → PASS.
- [ ] **Step 5: Commit** „Report freeable and on-disk sizes instead of summed file lengths“.

### Task 3: TUI – Umschalten Freigebbar/Größe, `⇄`, Snapshot-Hinweis

**Files:**
- Modify: `src/tui.rs`, `src/state.rs` (`ViewState.size_mode`), `src/main.rs` (Flag an `tui::run`)

**Interfaces:**
- Consumes: `SizeMode`, `DependencyDir::{bytes, mostly_shared}`, `usage::has_local_snapshots`
- Produces: `tui::run(root, opts, top, min_size, icons, size_mode: Option<SizeMode>, mouse)`; `App.size_mode`, `App.snapshots: bool`; `ClickAction::ToggleSizeMode`

Verhalten: Taste `f` und Footer-Chip `show:freeable|on disk` togglen; persistiert in `ViewState`. Spaltenkopf = `column_title()`. Sortierung, `s`-Filter, `--min-size`, Balken, Gruppen- und Gesamtsumme folgen dem Modus. Auswahl-Chip, Bestätigungsdialog (Liste + „Frees …“), Fortschritt und „reclaimed“ nutzen `freeable_bytes`. Größenzelle hängt ` ⇄` an, wenn `mostly_shared()`; Footer-Legende `⇄ mostly shared — deleting frees little`, sobald eine sichtbare Zeile markiert ist. Wenn `snapshots`: Footer-Hinweis `Time Machine snapshots: space may only free up once they expire`. Hilfe-Overlay + Hint-Zeile um `f` ergänzen.

- [ ] **Step 1: Tests**
  - `size_mode_toggle_changes_sort_and_totals`: zwei Zeilen A(allocated 100, freeable 90), B(allocated 200, freeable 10); Default-Sortierung → A vor B; nach `handle_key('f')` → B vor A; `view_state().size_mode == Allocated`.
  - `clicking_the_size_mode_chip_toggles_it` (Muster wie Sort-Chip).
  - `deletion_numbers_use_freeable`: `selected_total()` mit A markiert == `(1, 90)` auch im Allocated-Modus.
  - `mostly_shared_rows_show_marker`: Render 140×24, Zeile mit allocated 1000/freeable 10 → Screen enthält `⇄` und `mostly shared`.
- [ ] **Step 2:** FAIL. **Step 3:** implementieren. **Step 4:** `mise run test`, `mise run lint` PASS.
- [ ] **Step 5: Commit** „Let the TUI switch between freeable and on-disk sizes“.

### Task 4: pnpm-Stores finden und als Zeilen messen

**Files:**
- Create: `src/pnpm.rs`
- Modify: `src/scanner.rs`, `src/state.rs` (`Settings.pnpm_default_store: bool`, Default `true`), `src/cli.rs` (`--no-pnpm-default-store`), `src/main.rs`, `src/tui.rs` (Icon/Label), `src/state.rs` Icons

**Interfaces:**
- Produces:
  - `pnpm::STORE_ECOSYSTEM: &str = "pnpm store"`
  - `pnpm::parse_store_dir(contents: &str, base: &Path) -> Option<PathBuf>` (Zeile mit `storeDir`, Formate `storeDir: /p`, `storeDir: '/p'`, `"storeDir": "/p",`; relative Pfade gegen `base`)
  - `pnpm::store_dir_for(node_modules: &Path) -> Option<PathBuf>` (liest `.modules.yaml`)
  - `pnpm::default_store_dirs() -> Vec<PathBuf>` (Unterordner `v<Zahl>` von: macOS `~/Library/pnpm/store`; Linux `$XDG_DATA_HOME/pnpm/store` sonst `~/.local/share/pnpm/store`; Windows `%LOCALAPPDATA%\pnpm\store`)
  - `ScanOptions { follow_symlinks, pnpm_default_store: bool }` (Default `false`, damit Tests nicht echte Stores sehen)
  - `scanner::collect_stores(referenced: Vec<PathBuf>, matched: &[PathBuf], opts) -> Vec<PathBuf>`: + Default-Dirs falls aktiv; existierende Verzeichnisse; dedupliziert per `canonicalize`; ohne Stores, die in einem gematchten Ordner liegen.
  - Store-Zeilen: `DependencyDir { kind: PnpmStore, ecosystem: STORE_ECOSYSTEM, name: "pnpm store", .. }`, gemessen wie andere Zeilen.
  - `scan` hängt Store-Zeilen an; `scan_streaming` sendet für Stores `Discovered` + `Sized` **erst nach** Abschluss aller Projekt-Größen (zweiter `rayon::scope`).
  - `state::Settings.store_action` kommt erst in Task 5.

- [ ] **Step 1: Tests**
  - `parse_store_dir_yaml_style`, `parse_store_dir_json_style` (`"storeDir": "/Users/x/Library/pnpm/store/v11",` → `/Users/x/Library/pnpm/store/v11`), `parse_store_dir_relative_is_resolved_against_base`, `parse_store_dir_missing_returns_none`.
  - scanner `pnpm_store_referenced_by_project_becomes_a_row`: tmp root mit `proj/node_modules/.modules.yaml` → `storeDir` auf tmp `store/v10` (außerhalb root) mit Datei; `scan` liefert Zeile `kind == PnpmStore`, Pfad = Store.
  - `stores_are_deduplicated_by_canonical_path`: zwei Projekte, eines zeigt via Symlink/Trailing-Slash auf denselben Store → eine Store-Zeile.
  - `store_inside_a_matched_dir_is_not_listed_separately`: Store unter `root/.cache/pnpm/v10` → keine Store-Zeile.
  - `streaming_sizes_stores_after_projects`: Reihenfolge der `Sized`-Events: alle `Dependency` vor `PnpmStore`.
  - state: `settings_default_to_pnpm_default_store_on`.
- [ ] **Step 2:** FAIL. **Step 3:** implementieren; `main` setzt `pnpm_default_store = !args.no_pnpm_default_store && settings().pnpm_default_store`. Icons: emoji `📦`, nerd wie Node. **Step 4:** PASS + lint.
- [ ] **Step 5: Commit** „List pnpm stores so the space they hold becomes visible“.

### Task 5: Store-Aktion prune/löschen

**Files:**
- Modify: `src/pnpm.rs`, `src/state.rs`, `src/tui.rs`

**Interfaces:**
- Consumes: `DirKind::PnpmStore`, `scanner::size_dir(path: PathBuf, kind, ecosystem, name, follow) -> DependencyDir` (in Task 4 als `pub(crate)` für Neu-Messung bereitstellen, falls noch nicht)
- Produces:
  - `state::StoreAction { #[default] Prune, Delete }` mit `toggle()`, `label()` → `"prune"` / `"delete"`; `Settings.store_action`
  - `pnpm::store_major(store: &Path) -> Option<u32>` (`v10` → 10, `v3` → 9, sonst None)
  - `pnpm::prune_command(store: &Path, installed_major: Option<u32>) -> (String, Vec<String>)`: `--store-dir` = Store ohne `vN`-Suffix; Programm `pnpm`, wenn `installed_major == store_major` oder `store_major` None; sonst `npx --yes pnpm@<major>`.
  - `pnpm::prune(store: &Path) -> Result<(), String>` (ermittelt `pnpm --version`, führt aus; Fehlertext enthält den manuellen Befehl)
  - `DeleteOutcome::Pruned { old: DependencyDir, new: DependencyDir }`
  - `ClickAction::ToggleStoreAction`

Verhalten: Worker löscht erst alle `Dependency`-Ziele, danach Stores (Prune oder `remove_dir_all` je `store_action`). Nach Prune wird der Store neu gemessen, Zeile ersetzt, `reclaimed += old.allocated - new.allocated` (saturating). Footer-Chip `store:prune` (grün) / `store:delete` (rot, fett), nur wenn eine Store-Zeile existiert; Taste `p` und Klick togglen; im Bestätigungsdialog steht hinter jeder Store-Zeile `prune` bzw. `DELETE whole store`, `p` togglet dort ebenfalls, Legende zeigt `p prune/delete`. Persistiert global (`save_settings`). Hilfe-Overlay ergänzt.

- [ ] **Step 1: Tests**
  - `prune_command_uses_matching_pnpm`: Store `/s/store/v10`, installed 10 → (`pnpm`, `["store","prune","--store-dir","/s/store"]`); installed 11 → (`npx`, `["--yes","pnpm@10","store","prune","--store-dir","/s/store"]`); Store `/x/custom` → (`pnpm`, `[... "--store-dir","/x/custom"]`).
  - `store_action_toggles_and_defaults_to_prune` (state).
  - `store_chip_only_shows_with_store_rows`: Render ohne Store → kein `store:`; mit Store → `store:prune`; `handle_key('p')` → `store:delete`.
  - `confirm_dialog_names_store_action` (Screen enthält `prune` bzw. `DELETE whole store`).
  - `prune_failure_keeps_row_and_warns`: Store-Zeile markiert, `PATH` leer für den Prozess ist nicht testbar → stattdessen `App::apply_delete_outcome(DeleteOutcome::Failed{..})` für Store: Zeile bleibt, `warnings == 1`.
  - `delete_mode_removes_store_dir` (tmp-Store, Modus Delete, Worker wie in `deletion_removes_dir_and_prunes_row`).
- [ ] **Step 2:** FAIL. **Step 3:** implementieren. **Step 4:** PASS + lint.
- [ ] **Step 5: Commit** „Prune pnpm stores instead of deleting them by default“.

### Task 6: Doku, Messung, CI

**Files:**
- Modify: `README.md`, `CLAUDE.md`

- [ ] **Step 1:** README: Options-Block (`--size-mode`, `--no-pnpm-default-store`), Tastentabelle an den Code angleichen (`S` sort, `s` size filter, neu `f`, `p`), neuer Abschnitt „How sizes are measured“ (freeable vs on disk, Clones/Hardlinks, pnpm-Stores & Prune, Snapshot-Problem: Snapshots erfassen das ganze Volume, nichts lässt sich ausnehmen, Platz ggf. erst nach Ablauf frei, `tmutil addexclusion` betrifft nur Backups), JSON/CSV-Felder. CLAUDE.md: Layout um `usage.rs`, `pnpm.rs`; Scanner-Abschnitt um Messung/Stores.
- [ ] **Step 2:** Messung: `git stash`-frei — Release-Build von `main` in `/tmp`-freiem Scratchpad-Worktree vs. Branch, je `time dda --format json ~/src > /dev/null` 3× (mit `--no-pnpm-default-store` für den Projekt-Anteil). Ergebnis notieren; > ~30 % langsamer im Projekt-Teil → `getattrlistbulk` als Folge-Task.
- [ ] **Step 3:** `mise run ci` → PASS.
- [ ] **Step 4: Commit** „Document how dda measures freeable space“ (+ Spec & Plan).
