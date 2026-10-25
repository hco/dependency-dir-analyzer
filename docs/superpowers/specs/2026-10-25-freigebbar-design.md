# Design: „Freigebbar“ statt naiver Größe, pnpm-Stores mit Prune

Stand: 2026-10-25

## Warum

dda summiert heute pro Dependency-Ordner `metadata().len()` aller Dateien
(`scanner::directory_stats`). Bei pnpm ist das irreführend: pnpm legt Pakete
aus einem zentralen Store in `node_modules` ab — auf macOS als APFS-Clones
(copy-on-write, `packageImportMethod=auto` probiert dort zuerst `clone`), auf
Linux als Hardlinks. Die Blöcke sind dann zwischen Store und Projekt geteilt.
dda zeigt z. B. `thymian/node_modules` mit ~1,2 GB an, löschen gibt aber nur
~77 MB frei, weil der Rest weiterhin vom Store belegt ist. Andere Aufräum-Tools
haben denselben Bug (Mole #1698: „168 GB potenziell“, real 1,94 GB).

Ziel: dda soll zeigen, **was Löschen tatsächlich freigibt**, und den Platz, der
im pnpm-Store steckt, sichtbar und per Prune freigebbar machen.

## Ziele

1. Standardanzeige ist **freigebbar**: Bytes, die frei werden, wenn man genau
   diesen Ordner löscht.
2. Umschaltbar auf **Größe** = belegter Platz wie `du` (allozierte Blöcke,
   Hardlinks innerhalb des Ordners einmal gezählt).
3. pnpm-Stores erscheinen als eigene Zeilen; ihr „freigebbar“ ist der
   **prunebare** Anteil. Löschen einer Store-Zeile führt standardmäßig
   `pnpm store prune` aus (umschaltbar auf „komplett löschen“, gut sichtbar).
4. Zeilen, die größtenteils geteilt sind, bekommen eine Markierung `⇄`.
5. Das Snapshot-Problem (siehe unten) ist im README dokumentiert.

## Nicht-Ziele

- Lokale Time-Machine-Snapshots löschen oder Ordner aus Snapshots entfernen
  (geht technisch nicht gezielt; ganze Snapshots zu löschen ist zu destruktiv).
- Ordner per `tmutil addexclusion` aus Backups ausschließen (eigenes Thema,
  bewusst nicht vorgemerkt).
- Reflink-Erkennung auf Linux (btrfs/XFS via `FIEMAP`) und ReFS auf Windows.
- Gemeinsame Berechnung über eine Auswahl hinweg (zwei Ordner, die nur
  untereinander teilen). Auswahlsumme = Summe der Zeilen (konservativ).

## Anti-Ziele (wäre ein Fehlschlag, auch wenn es „funktioniert“)

- „Freigebbar“ zeigt wegen Snapshots fast überall ~0 und wird dadurch nutzlos.
- dda löscht Store-Dateien selbst am pnpm-Index vorbei und hinterlässt einen
  inkonsistenten Store.
- Der Scan wird spürbar langsamer, ohne dass das gemessen wurde.

## Begriffe und Messung

### Snapshots werden ignoriert (entschieden)

Lokale APFS-Snapshots (Time Machine, ~stündlich, ~24 h Lebensdauer) halten
Blöcke gelöschter Dateien fest. `ATTR_CMNEXT_PRIVATESIZE` zählt Snapshot-Blöcke
als „geteilt“ und ist auf dem Rechner des Users praktisch überall 0 — daher
**nicht** verwenden. „Freigebbar“ meint: frei, sobald etwaige Snapshots
verfallen bzw. macOS sie bei Platzmangel wegräumt.

Wenn auf macOS lokale Snapshots existieren, zeigt die TUI im Footer einen
kurzen Hinweis (z. B. „Time-Machine-Snapshots: Platz wird ggf. erst nach deren
Ablauf frei“). Erkennung z. B. einmalig per `tmutil listlocalsnapshotdates /`
beim Start; schlägt das fehl, kein Hinweis.

### macOS (APFS): Clone-Familien

Pro Datei per `getattrlist` (mit `FSOPT_ATTR_CMN_EXTENDED | FSOPT_NOFOLLOW`)
lesen: `ATTR_CMNEXT_CLONEID`, `ATTR_CMNEXT_CLONE_REFCNT`, dazu allozierte Größe
und Linkzahl. Per Spike verifiziert (macOS 27, APFS-Datenvolume):

- Datei ohne Clones: `refcnt = 1`.
- pnpm-Datei: `refcnt = n` (Store + jedes Projekt, das sie nutzt), alle
  Mitglieder tragen dieselbe `cloneid`.
- `PRIVATESIZE` ist wegen Snapshots durchgehend 0 (s. o.).

Regeln pro gescanntem Ordner:

- `refcnt ≤ 1` und `nlink = 1` → exklusiv, voll freigebbar.
- `refcnt > 1` → Datei gehört zur Familie `cloneid`. Die Familie ist nur dann
  freigebbar, wenn **alle** `refcnt` Mitglieder innerhalb dieses Ordners liegen;
  ihre Blöcke zählen dann einmal. Sonst trägt sie 0 zu „freigebbar“ bei.
- Liefert das Volume `CLONE_REFCNT`/`CLONEID` nicht (per
  `ATTR_CMN_RETURNED_ATTRS` prüfen, z. B. Nicht-APFS-Volume), fällt die Datei
  auf die Hardlink-Regel zurück.

Vergleichsmessung auf dem Rechner des Users (allozierte Bytes):

| Ordner | belegt | freigebbar |
|---|---|---|
| `~/src/thymian/node_modules` | 1.217 MB | ~77 MB |
| `~/src/wombat/node_modules` | 721 MB | ~1 MB |
| `~/src/domain-email-extension/node_modules` (Store geleert) | 116 MB | ~112 MB |
| Store `~/Library/pnpm/store/v11` | 8.469 MB | ≈ 2.700 MB |
| Store `~/Library/Caches/mise/timjami-pnpm-store` | 2.796 MB | ≈ 1.200 MB |

### Hardlinks (alle Unix-Plattformen)

Dateien mit `nlink > 1` werden über `(dev, ino)` identifiziert und pro Ordner
einmal gezählt (für „Größe“ und „freigebbar“). Freigebbar nur, wenn alle `nlink`
Links innerhalb des Ordners gesehen wurden. Das deckt pnpm auf Linux ab.

### Windows

Keine Erkennung (stabiles `std` liefert dort weder Blöcke noch Linkzahl).
„Größe“ = `len()`, „freigebbar“ = „Größe“. Store-Zeilen erscheinen trotzdem.

### „Größe“

Allozierte Bytes (`st_blocks * 512`), jede Inode einmal pro Ordner — wie `du`.
Ersetzt die bisherige logische Summe. Damit gilt immer freigebbar ≤ Größe.

## pnpm-Stores

- **Erkennung, Weg 1 – genutzte Stores:** Wird ein `node_modules` gefunden,
  liest dda dessen `.modules.yaml` und daraus `storeDir`. Die Datei kommt in
  zwei Formaten vor (YAML-artig `storeDir: /pfad` und JSON
  `"storeDir": "/pfad"`); ein einfacher Zeilen-Parser genügt, keine neue
  YAML-Abhängigkeit. Der User hat mehrere Stores (`~/Library/pnpm/store/v10`,
  `…/v11`, `~/Library/Caches/mise/timjami-pnpm-store/v11`).
- **Erkennung, Weg 2 – pnpm-Standard-Ort (entschieden):** zusätzlich werden
  immer die Versions-Unterordner (`v3`, `v10`, `v11`, …) am pnpm-Standard-Ort
  gelistet, auch wenn kein gescanntes Projekt darauf zeigt — damit tauchen
  verwaiste Stores auf, die am meisten prunebar sind, und eine Store-Zeile
  verschwindet nicht, nur weil das letzte Projekt gelöscht wurde. Standard-Ort:
  macOS `~/Library/pnpm/store`, Linux `$XDG_DATA_HOME/pnpm/store` bzw.
  `~/.local/share/pnpm/store`, Windows `%LOCALAPPDATA%\pnpm\store`.
  Projekteigene Stores (z. B. der mise-Store) findet nur Weg 1.
- **Abschaltbar (entschieden):** Weg 2 lässt sich abschalten. Meine
  Entscheidung zur Form: CLI-Flag (z. B. `--no-default-store`) plus globale
  Einstellung in `state::Settings` wie `mouse`; das Flag überschreibt die
  Einstellung.
- **Darstellung:** Jeder gefundene Store wird einmal als eigene Zeile mit
  eigenem Ökosystem-Label (z. B. „pnpm store“) gelistet, **auch wenn er
  außerhalb des Scan-Roots liegt**. Er wird wie jede andere Zeile gemessen;
  sein „freigebbar“ ist damit genau der Anteil, den nur noch der Store hält —
  der prunebare Anteil. Der Wert ist eine Schätzung von dem, was
  `pnpm store prune` löscht (pnpm entscheidet über seine Projekt-Registry, nicht
  über Clone-Zähler).
- **Kreislauf:** Löscht man pnpm-`node_modules`, steigt beim nächsten Scan der
  prunebare Anteil des Stores.
- **Store-Aktion (entschieden):** Beim Löschen einer Store-Zeile gibt es zwei
  Modi, umschaltbar:
  - **prune** (Default): dda führt `pnpm store prune` für diesen Store aus;
    nur ungenutzte Pakete verschwinden.
  - **komplett löschen**: der ganze Store-Ordner wird gelöscht wie jede andere
    Zeile. Unbedenklich für bestehende Projekte (Clones/Hardlinks sind
    eigenständige Dateien), kostet aber Re-Downloads bei der nächsten
    Installation. Da der Ordner als Ganzes weggeht, bleibt nichts
    Inkonsistentes zurück (kein Konflikt mit dem Anti-Ziel).

  Danach wird die Store-Zeile neu gemessen (bzw. verschwindet).
- **Prominent sichtbar (entschieden):** Der aktive Store-Modus muss in der
  TUI deutlich erkennbar sein, nicht nur in der Hilfe. Meine Entscheidung zur
  Form: ein eigener Footer-Chip (z. B. `store: prune` / `store: delete`),
  klickbar und per Taste umschaltbar, sobald mindestens eine Store-Zeile
  existiert; der Bestätigungsdialog nennt pro Store-Zeile ausdrücklich, ob
  geprunt oder gelöscht wird, und lässt dort ebenfalls umschalten. Der Modus
  wird global in `state::Settings` gespeichert.

## UX

- **Spalte:** zeigt je nach Modus „freigebbar“ (Default) oder „Größe“. Der
  Spaltenkopf und der Footer benennen den aktiven Modus.
- **Umschalten:** TUI-Taste (Vorschlag `f`), pro Scan-Root persistiert in
  `ViewState` wie `size_unit`; in Hilfe-Overlay, README-Tastenliste und
  (wo es klickbare Chips gibt) Maus-Bedienung ergänzen.
- **Alles folgt dem aktiven Modus:** Sortierung nach Größe, Größenfilter `s`,
  `--min-size`, `--top`, Gruppensummen, Gesamt-/Auswahlsumme, Lösch-Dialog und
  „reclaimed“-Anzeige.
- **CLI-Flag** für die Tabellen-Ausgabe, `--min-size` und `--top` (Name z. B.
  `--size-mode freeable|allocated`, Default `freeable`).
- **JSON/CSV:** enthalten immer beide Werte (unabhängig vom Flag).
- **`⇄`-Markierung:** an Zeilen, deren geteilter Anteil groß ist; kurze
  Legende im Footer, wenn mindestens eine Zeile markiert ist.

## README

Neuer Abschnitt, wie dda Größen misst: freigebbar vs. Größe, pnpm-Clones/
Hardlinks, Stores und Prune, und das Snapshot-Problem (Snapshots erfassen immer
das ganze Volume, einzelne Ordner lassen sich nicht ausnehmen, Platz wird ggf.
erst nach Ablauf frei; `tmutil addexclusion` betrifft nur Backups). Zusätzlich
neue Taste, neues Flag und geänderte JSON/CSV-Felder dokumentieren.

## Constraints

- `cargo fmt`, `clippy -D warnings`, `cargo test` bleiben grün (`mise run ci`).
- Abhängigkeiten minimal, pure Rust: `libc` (bereits transitiv im Lockfile)
  als direkte Abhängigkeit nur für Unix-Targets. `libc` bringt `getattrlist`
  und die `ATTR_CMNEXT_*`-Konstanten mit; `ATTR_CMNEXT_CLONE_REFCNT`
  (`0x00001000`) fehlt dort und wird selbst definiert.
- Cross-Compilation (Linux x86_64/aarch64, macOS, Windows) muss weiter gehen;
  plattformspezifischer Code hinter `cfg`.
- Unit-Tests neben dem Code; die APFS-Pfade lassen sich in Tests auf macOS mit
  `clonefile`/`cp -c` erzeugen.

## Entscheidungen nach dem Review (meine Entscheidungen, bitte prüfen)

- **Löschen ist endgültig:** Seit `main` (a71fe53) gibt es keinen
  Papierkorb mehr. Gelöschter Platz ist also sofort frei (abgesehen von
  Snapshots), und „reclaimed“ stimmt mit „freigebbar“ überein.
- **Store innerhalb einer anderen Zeile** (z. B. ein Store unter einem
  `.cache`-Treffer): wird nicht zusätzlich als eigene Zeile gelistet, sondern
  zählt zu dieser Zeile. Wird sie gelöscht, geht der Store als Ganzes weg —
  unbedenklich, siehe „komplett löschen“.
- **Store-Zeilen in allen Ausgaben:** Store-Zeilen sind normale Zeilen auch in
  Tabelle, JSON und CSV und zählen in die Summen. JSON/CSV kennzeichnen sie
  (z. B. eigenes Ökosystem-Label oder Feld `kind`).
- **Reihenfolge im Streaming-Scan:** Stores werden nach den Projekt-Zeilen
  gemessen, damit die eigentlichen Treffer nicht auf einen 8-GB-Store warten.
- **Laufzeitbudget:** Vorher/nachher auf `~/src` messen, getrennt nach
  Projekt-Zeilen und Store-Zeilen. Wird der Projekt-Teil mehr als ~30 %
  langsamer, `getattrlistbulk` statt Einzel-`getattrlist` verwenden.
- **Gemischte Auswahl** (Projekte + Store): erst Ordner löschen, danach den
  Store prunen, damit Prune die frei gewordenen Pakete schon erfasst.

## Offen für den Builder

- Konkreter Tastenname, Flag-Name, Feldnamen in JSON/CSV (Vorschläge oben).
- Schwelle für `⇄` (Vorschlag: geteilter Anteil > 50 % der Größe).
- Ob pro Datei `getattrlist` zusätzlich zu `lstat` läuft oder ihn ersetzt
  (oder `getattrlistbulk` pro Verzeichnis). Laufzeit vorher/nachher auf einem
  großen `node_modules` messen.
- Wie Store-Zeilen in den Streaming-Scan eingespeist werden (`ScanEvent`).
- Wie `pnpm store prune` aufgerufen wird; Fehler (pnpm nicht gefunden,
  Versionskonflikt) als Warnung anzeigen, inkl. des Befehls zum manuellen
  Ausführen.
