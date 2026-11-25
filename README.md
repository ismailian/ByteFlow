# ByteFlow Desktop

Rust + egui port of the ByteFlow mobile app: a single-user file vault for one
Cloudflare R2 bucket, with end-to-end file encryption. Built to `docs/SPEC.md`.

## Build & run

Builds are **release-only and versioned**. `crates/bf-core/build.rs` refuses
the debug profile, so `cargo build`, `cargo run`, `cargo test` and
`cargo check` without `--release` stop with an explanation instead of
filling `target/debug`.

```sh
scripts\build.ps1 -Bump patch          # bump the version, build, write dist\v<version>\
scripts\build.ps1 -Test                # same version, run the release test suite first
scripts\build.ps1 -Bump minor -Installer   # build, then package with cargo-packager (NSIS)
scripts\installer.ps1                  # package an existing dist\v<version>\ only
scripts\make-icon.ps1                  # redraw the app + installer icon (crates\byteflow\assets\icon\)
cargo rr                                # cargo run --release -p byteflow
cargo rb                                # cargo build --release
cargo rt                                # cargo test --release --workspace (65 tests)
cargo test --release -p bf-data -- --ignored           # + live TLS/SigV4 probe (network)
BYTEFLOW_TEST_MEDIA=clip.mp4 cargo test --release -p byteflow -- --ignored media
cargo run --release -p bf-cli -- -e file.txt           # standalone encrypt/decrypt CLI
```

`scripts/build.ps1` writes `dist/v<version>/byteflow-<version>-windows-x64.exe`,
`byteflow-crypto-<version>-windows-x64.exe`, `SHA256SUMS.txt` and
`BUILDINFO.txt`, and refuses to overwrite a version that was already built:
bump it (`-Bump patch|minor|major`) for every new build. The version also
shows in the sidebar and in Settings → About. `.vscode/settings.json` makes
rust-analyzer check with `--release` too.

**Installer.** `-Installer` (or `scripts/installer.ps1` on its own) runs
`cargo packager` (NSIS; `cargo install cargo-packager --locked`, it fetches NSIS
itself) with the config in `crates/byteflow/Cargo.toml`
(`[package.metadata.packager]`) and the template `installer/nsis/installer.nsi`,
and writes `dist/v<version>/ByteFlow-Setup-<version>-windows-x64.exe`, its line
in `SHA256SUMS.txt`, and `latest.json` (the update manifest). It installs per
user (no admin prompt) into `%LOCALAPPDATA%\Programs\ByteFlow` — never the
vault folder — adds a Start menu entry (desktop icon optional), closes a
running ByteFlow, and upgrades in place. It takes over an install made by the
old Inno Setup installer (0.4.0 and earlier): same folder, the old uninstaller
and its Add/Remove Programs entry removed. Uninstalling removes the program
only: the vault in `%LOCALAPPDATA%\ByteFlow` stays, and Settings → Danger zone
erases it. Silent flags: `/S` (silent), `/D=<folder>` (install folder), `/NS`
(no shortcuts), `/R` (start ByteFlow when done). The executables are not
code-signed, so SmartScreen warns on first run.

**Updates.** At launch the app reads
`https://byteflow-app.pages.dev/download/latest.json` (`{version, url, sha256,
size}`). When it names a newer version it offers **Download** / **Close**.
Download fetches the setup (only from that site, checked against the SHA-256),
then ByteFlow closes, a hidden helper runs the setup silently, starts the new
version and deletes the installer. To publish a release, copy the setup and
`latest.json` from `dist/v<version>/` into `site/download/` and deploy the
site. Offline or a missing manifest: nothing is shown.

**Window.** The first launch opens centred at 70 % × 80 % of the screen; later
launches reopen at the last position and size (normal state only, and only if
that spot is still on a connected monitor). Stored in
`%LOCALAPPDATA%\ByteFlow\window.txt`.

Build profile: `debug = "line-tables-only"` and no debug info for
dependencies or build scripts (only relevant if the guard is lifted),
`doctest = false`, `rust-lld` on Windows MSVC, release with thin LTO and
`panic = "abort"`. A panic shows an error dialog before the process ends.
The UI uses eframe's `glow` backend instead of `wgpu`: far fewer crates, a
smaller binary.

UI-development conveniences exist only with the `dev-env` cargo feature
(`cargo build --release -p byteflow --features dev-env`). The build script
never enables it, and the window title says `(dev-env)` when it is on:

| Variable | Effect |
|---|---|
| `BYTEFLOW_DEV_DATA_DIR=<dir>` | Use a throwaway vault directory instead of `%LOCALAPPDATA%\ByteFlow` |
| `BYTEFLOW_DEV_ALLOW_CAPTURE=1` | Don't exclude the window from screen capture (for UI screenshots) |
| `BYTEFLOW_DEV_VIEW=<local file>` | Open that local file in the viewer after unlock (viewer work without a bucket) |
| `BYTEFLOW_DEV_DEMO=1` | Seed the (throwaway!) vault with a made-up bucket, thumbnails, transfers, sync pairs and links, and unlock it on start. No network. Extras: `BYTEFLOW_DEV_PAGE`, `_PREFIX`, `_SELECT`, `_THEME`, `_GRID`. `scripts/screenshot.ps1` uses it for the site screenshots in `site/assets/` |

## Interface (0.3.0)

- **Type**: Plus Jakarta Sans Regular for text (egui's default was Ubuntu
  Light), Medium/SemiBold/Bold for buttons and headings, Phosphor icons
  instead of emoji. Fonts are bundled (`crates/byteflow/assets/fonts`, OFL
  and MIT).
- **Arabic** (and Persian, Urdu): Noto Sans Arabic is bundled as a fallback
  font (one variable file for the four weights), and egui 0.37's shaper joins
  the letters. egui has no bidirectional layout, so `bidi.rs` reorders
  right-to-left text with the Unicode Bidirectional Algorithm before egui
  lays it out. Names, paths and messages are left-to-right paragraphs like
  the rest of the UI ("تقرير.pdf" keeps `.pdf` on the right, as in
  Explorer, and path segments stay in parent → child order). Text-file lines
  and CSV cells take their direction from their first letter. Wrapping and
  truncation work on the logical text, so a long Arabic name wraps with its
  first words on the first line and is cut at its end. In text boxes the
  text shows in visual order, but the caret follows egui's left-to-right
  glyph order. Hebrew is reordered too but has no bundled font.
- **Theme**: light and dark palettes as tokens (`theme.rs`), 1 px borders,
  8 px controls and 12–16 px cards and modals, visible focus rings, pointer
  cursor on clickable controls.
- **Components** (`ui/widgets.rs`): primary/secondary/ghost/danger buttons
  with hover and pressed states, icon buttons with tooltips, segmented
  controls, underline tabs with counts, badges, labelled inputs, a search
  field, progress bars, callouts, empty states, a shared modal frame (icon,
  title, subtitle, close, Esc and backdrop dismiss) and menu rows.
- **Left sidebar**: brand, bucket chip with the space used (from the local
  listing; R2 has no quota) and the listing status, grouped navigation with
  count badges, a live transfer card, Settings, Lock vault and the version.
  There is no separate Overview page: its content moved to the details panel.
- **Files toolbar**: one row with search and sort on the left and Refresh, New
  folder and Upload on the right (labels drop to icons, and sort to its icon,
  when the window is narrow). Under it, a fixed-height row shows the
  breadcrumbs (a house for the bucket root; middle folders fold into a "…"
  menu), the counts and the view options, and is replaced in place by the
  selection's actions when something is selected, so the listing never
  moves. The actions keep their labels when there is room (the view options
  step aside first) and turn into icons with tooltips when there isn't. A
  narrow list drops the Uploaded column.
- **Right details panel** (Files): photo preview on top, name and type,
  actions, size, upload date, MIME type, location, ETag, and encryption
  status. With several items it summarises the selection. With nothing
  selected it is the **bucket overview**: space used, storage by file type
  (a thin segmented bar plus one row per type with its file count and size;
  hover for the share), uploads over the last 7 days, the largest root
  folders and the latest files (click to open), and the current folder's
  totals below the root. Chart colours are a colour-blind-checked set per
  theme (`theme::series`). Toggle the panel with the button in the toolbar.
- **Built-in viewers** (`ui/viewer.rs`): photos (zoom, pan, fit, 1:1,
  rotate), text and code (line numbers, wrap), CSV/TSV (table or raw),
  **video and audio played in the app** through Windows Media Foundation
  (`platform/media.rs`): play/pause, seek, ±10 s, volume, mute; Space, ←/→,
  ↑/↓ and M shortcuts. Other types offer "Open in default app" and Download;
  nothing is fetched until you choose.

## Architecture

The layering follows spec §2.1. Dependencies point inward only.

```
byteflow (egui UI) ──► bf-app (controllers, engines) ──► bf-data (R2, stores, DBs) ──► bf-core (crypto, failures)
bf-cli ──────────────────────────────────────────────────────────────────────────────► bf-core
```

| Crate | Contents |
|---|---|
| `bf-core` | AES-256-GCM envelope, Argon2id, streaming file crypto v1/v2 (byte-identical with `file_crypto.dart`), failure taxonomy + the one place failures become sentences, logging and redaction |
| `bf-data` | Hand-rolled SigV4 (checked against the AWS vectors), R2 client (reqwest + rustls/ring), S3 XML, DPAPI-protected secure store, credential store, the app DB and the separate listing-cache DB (SQLite), sharded thumbnail store |
| `bf-app` | Vault state machine and wipe, bucket scan, browser and overview, transfer engine, encryption engine, upload pipeline, sync, sharing, thumbnails |
| `byteflow` | egui UI (theme, components, pages, viewers), Media Foundation playback, and platform hardening |
| `bf-cli` | `byteflow-crypto -e/-d [-p pass] [-o out] [-f] <file>`, the twin of `bin/vault_crypto.dart` |

**Concurrency.** The UI thread only draws. A multi-threaded tokio runtime does
all network and disk I/O. CPU-heavy work goes to tokio's blocking pool and
rayon: Argon2id lanes in parallel, file-crypto segments sealed and opened in
parallel batches, image decoding, and directory walks. SQLite runs on the
blocking pool and never touches the UI thread. Services publish snapshots that
the UI reads each frame, and they request a repaint only when something changes.

**Performance work beyond the mobile baseline:**
- Multipart parts upload 3 at a time per task. Mobile uploads them sequentially [PARTIAL].
- Encryption and decryption run in parallel across segments. The output bytes are unchanged.
- The sync walk runs off the async runtime, shows live progress, and can be cancelled.
- File lists are virtualised (`show_rows`), and thumbnail textures are LRU-capped and decoded off the UI thread.

## Owner decisions (spec §17 `[DECISION]`)

The spec says these need your call. I picked the most conservative option in
each case, and each one is easy to change.

| Decision | What this build does | Where |
|---|---|---|
| `KEK_bio` / biometrics (§17.2) | **Password-only.** No desktop backend meets the hardware/OS-enforced requirements. The `KeyManager` trait keeps the channel contract, so a Windows Hello or Secure Enclave backend can be added later. | `bf-data/src/secure/key_manager.rs` |
| Secure storage (§17.1) | One file under `%LOCALAPPDATA%`, DPAPI-protected (current user). On macOS and Linux, a keychain-held key seals the file. | `bf-data/src/secure/protector.rs` |
| Data location / backups | **Local** AppData, not Roaming, so the vault never roams to other machines. | `bf-data/src/paths.rs` |
| Default download folder | The OS Downloads folder | `paths::default_download_dir` |
| Auto-lock trigger | Idle for N seconds (this also covers minimised windows), **plus an immediate lock on OS lock or sleep** | `app.rs`, `platform/oslock.rs` |
| Privacy overlay | Always on minimise. On focus loss only if you opt in (Settings). | `app.rs` |
| Screen capture | `WDA_EXCLUDEFROMCAPTURE` on Windows: no screenshots, recordings, screen shares or taskbar previews | `platform/window.rs` |
| Clipboard | Share links only. Excluded from clipboard history, cloud clipboard and clipboard monitors, and auto-cleared after 60 s (configurable). | `platform/clipboard.rs` |
| Tray residency | None. Closing the app quits, and transfers resume on the next unlock (same as mobile). | — |
| Single instance (§17.3) | Named mutex. A second launch brings the first window to the front. | `platform/instance.rs` |
| Cache wipe exemption (§5.6) | Kept as on mobile. A custom-located cache survives a wipe; the default location does not. Settings explains this. | `wiper.rs` |
| Plaintext-at-rest stores (§17.4) | Accepted as on mobile (object names in the cache, app DB and `encryption_state.json`; thumbnails), with app-private placement and wipe coverage. **Not** sealed under a session key. | — |
| Video/audio preview (0.4.0) | **Streamed in the app** (Media Foundation Media Engine, in-process, OS codecs). The engine reads through an in-process `IMFByteStream` backed by ranged GETs in ~1 MiB blocks with read-ahead, so playback starts after the first blocks and a seek fetches only what it needs. **Encrypted objects stream too**: their 64 KiB segments are authenticated independently, so each range is decrypted in memory. Nothing is written to disk, and no presigned URL is created or handed to the OS (unlike mobile, which gives its player a presigned URL). "Open in default app", and platforms without the built-in player, still use a private temp copy. | `bf-app/src/stream.rs`, `platform/media.rs`, `ui/viewer.rs` |
| Audio playback (0.4.0) | Opening an audio file plays it **in the details panel**, not the full-window viewer. A "Now playing" strip stays while you browse. Playback stops when the panel is hidden, when you leave Files, or on lock. Audio doesn't count as activity for auto-lock (video still does). | `ui/files/audio.rs` |
| Destruction password (0.4.0) | **Optional, off for fresh installs.** Onboarding asks only for the login password, and Settings → Security turns it on, changes it or turns it off (each needs the login password). Installs that already set one keep it. The unlock path always runs both Argon2id checks, so timing doesn't reveal whether one is set. | `auth.rs`, `ui/settings.rs` |
| Multiple buckets (0.4.0) | After unlock you see a **grid of connected buckets**. One bucket is open at a time, and switching closes the open one, whose transfers pause until you reopen it. Each bucket has its own listing cache, transfer/sync/link database, thumbnails, encryption index and **file master keys** (it may have been set up from another device with another passphrase). The first bucket keeps every original path and key name, so earlier installs need no migration. The list is sealed under the DEK (`bucket_list_blob`). Adding, editing or removing a bucket asks for the login password. Removing one erases its local data; the bucket's contents are untouched. A wipe erases all buckets. | `bf-app/src/buckets.rs`, `credential_store.rs`, `ui/home.rs` |
| Auto-lock while watching | A video **playing in the focused, visible window counts as activity**, so the idle lock doesn't stop it. OS lock/sleep still locks at once; minimising still hides the window. The display is kept awake only while a video plays (spec §10.1). Remove the `watching` term in `app.rs` to revert. | `app.rs` |
| Details-panel photo preview | A stored thumbnail shows at once. A plaintext photo up to 24 MB is fetched into memory 350 ms after you select it and seeds its thumbnail (like opening it). Encrypted photos are decrypted only when you click "Load preview", and never thumbnailed. | `ui/files/details.rs` |
| Video thumbnails | Not generated. The type icon is shown instead, which the spec allows as a safe outcome. | `thumbnails.rs` |
| Arabic text direction (0.3.0) | Names, paths and messages are **left-to-right paragraphs** (the UI is English and left-aligned), so a mixed name keeps its extension on the right and paths read parent → child. Text-file lines and CSV cells use the direction of their first letter. Flip `visual` to `visual_auto` for first-letter direction everywhere. | `bidi.rs` |
| Installer (0.5.0) | cargo-packager (NSIS), replacing Inno Setup. **Per-user install** into `%LOCALAPPDATA%\Programs\ByteFlow` (no admin). **Uninstall keeps the vault** (`%LOCALAPPDATA%\ByteFlow`); erasing it stays an explicit action in Settings. Takes over an Inno Setup install. Not code-signed. | `installer/nsis/installer.nsi`, `crates/byteflow/Cargo.toml` |
| UI font (0.5.0, trial) | **Inter** instead of Plus Jakarta Sans, pending approval. One constant switches back. The bundled Inter files have their Private Use Area mappings removed so Phosphor icons aren't shadowed. | `theme.rs` (`UI_FONT`) |

## What's verified

- **All 69 automated tests pass** (`cargo rt`): right-to-left reordering (word order, mixed scripts, brackets, Arabic-Indic digits, ZWNJ, emoji, paths, logical wrapping and truncation); Arabic font coverage; segmented-bar widths; SigV4 against three AWS S3 vectors; key-encoding round trips; envelope and tamper; v1/v2 file crypto round trips; tamper, truncation and reorder all failing identically; hostile-header bounds; Argon2id; DPAPI; the sync truth table; multipart part-size maths; the retry policy; cache key-range queries (`%` in folder names); the thumbnail policy; the sealed bucket list and per-bucket key scoping; random-access segment decryption; streamed reads; and that every font family resolves to a bundled font.
- **Media Foundation player** (ignored tests, run with `BYTEFLOW_TEST_MEDIA` / `BYTEFLOW_TEST_AUDIO`): an H.264/AAC MP4 loads, reports video and audio, delivers frames GPU-scaled to the requested size, pauses, seeks, and releases the file when closed. **Streaming**: an 18 MB MP4 with its index at the end, encrypted with the vault format, served through a throttled source (40 ms per request, 8 MB/s) and decrypted per segment. The first frame arrives at ~0.8 s, against ~2.3 s for the whole object. A seek near the end resumes there, and the player shuts down within 3 s. An MP3 plays and seeks both plain and encrypted.
- **Random access** (`stream.rs`, `file_crypto.rs`): reads at random offsets match the plaintext for plain and encrypted objects. Any run of segments decrypts the same as a full decrypt. Tampered, swapped and truncated segments fail. A read 40 MiB into a 64 MiB object fetches only the nearby blocks.
- **End-to-end vault test** (`bf-app/tests/vault_flow.rs`), with full-strength Argon2id and real DPAPI: onboarding with only a login password, turning the destruction password on (and refusing one equal to the login password, or a wrong login password), the generic error on a wrong password, unlock to the bucket grid, opening a bucket, adding a second one (duplicate refused) with its own stores, then removing it (its folder is erased, the last bucket can't be removed), turning the destruction password off (it is then just a wrong password) and on again, then the destruction password. After the wipe, secure storage is empty, the thumbnails, temp files and encryption state are gone, the Documents folder is untouched, and a leftover tombstone re-runs the wipe on launch.
- **Live network probe**: rustls TLS, SigV4 signing and S3 XML error mapping work against a real S3 endpoint.
- **UI driven by hand (0.3.0, 125 %)** with a seeded listing that mixes Arabic and Latin names: the Files toolbar and context row (root and nested breadcrumbs, selection with labels and as icons, narrow window), list and grid, details for a long Arabic name, the bucket overview in light and dark, Arabic in the text viewer (wrapped right-to-left paragraphs). Installer built with Inno Setup 7.1.1 and round-tripped (silent install and uninstall).
- **UI driven by hand (0.2.0, 1920×1080 at 150 %)**: onboarding, unlock, the Files list and grid with a seeded listing cache, thumbnails and encryption badges, selection bar, details panel (photo, encrypted file, folder summary), context menu, New folder and Delete modals, the no-viewer state, the photo, CSV/text, video and audio viewers with local files, Transfers, Overview, Settings, and dark mode.
- **Upload crash fixed**: the upload dialog's "no more name conflicts" marker (`usize::MAX`) was used as an index before it was checked, which panicked (and, with `panic = "abort"`, closed the app) on every upload without a conflict. It is now an explicit `Ready` stage.

## Not yet verified (needs your bucket)

- I haven't run anything against a real R2 bucket, because there were no credentials. The first run should cover: Test connection; Refresh; upload above and below 8 MiB (pause and resume mid-multipart; the fixed upload dialog end to end); download with Range resume; rename, move and delete for files and folders; set an encryption key, upload with encryption on, preview (photo, text, video, audio, and an encrypted video); the details-panel photo fetch; share a link.
- **Cross-platform byte compatibility** was implemented by mirroring `file_crypto.dart` field by field, but no Dart toolchain was available to round-trip a file. To check: encrypt with `dart run bin/vault_crypto.dart -e` and decrypt with `byteflow-crypto -d`, then the reverse.
- macOS and Linux code paths (the keychain protector, single instance, opening files) compile only on those targets and have not been built here.
