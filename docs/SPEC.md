# ByteFlow — Functional & Security Specification (desktop port baseline)

This document specifies **what ByteFlow does** (features, security model,
architecture, and performance behaviour) so a desktop build (Windows / macOS /
Linux) can be built to parity with the shipped mobile app. It does not cover
UI: no layouts, screens, widgets, motion, or visual design.

It was written from the code on `dev` (app version `1.7.0+12`, Nov 2026), not
only from `CLAUDE.md`. Where the code and `CLAUDE.md` disagree, this document
follows the code and says so.

**Status markers used throughout**

| Marker | Meaning |
|---|---|
| *(no marker)* | Shipped on mobile, behaves as described. |
| **[PARTIAL]** | Shipped, but simpler than `CLAUDE.md` describes. The desktop port should match the shipped behaviour unless the owner decides otherwise. |
| **[NOT BUILT]** | Specified in `CLAUDE.md`, not implemented on mobile. Don't port it as if it already exists. |
| **[DESKTOP]** | A desktop-specific adaptation or constraint. |
| **[DECISION]** | Needs an owner decision before it is built. `CLAUDE.md` §13.3/§13.6 forbid inventing behaviour under security, transfers, or sharing. |

---

## Contents

1. [Product scope](#1-product-scope)
2. [Architecture](#2-architecture)
3. [Security layer](#3-security-layer)
4. [Cloudflare R2 integration](#4-cloudflare-r2-integration)
5. [Bucket-listing cache](#5-bucket-listing-cache)
6. [Browsing & file operations](#6-browsing--file-operations)
7. [Transfer engine](#7-transfer-engine)
8. [Auto-delete after upload](#8-auto-delete-after-upload)
9. [File-content encryption](#9-file-content-encryption)
10. [Media preview & thumbnails](#10-media-preview--thumbnails)
11. [Directory sync](#11-directory-sync)
12. [Sharing links](#12-sharing-links)
13. [Preferences](#13-preferences)
14. [Error model](#14-error-model)
15. [Performance & optimizations](#15-performance--optimizations)
16. [Testing requirements](#16-testing-requirements)
17. [Desktop adaptation](#17-desktop-adaptation)
18. [Known gaps on mobile](#18-known-gaps-on-mobile)
19. [Appendix — inventories & constants](#19-appendix--inventories--constants)

---

## 1. Product scope

### 1.1 What it is

A **single-user** file manager that connects directly to **one Cloudflare R2
bucket** with S3-compatible credentials stored on the device. It browses,
uploads, downloads, renames, moves, and deletes; encrypts object contents
end-to-end; syncs local folders up one way; and mints time-limited presigned
share links.

### 1.2 Non-goals (carry over unchanged)

- No backend, no user accounts, no sync server, no multi-tenancy.
- **No network calls to any host other than the configured R2 endpoint.** No
  analytics, telemetry, or third-party crash reporting.
- No "share with other users", ACL management, or bucket creation.
- No WebView. No deep links or URL handlers that accept credentials.

### 1.3 Threat model

The owner accepts that long-lived R2 credentials live on the device. The
security work protects **credentials and file contents at rest on a lost,
seized, or stolen device**. It does not protect the bucket from the app's own
user.

**In scope**
- Device stolen while locked: credentials can't be recovered without the
  owner's biometric or recovery password.
- Device seized under coercion: the destruction password wipes all key
  material irrecoverably, and the app then looks like a fresh install.
- The app being backgrounded leaks file names or credentials through OS
  snapshots or screen capture.
- Object contents stored in R2 are unreadable to anyone holding only the
  bucket (opt-in file encryption).

**Out of scope**
- A rooted, jailbroken, or admin-compromised machine while the owner is
  actively unlocking the app.
- Malicious code inside the app's own process.

**[DESKTOP]** Desktop OSes give much weaker app isolation than mobile: any
process running as the same user can usually read the app's data directory.
"At rest" therefore has to mean "encrypted under a key the attacker can't
get". Being inside the app's folder is not protection. Every store in §19.1
must be judged that way (see §17).

---

## 2. Architecture

### 2.1 Layers

Feature-first, three layers. Dependencies point inward only:

```
presentation  →  application  →  data
                      ↘            ↙
                         core
```

- **core**: crypto primitives, `Result`/failure types, logging and redaction,
  formatting. Pure Dart; no platform UI.
- **data**: R2 client and SigV4 signer, S3 XML parsing, secure-storage
  stores, the two SQLite databases, the thumbnail store.
- **application**: controllers and engines (auth, browser, cache, encryption,
  thumbnails, transfers, sync, sharing, overview). State is exposed through
  Riverpod providers.
- **presentation** never imports from `data`. It talks only to application
  providers.

### 2.2 Cross-cutting rules

- **No throwing across layer boundaries.** Every fallible operation returns
  `Result<T, AppFailure>` (`core/result.dart`). Platform-channel exceptions,
  dio exceptions, and anything else unexpected are caught at the boundary and
  mapped (§14).
- **CPU-bound work runs off the UI isolate**: Argon2id (the `cryptography`
  package fills memory lanes on worker isolates) and file encryption and
  decryption (`Isolate.run`).
- **Streamed I/O only** for object bodies. A body is never fully buffered,
  except single-PUT uploads at or under 8 MiB and individual multipart parts
  (§15).
- **Secrets are `Uint8List`, never `String`**, and are zeroed with
  `fillRange(0, len, 0)` in `finally` blocks. Bytes returned from a platform
  channel are copied into a fresh modifiable buffer first, because the
  channel's reply buffer may be unmodifiable and then the wipe would throw.
- **Immutable models**: `freezed` + `json_serializable` for DTOs and state.
- **Exact dependency pinning**; any new dependency needs owner approval.

### 2.3 Module map

| Module | Responsibility | Platform-bound? |
|---|---|---|
| `core/crypto/envelope.dart` | AES-256-GCM seal/open with AAD | Pure Dart |
| `core/crypto/password_hasher.dart` | Argon2id derive/verify, constant-time compare | Pure Dart |
| `core/crypto/file_crypto.dart` | Streaming object-body encryption, v1/v2 formats | Pure Dart (`dart:io`) |
| `core/crypto/key_manager.dart` | Hardware KEK wrap/unwrap over `r2vault/keystore` | **Platform channel** |
| `core/media/thumbnailer.dart` | Native thumbnail generation over `byteflow/thumbnails` | **Platform channel** |
| `core/logging.dart` | The only sanctioned log sink; `redact()`, `redactUrl()` | Pure Dart |
| `data/r2/*` | SigV4 signer, R2 client, XML parsing, error mapping | Pure Dart (dio) |
| `data/secure/secure_store.dart` | Abstraction over `flutter_secure_storage` | Plugin |
| `data/secure/credential_store.dart` | Credential blob, DEK wraps, file master key, external-key table | Pure Dart over `SecureStore` + `KeyManager` |
| `data/secure/vault_state_store.dart` | Destruction hash, flags, preferences | Pure Dart over `SecureStore` |
| `data/local/db/app_database.dart` | drift: transfer queue, sync pairs/manifest, shared links | drift/sqlite3 |
| `data/local/db/cache_database.dart` | drift: bucket-listing cache (separate DB) | drift/sqlite3 |
| `data/local/thumbnail_store.dart` | Sharded on-disk thumbnail cache | Pure Dart (`dart:io`) |
| `application/auth/*` | Vault state machine, unlock, onboarding, wipe | Pure Dart |
| `application/cache/*` | Full-scan service, cache location | Pure Dart (+ path_provider) |
| `application/encryption/*` | Encryption controller, engine, state store, session keys | Pure Dart |
| `application/transfers/*` | Transfer engine, retry, download location, source delete | Mostly pure; source delete uses **`byteflow/files` channel** on Android |
| `application/sync/*` | Sync scan + classifier | Pure Dart |
| `application/sharing/*` | Shared-link records | Pure Dart |
| `application/thumbnails/*` | Thumbnail policy + service | Pure Dart over `Thumbnailer` |
| `bin/vault_crypto.dart` | Standalone encrypt/decrypt CLI | Pure Dart (**already runs on desktop**) |

**[DESKTOP]** Everything above except the three platform channels
(`r2vault/keystore`, `byteflow/thumbnails`, `byteflow/files`) is portable
as-is. Recommended approach: add `windows/`, `macos/` (and possibly `linux/`)
targets to **this** Flutter project and implement those channels natively.
Forking a separate codebase would let the crypto formats drift. The v1/v2
object format and the credential envelope **must stay byte-identical**: a file
encrypted on one platform has to decrypt on every other, and in the CLI.

### 2.4 Top-level state machine

`AuthController` (Riverpod `Notifier<VaultState>`) drives the whole app:

```
VaultLoading ──(wipe_pending?)──► resume wipe ──► VaultNeedsPasswords
     │
     ├─ credentials configured ──► VaultLocked ◄──────────────┐
     │                                │  bio / password       │ lock / auto-lock
     │                                ▼                       │
     │                           VaultUnlocked(config) ───────┘
     │                                │  destruction pwd / wipe-everything / tamper-wipe
     │                                ▼
     │                           VaultWiping ──► VaultNeedsPasswords
     │
     └─ not configured ──► VaultNeedsPasswords ──► VaultNeedsConfig ──► VaultUnlocked
```

- `VaultLocked.error` carries one generic, non-revealing message.
- Every session-scoped provider (R2 client, transfer engine, encryption
  engine, cache service) derives from `unlockedConfigProvider`. Reading one
  while locked is a `StateError`. Locking tears the session graph down.

### 2.5 Persistent stores

Full inventory with wipe behaviour in §19.1. In summary:

| Store | Holds | Location (mobile) | Wiped by destruction? |
|---|---|---|---|
| Secure storage | Credential blob, DEK wraps, destruction hash, file master key, external keys, prefs | Android Keystore-encrypted prefs / iOS Keychain (`first_unlock_this_device`) | Yes (overwrite then delete) |
| Hardware key | `KEK_bio` | Android Keystore (StrongBox if available) / iOS Secure Enclave | Yes (step 1) |
| `r2vault.sqlite` (+WAL/SHM) | Transfer queue, sync pairs and manifest, shared-link records | App-support dir | Yes |
| Cache DB | Object listing mirror | App-support dir **or a user-chosen external folder** | **No** (deliberate, §5.6) |
| `encryption_state.json` | Encrypted-key index + encryption queue | App-support dir | Yes |
| `thumbs/` | Thumbnails | App-support dir | Yes; must **never** move out of it |
| Temp dirs | `enc_work`, `enc_preview`, `enc_upload`, picker copies | OS temp | Yes |
| Downloads | Downloaded files | App-private documents dir (default) or a chosen folder | Private dir yes (mobile only); chosen folder no |

---

## 3. Security layer

This section is authoritative. Desktop must keep every invariant here, or
stop and raise the trade-off with the owner (§17).

### 3.1 Key hierarchy

```
Credential blob = JSON { accountId, accessKeyId, secretAccessKey, bucket, publicUrl?, endpoint? }
      │  AES-256-GCM, random 96-bit nonce, AAD = "r2vault.v1.credentials"
      ▼
DEK — 32 random bytes (CSPRNG), generated once at setup
      │
      ├─ wrapped by KEK_bio  — hardware key, biometric-gated          → dek_wrap_bio
      └─ wrapped by KEK_pwd  — Argon2id(login pwd, 16-byte salt,       → dek_wrap_pwd (+ dek_wrap_pwd_salt)
                               64 MiB / t=3 / p=4, 32-byte out)
                               AES-GCM, AAD = "r2vault.v1.dek.pwd"

File master key (opt-in, §9) — 32 bytes = Argon2id(passphrase, salt)
      │  AES-256-GCM under the DEK, AAD = "r2vault.v1.file.master"
      ▼
enc_master_blob (active) / enc_master_pending (rotation), plus non-secret KDF salt/params

External object keys (§9.9) — per KDF-salt master keys
      │  AES-256-GCM under the FILE MASTER KEY, AAD = "r2vault.v1.file.external:<saltId>"
      ▼
enc_external_keys (one JSON map)
```

- **Envelope layout**: `nonce(12) ‖ ciphertext ‖ tag(16)`. Every AAD is
  distinct, so no blob can be swapped into another blob's slot.
- A wrong key, a tampered ciphertext, and a wrong AAD all fail identically
  as `CryptoFailure` (no oracle).
- **Why the password wrap exists** even though biometrics are the primary
  path: biometric-bound hardware keys are permanently invalidated by
  fingerprint/face re-enrollment. Without a second wrap, the owner would lose
  everything on re-enrollment. If the owner ever rejects the password wrap,
  the data-loss consequence must be stated in onboarding. It is never dropped
  silently.

### 3.2 Setup (onboarding)

1. **Passwords**: a login/recovery password and a destruction password. Each
   has ≥ 10 characters and is entered twice; the two must differ. A strength
   estimate is shown (a length + character-class heuristic, 0–4; not zxcvbn
   **[PARTIAL]**).
   - The destruction hash is written immediately (§3.5).
   - The login password is held **in memory only** (as bytes) until the
     config is saved, then wiped. It is wiped on dispose too.
2. **R2 config**: Account ID, Access Key ID, Secret Access Key, Bucket, plus
   optional Public URL and optional custom endpoint (defaults to
   `https://<accountId>.r2.cloudflarestorage.com`). "Test connection" runs
   `ListObjectsV2 max-keys=1` and reports a targeted diagnostic (§14.3).
3. **Save** (`CredentialStore.setup`), in an order chosen for crash safety:
   1. Generate the DEK, seal the config.
   2. Write the **password wrap first**, so a later biometric failure can
      never leave the vault with no way in.
   3. Write the biometric wrap (this prompts for biometrics on Android).
   4. Write the **blob last**. `isConfigured` only becomes true once a wrap
      exists, so an interrupted setup falls back cleanly to onboarding
      instead of leaving a half-configured vault that can't be unlocked.
   5. Wipe the DEK.
- `isConfigured` = the blob exists **and** at least one wrap exists.

### 3.3 Hardware KEK (`KEK_bio`)

The idea behind "use the fingerprint as the key": a fingerprint yields no
stable key. Instead, a random key is generated **inside secure hardware**,
marked as requiring biometric auth, and the OS performs the crypto only after
a successful prompt. The key never leaves the hardware.

**Channel contract** `r2vault/keystore` (methods take `{data: bytes}`):

| Method | Behaviour |
|---|---|
| `wrap` | Encrypt the DEK with the hardware key (creating the key if absent). May prompt. |
| `unwrap` | Decrypt a wrap blob. **Always prompts.** |
| `deleteKey` | Delete the alias. Idempotent; a missing alias is not an error. |
| `isAvailable` | Strong biometrics usable right now. |

Error codes map to `BiometricFailureReason`: `permanently_invalidated`,
`user_canceled`, `not_available`, `not_enrolled`, `locked_out`,
`locked_out_permanent`, anything else → `unknown`. A
`MissingPluginException` (channel not registered) → `notAvailable`.
**Nothing may escape as a raw exception.** An escaping throw once hung the
unlock screen.

**Android reference**: an AES-256-GCM key in `AndroidKeyStore`, alias
`r2vault_kek_bio`, `setUserAuthenticationRequired(true)`,
`AUTH_BIOMETRIC_STRONG` with timeout 0 (API 30+; validity -1 before that),
`setInvalidatedByBiometricEnrollment(true)`, StrongBox with fallback.
`BiometricPrompt` + `CryptoObject(cipher)`, and **only the cipher returned
through the CryptoObject** does the crypto; otherwise the binding is
decorative. Blob = `iv(12) ‖ ct+tag`.

**iOS reference**: a Secure Enclave P-256 key, tag `r2vault.kek.bio`,
`SecAccessControl(.biometryCurrentSet, .privateKeyUsage)`, ECIES
`eciesEncryptionCofactorVariableIVX963SHA256AESGCM`,
`WhenUnlockedThisDeviceOnly`. Wrapping uses the public key and needs no
prompt. Unwrapping needs the prompt.

**Invalidation handling**
- Android `KeyPermanentlyInvalidatedException` → delete the alias, report
  `permanently_invalidated` (a precise signal).
- iOS: **only** `errSecInvalidKey` → `permanently_invalidated`; every other
  enclave error → `unknown`. This is conservative on purpose: treating
  generic errors as invalidation could trigger the tamper-wipe on a glitch.
- Default response: show the biometric error; the always-visible password
  field is the way in. Biometric unlock does not come back without a fresh
  setup. (`reWrapBiometric` exists in `CredentialStore` but is not exposed
  **[PARTIAL]**.)

**[DESKTOP]** see §17.2. This is the single largest porting decision.

### 3.4 Unlock

- **Password-first**: the password field is the default entry. Biometrics
  are **never** prompted automatically. They run only when the owner
  explicitly asks.
- The field is labelled neutrally ("Password") and gives no hint that a
  destruction password exists.
- **One password field, three outcomes** (`AuthController.submitPassword`):
  1. Check the destruction hash first (constant-time, §3.5). On a match, run
     the wipe (§3.6) silently.
  2. Otherwise derive `KEK_pwd`, unwrap the DEK, and open the blob → unlocked.
  3. Otherwise show the generic error "Incorrect. Try again."
  - Up to the point of success, the behaviour is indistinguishable across all
    three outcomes. Both checks run Argon2id, so their timing is comparable.
- **Biometric unlock**: unwrap the DEK through hardware, then open the blob.
  On failure, show a human sentence plus a short **non-secret** diagnostic
  tail (helps the owner report problems).
- **Opt-in tamper-wipe** (`wipe_on_bio_change`, default **off**): when on, a
  **confirmed** `permanentlyInvalidated` runs the full destruction wipe
  silently instead of showing an error. Never on cancel, lockout, or a
  transient error. Enabling it needs a danger confirmation, because the app
  can't tell an attacker's new fingerprint from the owner's re-enrollment.
  Since unlock is password-first, invalidation is only detected on an
  explicit biometric attempt. (Proactive launch-time probing **[NOT BUILT]**.)
- The config lives in memory only while unlocked (`VaultUnlocked.config`).
  `R2Config.toString()` redacts the secret.

### 3.5 Destruction password

- Stored as `Argon2id(password, own 16-byte salt)` in secure storage
  (`destruction_hash`, `destruction_salt`). It is **not** inside the
  encrypted blob, because it must be verifiable while locked.
- Its salt is independent of the login salt and is never reused.
- Compared in constant time (`constantTimeEquals`: XOR-accumulates over the
  full length; only a length difference can leak).
- Store APIs exist to change the login password (`changeLoginPassword`, which
  rewrites only the password wrap) and to set a new destruction password.
  Neither is exposed in Settings **[PARTIAL]**.

### 3.6 Wipe sequence

Triggered by the destruction password, by "Wipe everything", or by the
tamper-wipe. It must be **ordered, idempotent, and resumable**
(`VaultWiper`):

0. Write the tombstone `wipe_pending = 1` **before anything else**.
1. Delete the hardware key alias (`deleteBioKey`). From this point the
   biometric wrap is mathematically unrecoverable.
2. For every secure-storage key except the tombstone: **overwrite** it with
   32 random bytes (base64), then delete it.
3. Delete the `r2vault.sqlite` file and its `-wal` and `-shm` files. **The
   cache DB is deliberately excluded** (§5.6).
4. Empty these directories: temp, app-support (which takes
   `encryption_state.json`, `thumbs/`, and a default-located cache DB with
   it), app cache, and **on mobile only** the app-private documents dir.
   **Never touch the documents dir on desktop**, where it is the user's real
   Documents folder (already guarded in `_scratchDirectories`).
5. Delete the tombstone last. Afterwards `readAll()` on secure storage is
   empty.
6. *(caller)* Reset in-memory providers and go to onboarding step 1, with
   **no** toast, confirmation, or "wiped" message. It must look exactly like
   a fresh install.

- Each step is wrapped so that one failure never halts the wipe (it is
  logged and the wipe continues).
- On every launch, `resumeIfPending()` checks the tombstone and, if set,
  re-runs the whole wipe before anything else.

### 3.7 Runtime hygiene

- **Screen-capture blocking**: Android sets `FLAG_SECURE` (no screenshots,
  blank task-switcher preview). iOS uses an overlay for the switcher
  snapshot.
- **Privacy overlay**: on lifecycle `inactive`, `paused`, or `hidden`, an
  opaque blur is painted immediately, and it is removed on `resumed`.
- **Auto-lock**: on backgrounding, a timer starts (default 120 s; options
  30/60/120/300/600). On resume, if the idle time ≥ the timeout, the vault
  locks. There is also an explicit "Lock now".
- **On lock**, the encryption session (all epoch master keys plus any
  external passphrase) is wiped immediately (`EncryptionSession.clearAll`).
- **Logging**: `AppLog` is the only sink. `debug`/`info` are dropped in
  release builds. Callers must pass already-redacted strings: `redact()`
  keeps the last 4 characters, `redactUrl()` strips the query string, since
  presigned signatures live there. Credentials, presigned URLs, and keys are
  never logged. Error text shown in the UI never embeds a presigned URL
  (player errors are replaced by static copy).
- **Clipboard**: copying from secret fields is disabled. Only generated share
  links can be copied.
- **OS backups off**: Android has `allowBackup="false"` plus
  `data_extraction_rules.xml`. iOS excludes Application Support and Documents
  from backup. Thumbnails and the listing cache must not reach cloud backups
  or another device.
- **Session secrets in memory**: the DEK lives only for the duration of one
  operation. File master keys are held per session in a map that is wiped on
  lock. The external passphrase is held per session and **never persisted**.

---

## 4. Cloudflare R2 integration

### 4.1 Endpoint & signing

- Endpoint `https://<accountId>.r2.cloudflarestorage.com`, or a custom
  endpoint (for EU/FedRAMP jurisdictions). Region is the **literal `auto`**;
  service `s3`. Path-style addressing: `/<bucket>/<key>`.
- **Hand-rolled SigV4** (`sigv4_signer.dart`); no AWS SDK. It is a pure
  primitive, verified against the AWS SigV4 test-suite vectors.
  - Canonical request → string to sign → HMAC chain `AWS4<secret>` → date →
    `auto` → `s3` → `aws4_request`.
  - `x-amz-content-sha256`: the real SHA-256 for small bodies (single PUT,
    XML posts), the SHA-256 of the empty string for bodyless requests, and
    **`UNSIGNED-PAYLOAD`** for multipart parts, so large bodies are never
    hashed or buffered just to sign them.
  - **Path encoding**: RFC 3986 over UTF-8 bytes, `/` preserved. **The same
    encoder builds both the canonical URI and the wire URL**, so they can't
    disagree. Keys containing spaces, `+`, unicode, `#`, `&`, or `%` must
    round-trip; this is historically the #1 bug class, and it is unit-tested.
  - The canonical query is sorted by encoded key then value.
- **Presigned GET**: query-string signing with only `host` signed. Optional
  `response-content-disposition`. TTL is clamped to `[1, 604800]` s (R2's
  7-day max).
- **Clock**: an injectable clock (UTC) for signing. `SignatureDoesNotMatch`
  is surfaced as "check your device clock or secret key".

### 4.2 Operations

| Operation | Use | Notes |
|---|---|---|
| `ListObjectsV2` | Full scan (flat, `delimiter=''`), folder listing, sync remote map, folder fan-out | Paginates via `continuation-token`, 1000 keys/page. XML via the `xml` package (entity-escaped keys, namespace, CDATA). |
| `HeadObject` | Size for download progress, overwrite detection, auto-delete verification | 404 → `NotFoundFailure` |
| `GetObject` | Downloads (streamed), `Range` resume, 128-byte magic probe, 512-byte key-header probe, preview fetch | `ResponseType.stream` |
| `PutObject` | Uploads ≤ 8 MiB | Body fed in 64 KiB chunks for accurate send progress |
| `Create/UploadPart/Complete/AbortMultipartUpload` | Uploads > 8 MiB | Parts ≥ 5 MiB except the last |
| `CopyObject` + `DeleteObject` | Rename and move (no server-side rename) | |
| `DeleteObjects` | Batch delete, ≤ 1000 keys per call | Returns the keys that failed, with code and message |
| Presign GET | Share links, media streaming | Local only, no network |

- Never send storage-class or ACL headers; R2 rejects or ignores them.
- HTTP goes through `dio` (byte-level progress plus `CancelToken`).

### 4.3 Folder semantics

- R2 has no directories. A folder is a key prefix ending in `/`.
- "New folder" writes a zero-byte marker object at `prefix/`. Markers are
  hidden from file listings.
- Folders are inferred from every intermediate prefix of every key, plus
  marker keys (that's how empty folders appear).
- Folder rename/move = list every key under the prefix (paginated, flat) →
  `CopyObject` each to the new prefix → delete the originals that copied
  cleanly (batched). **Not atomic.** The result (`BulkResult`) reports the
  total and the failed keys, so the UI can say exactly what didn't move.
- Folder delete = paginated list + batched `DeleteObjects`. Deleting a large
  selection (> 50 objects) needs a typed `DELETE` confirmation.

---

## 5. Bucket-listing cache

### 5.1 Cache-primary browsing

A separate drift database (`cache_database.dart`, **never merged** with the
app DB) mirrors the bucket's object listing. **Browsing, overview stats,
search, sort, and per-folder counts are free local queries.** R2 is touched
only by:

1. An explicit **refresh** (pull-to-refresh, an error-state retry, or "refresh
   from bucket"), which runs `fullScan`.
2. Mutation call sites that **patch the cache directly** with a known outcome
   (upload completion, delete, rename, move, new folder). These re-read via
   `refreshFromCache()`, which performs **no network I/O**.

A cache that has never been scanned reports `hasScanned = false`, so the UI
can say "pull down to sync" rather than implying the bucket is empty. There is
no automatic scan on load.

### 5.2 Schema

`cached_entries`: `key` (PK; folders end in `/`), `isFolder`, `parentPrefix`
(`''` at root), `size`, `eTag`, `lastModifiedMs`, `encrypted`, `keyEpoch`.
`cache_meta` (a single row): `lastScanStartedAtMs`, `lastScanCompletedAtMs`,
`lastScanError` (a human-safe message only).

### 5.3 Queries

`childrenOf(prefix)`, `entryByKey`, `objectKeysUnder(prefix)`, `allEntries`,
`fileCountUnder` (non-recursive), **`folderUsage(prefix)`** (recursive files
and bytes, computed as a **key range** `[prefix, prefix + U+10FFFF)` rather
than `LIKE`, so `%`/`_` in a folder name can't widen the match),
`rootFolders`, whole-bucket totals, `objectTimestampsSince` (for the 7-day
upload histogram; day-bucketing happens in local time in the app layer),
`fileKeySizes` (two columns only, for the storage-by-type breakdown), and
`recentFiles(limit)` (undated objects sort last).

Mutations: `upsertEntry/Entries`, `deleteEntry`, `deleteEntryAndDescendants`,
`renameEntry`, `renamePrefix` (rewrites keys and the descendants'
`parentPrefix`), `setEncrypted`, and **`replaceAll`**: a single transaction,
so a reader never sees a half-applied scan.

### 5.4 Full scan (`BucketCacheService.fullScan`)

- **Single-flight**: a second call while one is running is a no-op.
- Walks the whole bucket flat and paginated, and collects every folder
  prefix.
- Encryption status per object, in priority order:
  1. **The encryption engine's index** (authoritative, free).
  2. **An existing cache row with the same eTag**: trusted as-is, no probe.
  3. **New or changed and unknown**: a **ranged GET of the first 128 bytes**
     checking for the `BFENC1`/`BFENC2` magic. Probes run with **bounded
     concurrency (6)** after the listing completes, so the scan isn't
     serialised on them. A probe establishes *that* an object is encrypted,
     not under which epoch (`keyEpoch = null`). An unreadable object counts
     as not encrypted (harmless).
- On success: `replaceAll` → record completion → hand back the complete
  picture (`encryptedKeys`, `presentKeys`) to
  `EncryptionEngine.reconcileFromScan` (§9.10) → report **stale thumbnail
  keys** (eTag changed, or the object is gone) to the thumbnail service.
- On failure: record a human-safe error, and leave the cache untouched. The
  reconcile step **never** runs on a partial listing, which would look like
  objects had vanished.

### 5.5 Mutation patching (call-site obligations)

Every mutation that changes an object's key must update **three indexes in
the same breath**: the cache DB, the encryption index (§9.10), and the
thumbnail store (§10.4).

| Mutation | Cache | Encryption index | Thumbnails |
|---|---|---|---|
| Rename/move file | `renameEntry` | `onRenamedOrMoved` | `onRenamedOrMoved` |
| Rename/move folder | `renamePrefix` | `onFolderMoved` | per key (from `objectKeysUnder` *before* the rewrite) |
| Delete file | `deleteEntry` | `onDeleted` | remove |
| Delete folder | `deleteEntryAndDescendants` | `onFolderDeleted` | per key (read first) |
| Upload complete | `upsertEntry` + ancestor folders (`cacheAncestorFolders`) | `markEncrypted` happened before enqueue | generated at enqueue |

A new mutation path must do the same. Both the single-item and the bulk
(multi-select) paths do this today.

### 5.6 Location & wipe exemption (owner-confirmed trade-offs)

- **Location** is configurable: the default is the app-support dir. A custom
  directory can be chosen so the cache **survives an uninstall**. Pointing
  Settings at the same folder after a reinstall **re-adopts** the existing
  DB. An unreachable custom directory falls back to the default rather than
  breaking browsing. Changing the setting closes the DB and reopens it at the
  new location. The on-disk size (main + WAL + SHM) is reported.
- **Exempt from the destruction wipe** (deliberate, recorded per §13.2):
  object names, sizes, timestamps, and encrypted-flags may survive a coercion
  wipe **if the cache was moved to shared storage**. Credentials, contents,
  and passphrases never survive. A default-located cache is still erased,
  because it lives in the app-support dir that wipe step 4 empties.
- The cache stores no secret, only what a bucket listing would reveal to
  anyone holding the credentials.

**[DECISION]** Whether the desktop port keeps this exemption. On desktop,
"outside the sandbox" is just any folder, and other same-user processes can
read it.

---

## 6. Browsing & file operations

### 6.1 Navigation & listing

- Folder navigation is held in controller state (not the route), so keys with
  slashes stay simple. Operations: open a folder, go up to a breadcrumb
  index, go to the parent (returns false at root, so back/escape can exit a
  folder before leaving the app), refresh, `refreshFromCache`.
- `BrowserData`: the current prefix, subfolders, files (markers excluded),
  per-folder recursive counts (`FolderCount`: files + bytes, exact, from
  `folderUsage`), `hasScanned`, and breadcrumb segments.
- **Search**: a client-side, case-insensitive filter within the current
  folder.
- **Sort** (`BrowserSort`): name A–Z/Z–A, date uploaded newest/oldest, size
  largest/smallest. **Folders always come first and are ordered by name**
  (they have no size or date of their own); a name sort flips their direction
  along with the files. The sort is held in its own provider, so it survives
  navigation and relaunches (persisted as `files_sort`). The default
  applies to a missing or unrecognised value.
- **View mode** (grid/list) persisted as `files_grid_view`. This is a
  preference, not a feature; noted only for parity.
- Each object carries an upload timestamp (`LastModified`), rendered as a
  relative age and an exact local time.

### 6.2 Overview statistics (all from the cache)

- Totals: file count, folder count, total bytes.
- **Storage composition by type** across five categories (Docs, Images,
  Videos, Music, Other), derived from extensions (`FileKind` →
  `StorageCategory`). R2 has no quota, so composition is shown, **never a
  fill percentage**.
- Recent files (newest first, limited).
- Root-level folders, largest first (capped), each with its recursive count
  and size.
- A 7-day daily upload histogram (oldest → newest, local-time days).
- Refresh = the same `fullScan` as the browser; both views share one cache.

### 6.3 Single-file operations

| Operation | Behaviour |
|---|---|
| Open/preview | §10.1 |
| Download | Plaintext → transfer queue (§7). Encrypted → foreground download + decrypt to the destination (§9.8). |
| Encrypt / Decrypt | Enqueues an encryption task (§9.5). Decrypt is not offered for external objects (§9.9). |
| Share link | §12 |
| Rename | Copy to `parent/newName` + delete; patches the three indexes (§5.5) |
| Move | Copy into a destination prefix (from a folder picker) + delete; a no-op if already there |
| Delete | `DeleteObject`; patches the indexes |

A file with an active encryption task is **locked**: no actions are allowed
until the task finishes (`encryptionLockedKeysProvider`).

### 6.4 Folder operations

Rename, move, and delete (§4.3), each with progress (done/total) and a
partial-failure report.

### 6.5 Multi-select (bulk) operations

The selection can mix files and folders. Folders expand to every key beneath
them.

- **Bulk delete**: expands, sizes the confirmation (typed `DELETE` for large
  sets), batches deletes at 1000/call, and patches all indexes.
- **Bulk move**: into a picked destination, via file and folder moves.
- **Bulk download**: plaintext files go to the background transfer queue
  immediately. **Encrypted files run in the foreground**: the key is unlocked
  once, then each file is downloaded and decrypted behind a progress dialog.
  (The background queue streams bytes straight to disk and would leave
  unreadable ciphertext.) Folder structure is recreated locally.
- **Bulk encrypt/decrypt**: the selection is split into
  *applies / already-in-target-state / locked-by-a-task / external*. The
  action applies only to the first group, and the rest are reported as
  skipped, never silently dropped. Decrypt skips external objects.
- **Bulk share**: one presigned link per file, copied together.

### 6.6 Upload flow (with overwrite detection)

1. Pick files (multi-select). Files without a local path are dropped.
2. *(Where supported)* an auto-delete review step (§8).
3. Resolve the **upload encryption target** once (§9.7). If encryption is on
   but the key can't be unlocked, upload as **plaintext with a clear
   warning**. Never silently, and never blocking the upload.
4. For each file, **overwrite detection**: R2 `PutObject` overwrites
   silently, so a `HeadObject` is made on the target key first. If it exists,
   the owner chooses *rename* (re-checked in a loop, so renaming onto another
   existing name prompts again) or *upload unchanged* (an intentional
   overwrite). A failed HEAD is treated as "doesn't exist", so a transient
   error never produces a spurious prompt.
5. If encrypting: encrypt to a temp file (`<temp>/enc_upload/`) →
   **`markEncrypted(key, epoch)` before enqueueing** (so thumbnail generation,
   which starts at enqueue, already sees the key as encrypted and stands down)
   → enqueue the temp file as the upload body. An encryption failure skips
   that file with an error.
6. Enqueue (§7).

**[NOT BUILT]** "Upload folder" (a whole-folder upload as a grouped
transfer). Folder upload exists only through Sync (§11).

---

## 7. Transfer engine

`application/transfers/transfer_engine.dart`: a persistent,
concurrency-limited queue over drift.

### 7.1 Model

`TransferKind { upload, download, folderUpload, folderDownload, move, copy,
delete }`. **Only `upload` and `download` are executed.** The other kinds are
reserved and return "unsupported" **[PARTIAL]**.
`TransferStatus { queued, running, paused, completed, failed, cancelled }`.

Row (`transfer_tasks`, schema v2): `id`, `kind`, `groupId?`, `localPath`,
`remoteKey`, `totalBytes`, `transferredBytes`, `status`, `error?`
(human-safe), `contentType?`, `multipartUploadId?`, `completedParts` (JSON),
`retryCount`, `deleteSourceRef?` (v2), `createdAt`, `updatedAt`.

### 7.2 Behaviour

- **Concurrency**: the global limit defaults to 3 (setting options 1–5,
  engine clamp 1–8). Changing it at runtime pumps the queue immediately. The
  persisted value is applied *before* the first pump.
- **Enqueue upload**: records the file length. Fires `onUploadEnqueued` (for
  the thumbnail, run in parallel; the upload never waits on it). Retains the
  source-delete permission if one was requested.
- **Enqueue download**: a best-effort `HeadObject` for the total size;
  progress still works if it fails.
- **Small upload (≤ 8 MiB)**: a single `PutObject`, with the body streamed to
  dio in 64 KiB slices so progress tracks real socket send rate (one event
  would report 100% before a byte had left).
- **Multipart (> 8 MiB)**:
  - Part size = 8 MiB, raised (rounded up to a MiB) so the part count stays
    < 1000. Never below 5 MiB.
  - `uploadId` is persisted the moment it's created. **`completedParts` and
    progress are persisted after every part**, so a restart resumes instead of
    restarting.
  - On resume, progress is seeded from *confirmed parts* only, never from the
    in-flight part's partial bytes. Seeding from those once double-counted and
    pushed the bar past 100%.
  - Parts upload **sequentially** within a task **[PARTIAL]**: `CLAUDE.md`
    asks for bounded per-task concurrency. Parallelism today comes from
    running multiple tasks at once.
- **Download**: writes to `<localPath>.part`. If a `.part` already exists,
  it resumes with `Range: bytes=<existing>-` and appends. On completion it
  **atomically renames** onto the final path, then fires
  `onDownloadCompleted` (for the thumbnail).
- **Pause**: cancels the running request (dio `CancelToken`) and sets
  `paused`; a queued task is just de-queued. **Resume** re-queues it.
- **Cancel**: cancels and **aborts the multipart upload** (avoiding orphaned
  storage charges), for both running and not-yet-running tasks.
- **Retry** (`RetryPolicy`): up to 5 attempts, exponential backoff with
  **full jitter** (uniform in `[0, min(500 ms × 2^(n-1), 30 s)]`). It retries
  **only `NetworkFailure`**, and the error mapping routes timeouts,
  connection errors, 5xx, 429, `SlowDown`, `ServiceUnavailable`,
  `RequestTimeout`, and `InternalError` there. Other 4xx errors never retry.
  `retryCount` is persisted.
- **Launch recovery** (`start()`): tasks left `queued` or `running` are reset
  to `queued` and pumped. Completed uploads whose source delete never ran are
  retried (§8).
- **Progress**: a throttled broadcast `Stream<TransferSnapshot>` with speed
  (bytes/s over the throttle window) and ETA. The throttle is **500 ms
  (~2/s)** in code, not the ~10/s in `CLAUDE.md`: the UI tweens between
  updates, and speed/ETA read less jittery. Progress is also written to the
  DB on each emission. A separate provider accumulates per-task live speed.
- **Errors** shown on a row are human sentences from `messageFor`. A crash
  inside a task is caught → `failed` with "Unexpected error".

### 7.3 Known limitation

There is no Android foreground service or iOS background session: a killed
app pauses in-flight transfers, and they resume on next launch.
**[DESKTOP]** The same limitation applies. Closing the window or quitting
ends transfers; the next launch resumes them. Whether closing the window
should keep transfers running in a tray/menu-bar process is a
**[DECISION]**.

---

## 8. Auto-delete after upload

Lets the owner remove a local original once its upload is **verified** in the
bucket.

- Before uploading, each picked file can be ticked for deletion. **Nothing is
  ticked by default.** Confirming states the count ("Upload & delete 2
  files"). A file whose original can't be addressed gets no checkbox at all.
- The row stores the **original's handle** in `deleteSourceRef`, not
  `localPath`. `localPath` is often a copy (a picker cache copy or an
  encrypted temp), and deleting it would remove nothing while reporting
  success.
  - Android: the picker's `content://` document URI, retained via
    `takePersistableUriPermission` at enqueue (so a resumed upload can still
    delete it). Deleted through `DocumentsContract.deleteDocument` over the
    `byteflow/files` channel. Falls back to the real path only for
    external-storage/`raw:` documents, and only when all-files access was
    already granted. Path traversal (`..`) and unknown providers are
    rejected.
  - iOS: not offered (there's no handle on the original).
  - **[DESKTOP]**: the plain filesystem path (already implemented in
    `sourceRefFor` / `PlatformSourceDeleter._deleteFile`).
- **Deletion only after** the upload completes **and** a `HeadObject` shows
  the object at the uploaded size.
  - Size mismatch → the file is kept, reported as `unverified`.
  - Verified, but the OS refuses the delete → the file is kept, reported as
    `failed`.
  - HEAD unreachable → the ref stays on the row, and `start()` retries next
    launch.
  - **One delete attempt per row**, after which the ref is cleared, so it is
    never retried against a path a later file may have reused.
  - A failed or cancelled upload never deletes.
- Every attempt emits an outcome on `TransferEngine.sourceDeletions`
  (`deleted` / `failed` / `unverified`), and each is reported to the owner.
  **Never silent.**

---

## 9. File-content encryption

An opt-in layer, separate from credential encryption: it encrypts what's
*inside* objects, so the bucket holds only ciphertext.

### 9.1 Object format (`core/crypto/file_crypto.dart`)

Streaming AES-256-GCM over **64 KiB plaintext chunks** (Tink STREAM
construction), with a random 32-byte **per-file content key** wrapped under a
master key.

```
v2 (current, "BFENC2")                        v1 (legacy, "BFENC1")
  magic        "BFENC2"            6 B          magic "BFENC1"   6 B
  kdfSaltLen   u8                  1 B          (no KDF fields)
  kdfSalt      random              n B
  kdfMemKib    u32 BE              4 B
  kdfIters     u16 BE              2 B
  kdfPar       u8                  1 B
  wrapLen      u16 BE              2 B          wrapLen, wrappedKey, noncePrefix, chunkSize
  wrappedKey   Envelope(contentKey, master; AAD "r2vault.v1.file.key")
  noncePrefix  random              7 B
  chunkSize    u32 BE              4 B
  segment_i    AES-GCM(contentKey, nonce_i, chunk_i, AAD = entire header) ‖ tag(16)
```

- **nonce_i = noncePrefix(7) ‖ u32BE(i) ‖ lastFlag(1)**. The last segment is
  tagged `0x01`, all others `0x00`. This defeats reordering (the index is
  authenticated) and truncation (dropping the true last segment leaves a
  `0x00` where a `0x01` is expected).
- **The entire header is the AAD of every segment**, so tampering with the
  salt, params, wrapped key, or chunk size fails authentication. No
  downgrade, and no salt substitution to force a weaker KDF.
- An empty file still produces one empty final segment.
- **Untrusted-header bounds** (checked before any allocation or KDF): salt
  1–64 B, memory ≤ 512 MiB, iterations ≤ 16, parallelism ≤ 32, wrap
  28–256 B, chunk size > 0. A hostile `kdfMemKib` must never become an
  allocation.
- A wrong key, tampered data, and truncation all fail identically (no
  oracle). The one deliberate exception is `deriveKeyForObject`, which says
  "that passphrase does not open this object". That only answers a question
  anyone holding the bytes can compute offline, and it reveals nothing about
  the body.
- **v1** objects (encrypted before 2026-08-07) have no embedded salt: they
  can only be decrypted with the device-held master key, never by
  passphrase. **Nothing writes v1 anymore.** Re-encryption (a key change)
  upgrades them to v2.
- **v2** objects are **self-recoverable: the bytes plus the passphrase are
  all that's needed.** No device, database, or secure-storage entry has to
  survive.
- All encrypt/decrypt work is file-to-file on seekable local files, inside
  `Isolate.run`. Key buffers are copied into the isolate and wiped there.
- Helpers: `isEncryptedFile`, `startsWithMagic`, `readKeyInfo(FromBytes)`
  (parses only the key-bearing header from ≤ 512 B), `opensWith(Info)` (one
  GCM open of ~60 B: a definitive "is this my key"), `deriveKeyForInfo`,
  `encrypt/decryptFileWithPassphrase` (one-shot, fresh salt per call).

### 9.2 Master key & epochs

- **Set key**: passphrase → Argon2id (64 MiB/t=3/p=4) with a fresh 16-byte
  salt → 32-byte master key. It is sealed under the DEK (a biometric-gated
  unwrap) into `enc_master_blob`, **with the salt and params stored
  alongside** (`enc_master_kdf_salt`, `enc_master_kdf_params` as `mem:it:par`)
  so every new object can embed them without re-deriving. Losing that local
  copy only means the next encrypt asks for the passphrase again. It is never
  a data-loss event.
- **Epochs** (starting at 1) identify which master key wrapped an object. At
  most two are live at once (active + rotation target).
  `EncryptionState.encrypted : Map<remoteKey, epoch>`.
- **Session keys** (`EncryptionSession`): `epoch → key`, loaded on demand by
  `ensureKeyLoaded()` (a biometric DEK unwrap; a password variant exists in
  the store) and **wiped on lock**.
- Recovery-password variants exist for sealing and opening the master key.

### 9.3 Settings-level operations

| Operation | Behaviour |
|---|---|
| Set key | §9.2 |
| Encrypt new uploads | Toggle (`encrypt_new_uploads`, default off; only effective once a key is set) |
| Encrypt all existing | Lists the whole bucket and enqueues encrypt for every plaintext object; returns the count |
| Change key (rotation) | Derive a new key → seal it into the **pending** slot (+ its own salt/params) → `beginRekey(newEpoch)` enqueues re-encrypt of every app-encrypted object. Fails fast if a rotation is already running. |
| Remove encryption | `beginRemoval()` sets the persisted `removing` flag and enqueues decrypt of every app-encrypted object |
| Concurrency | `encryption_concurrency`, default **2** (lower than transfers: it's CPU work) |
| Queue view | Active tasks with phase and progress; cancel queued, retry failed, clear finished |

### 9.4 Rotation & removal finalisation (resumable)

When the queue drains (`_onIdle`, only if no task is active):

- **Rotation**: if every *app-encrypted* object is at the target epoch, the
  epoch advances and `finalizeRekey(previousEpoch)` runs: the pending key is
  promoted to active (a pure move, no unwrap), the **external-key table is
  re-sealed** from the old master key to the new one while both are still in
  the session, then the session keeps only the new key.
- **Removal**: once there are no app-encrypted objects left, the flag clears
  and `finalizeRemoval` runs: the master key, its salt/params, and the
  external-key table are deleted.
- Failed tasks keep the operation pending until a retry succeeds.
  `rekeyToEpoch` and `removing` are persisted, so both resume across a
  restart instead of stranding a half-converted vault.
- External objects are excluded from both conditions (§9.9); otherwise
  they'd hang the operation forever.

### 9.5 Encryption engine (queue)

- A persistent queue (in `encryption_state.json`) of `EncryptTask { id,
  remoteKey, op: encrypt|decrypt|reEncrypt, status:
  queued|running|completed|failed, error?, updatedAtMs }`.
- Each task is a **round trip**: download to `<temp>/enc_work` → transform in
  an isolate → upload back to the **same key** (PUT ≤ 8 MiB, otherwise
  multipart, which is aborted on part failure) → update the index → delete
  the temps.
- **Phases** `downloading → transforming → uploading`, with real byte
  progress for the two network legs. The transform leg is indeterminate.
  Progress is throttled to ~10/s (100 ms) per task, **never persisted**, and
  doesn't bump the sort key (so rows don't jump around).
- **Key parking**: a task whose epoch key isn't loaded (for example right
  after a restart) is skipped while others run, and `kick()` resumes it once
  keys load.
- Tasks interrupted mid-run reset to `queued` on start. **A task retries as
  a whole**; there's no mid-object resume **[PARTIAL]**.
- **Encrypt** embeds the active epoch's KDF info (v2). **Re-encrypt** decrypts
  with the old epoch key and encrypts with the pending epoch's key and KDF
  info, so the output is always v2.
- The state store writes **atomically** (`.tmp` + rename) and **serialises
  writes** on one chain, so concurrent task transitions can't interleave or
  clobber each other. A corrupt file loads as empty state rather than
  breaking the app.
- `onIndexChanged` mirrors each definitive outcome into the cache DB's
  `encrypted`/`keyEpoch` columns.

### 9.6 Which store is authoritative

**The engine's `EncryptionState.encrypted` index is the only source the app
consults** for "is this object encrypted": badges, the availability of
Decrypt/Share, preview's decrypt path, epoch selection, and thumbnail
eligibility. The cache DB's columns are a mirror kept for the scan's own
bookkeeping, never a second opinion.

### 9.7 Encrypt-on-upload

`uploadEncryptionTarget()` returns `{key, epoch, kdfSalt?, kdfParams?}` or
null (toggle off / no key). **During a rotation it returns the new epoch's
key**, so an upload made mid-rotation survives the key swap. KDF info is null
only for an epoch that predates v2, which falls back to v1 for that one
upload. Ordering rules are in §6.6.

### 9.8 Opening encrypted objects

- **`decryptToTemp(key)`** (preview): resolve the key **before** downloading
  (§9.9) → download to `<temp>/enc_preview/<id>.enc` → decrypt to
  `<id>-<leafName>` (keeping the extension so players can infer the type) →
  delete the ciphertext temp. **The caller must delete the plaintext when the
  viewer closes**; it is also under the temp dir that the wipe empties.
- **`downloadDecrypted(key, dest)`**: the same, but the output goes to the
  owner's download folder, with download progress.

### 9.9 Objects encrypted outside the app (CLI / another device)

- A CLI object is wrapped under a key derived from a **fresh salt per file**,
  so no epoch key opens it, even with the same passphrase. The salt is in its
  header, so **the passphrase is the only missing input**.
- **Key resolution is header-first**: a ranged GET of the first **512 B**
  (`keyHeaderProbeBytes`) → `readKeyInfoFromBytes`. If the index names an app
  epoch, that key is tried with `opensWithInfo`. On a miss, the index entry is
  **re-filed as external** (`externalEpoch = -1`); this correction is one-way
  and self-healing. Then:
  1. Look up the **external-key table** by `kdfSaltId` (base64 of the header
     salt, so renames and moves can't strand it).
  2. Else derive from the **session passphrase**, if one was given this
     session.
  3. Else return `ExternalKeyFailure`, the single "needs a passphrase"
     signal. The UI prompts and retries; a wrong passphrase returns the same
     failure.
- A successfully derived key is **sealed under the app's file master key**
  into `enc_external_keys` (no second biometric prompt, since that key is
  already in the session). The passphrase stays in memory for the session
  only. Result: the first external object in a session prompts once, the rest
  open silently, and adopted ones open with no prompt across restarts. With no
  app master key configured (CLI-only use), sealing is skipped and the session
  passphrase alone saves re-prompts.
- An object that **can't be opened is never fully downloaded**.
- **The object is never rewritten** under the app's key. That would mean a
  full download, re-encrypt, and upload, because the header is the AAD of
  every segment. The owner rejected this on cost.
- **Rotation and removal skip external objects**, Decrypt-in-place is not
  offered for them, and `_onIdle` ignores them.
- A **v1 external object can't be opened** anywhere except its origin
  device, and says so plainly (`legacyFormatMessage`).

### 9.10 Index maintenance & reconcile

- **Hard rule**: every mutation that changes an object's key re-keys the
  index in the same call (`onRenamedOrMoved`, `onFolderMoved`, `onDeleted`,
  `onFolderDeleted`). A missed one strands the object: its bytes are
  ciphertext, but the app treats it as plaintext, offers no Decrypt, and
  preview feeds ciphertext to the player.
- **`reconcileFromScan` is the backstop**: after a *completed* full scan it
  **adopts** encrypted keys the index doesn't know (at their known epoch, or
  at the active epoch when the probe couldn't tell; first open then verifies
  and re-files, per §9.9) and **drops** entries for keys the bucket no longer
  holds. It **stands down** while a rotation or removal is in flight, or any
  task is queued or running, because those are the windows where bucket and
  index are *expected* to disagree.
- This means a refresh repairs an index stranded by an older bug, and **a
  reinstall recovers its encrypted-object list from the bucket itself**.

### 9.11 Standalone CLI (`bin/vault_crypto.dart`)

`dart run bin/vault_crypto.dart -e|-d -p <passphrase> [-o out] [-f] <file>`.
Local file to local file only, never R2. It imports `file_crypto.dart`
directly (no reimplementation). It produces and reads **v2 only**. Default
outputs are `<file>.enc`, and for decrypt `.enc` is stripped (otherwise
`.dec`). It **never overwrites without `-f`**. A wrong passphrase and a
tampered file fail identically. It must run from inside the project (it needs
the resolved `package_config.json`). **[DESKTOP]** It already works on all
three desktop OSes. The desktop app should produce objects that the CLI and
the mobile app can open, and the reverse.

---

## 10. Media preview & thumbnails

### 10.1 Preview

- **Media kinds** by extension (`mediaKindForKey`):
  - image: `jpg jpeg png gif webp heic bmp svg` (viewable set
    `kViewableImageExts`)
  - video: `mp4 mov mkv avi webm m4v`
  - audio: `mp3 wav flac aac ogg m4a opus`
  - text: `txt md log ini conf env` + code extensions + `csv`/`tsv`
  - otherwise none
- **Plaintext objects stream straight from a presigned GET** with a **6-hour
  TTL**. There is no disk cache and no temp file. **Playback presigns are not
  share links**: they're never recorded in shared-links history.
- Images decode from the URL, video and audio play from it (`video_player`,
  `just_audio`), and the screen is kept awake only while a video is actually
  playing (`wakelock_plus`).
- **Text viewer**: fetches up to **2 MiB** (`kTextPreviewMaxBytes`) and
  decodes it as text (encoding handling in `text_decode.dart`). It renders
  text, code, and CSV/TSV.
- **Encrypted objects** go through `decryptToTemp` (§9.8), including the
  external-passphrase retry loop. The plaintext temp is deleted when the
  viewer closes.
- Player and network errors are shown as static copy. Raw error text can
  embed the presigned URL and must never surface.

### 10.2 Thumbnail policy (hard security boundary)

`thumbIneligibleReason(key, isEncrypted, isLocked)` is the **single
predicate** both generation and display route through:
`folder` → `encrypted` → `encryptionInFlight` → `notMedia` (only image and
video are eligible). `isEncrypted` and `isLocked` **must come from the
engine's index**, never from the cache column. The predicate is re-checked on
every read **and again immediately before every write**, so an object
encrypted mid-generation never gets a thumbnail on disk. Preview's
decrypt-to-temp file is never a thumbnail source.

### 10.3 Generation (never speculative)

Rendering a cell is an in-memory set lookup and **issues no network request,
ever** (this preserves §5.1). Thumbnails come only from bytes that are already
in hand, or already justified:

- **Upload**: from the local source file, started at **enqueue** (in
  parallel; the upload never waits).
- **Download complete**: from the file just written.
- **Image preview**: reuses the frame already decoded for the screen (zero
  extra bytes).
- **Video preview**: the platform frame extractor runs over the same
  presigned URL, range-reading the container index plus one keyframe (a few
  hundred KB).
- **There is no bucket-wide backfill.** Objects uploaded elsewhere show their
  type icon until opened once. Decryption doesn't regenerate a cover either.

Output: max edge **144 px** (48 logical × 3 dpr), JPEG quality **80**,
~4 KB each. Native decoders **downsample during decode**, so a 48 MP photo
never becomes a ~190 MB bitmap in the Dart heap. HEIC and EXIF orientation
are handled natively. Raising `kThumbMaxEdge` requires clearing the store.

### 10.4 Storage & invalidation

- `<app-support>/thumbs/<first 2 hex>/<sha256(key)>.jpg`. Sharding keeps
  directories small, and hashing sidesteps every filesystem limit an R2 key
  can trip (length, slashes, unicode, reserved names).
- **App-private and non-negotiable**: never configurable, never colocated
  with the listing cache. Wipe and uninstall must take it. A thumbnail is a
  picture of file content, so the cache's "no secret" reasoning does **not**
  extend to it.
- Presence is a **memory-only set**, loaded once at startup. Lookups never
  `stat`.
- **Invalidation** covers every path that changes an object's bytes: rename,
  move, delete, and their folder equivalents (folder paths read
  `objectKeysUnder` *before* the cache rows are rewritten); a completed scan
  retires thumbnails whose eTag changed or whose object is gone; and the
  service **subscribes to the encryption engine's state stream**, so anything
  that becomes encrypted loses its cover. That sweep runs over the whole index
  (idempotent, so a missed event can't strand a plaintext thumbnail). The
  dependency is one-way: thumbnails know about encryption, never the reverse.
- The owner can clear all thumbnails.

### 10.5 Thumbnail channel contract (`byteflow/thumbnails`)

Native generation from a local file (image or video) or a URL (video frame).
Arguments include `maxEdge` and `quality`; the result is JPEG bytes or null.
The Android reference uses `ImageDecoder`/`MediaMetadataRetriever`; iOS uses
`CGImageSource`/`AVAssetImageGenerator`. **[DESKTOP]** Needs a native
implementation per OS (see §17.5), or the platform simply shows type icons
(`Thumbnailer` returning null is a supported, safe outcome).

---

## 11. Directory sync

Scope: **one-way, local → remote, prompt-driven.** No automatic uploads, no
deletions, and no automatic conflict resolution.

- **Sync pairs** (`sync_pairs`): `{ id, localDirPath, remotePrefix,
  lastScanAt? }`. Multiple pairs are supported. Deleting a pair deletes its
  manifest.
- **Manifest** (`sync_manifest`, PK `(pairId, relativePath)`): `size`,
  `mtimeMs`, `lastUploadedMtimeMs?`.
- **Scan**:
  1. Walk the local directory recursively (no symlink following), skipping
     **hidden files** (leading `.`), `.DS_Store`, `Thumbs.db`, and `*.tmp`.
     Unstattable files are skipped, not fatal. Relative paths are normalised
     to `/`.
  2. List the remote prefix (flat, paginated) → `relPath → (size,
     isMultipartETag)`. Markers are excluded.
  3. **Classify** (the pure function `SyncEngine.classify`, truth-table
     tested):
     - no remote → **New**
     - size differs → **Modified**
     - no recorded upload mtime, or local mtime newer → **Modified**
     - otherwise **Unchanged**
     - Remote keys with no local file → **Remote-only** (informational,
       never deleted)
  4. Review: grouped, selectable, with a total byte count. **Nothing uploads
     until the owner confirms.**
  5. Confirmed items are enqueued as uploads; the manifest is updated on
     success.
- **[PARTIAL]**: the walk runs on the main isolate (not an isolate) with no
  progress counter and no cancel; there is no optional MD5 content-hash
  verification; `.nomedia` isn't respected; and the ignore list isn't
  user-editable. `CLAUDE.md` §7 specifies all of these. Multipart ETags are
  already never treated as MD5.
- **[DESKTOP]** Sync is likely the headline desktop feature. Large trees make
  the isolate walk, progress, and cancel requirements matter more; implement
  them there rather than copying the simplified version.

---

## 12. Sharing links

- **Presigned GET** is signed locally with the stored credentials. No server.
- The TTL comes from the **default link TTL** setting (presets 15 min, 1 h
  *(default)*, 24 h, 7 days), **clamped to R2's 7-day maximum**.
- On generation: show the link, state validity, and state plainly that it
  **can't be revoked without rotating the access key**. Copying to the
  clipboard is the only clipboard write the app allows.
- **History**: each issued link is recorded (`shared_links`: `id`,
  `remoteKey`, `createdAt`, `expiresAt`), **never the signed URL itself**,
  because it contains a signature derived from the secret. The shared-links
  view lists them with live countdowns, and records can be forgotten.
- Bulk share generates one link per selected file.
- **[NOT BUILT]**: a per-link TTL choice at generation time (custom TTL), a
  native share sheet, a "public link" option using the configured Public URL
  (stored in config but unused), and "rotate credentials" guidance linking to
  the Cloudflare dashboard. All are in `CLAUDE.md` §9.

---

## 13. Preferences

All are stored in secure storage via `VaultStateStore` (so all are erased by
a wipe).

| Key | Meaning | Default |
|---|---|---|
| `auto_lock_seconds` | Auto-lock timeout | 120 |
| `transfer_concurrency` | Parallel transfers | 3 |
| `encryption_concurrency` | Parallel encryption tasks | 2 |
| `default_link_ttl_seconds` | Share-link TTL | 3600 |
| `wipe_on_bio_change` | Tamper-wipe (§3.4) | off |
| `encrypt_new_uploads` | Encrypt on upload | off |
| `download_dir` | Custom download folder | null = app-private documents dir |
| `cache_db_dir` | Cache DB folder | null = app-support |
| `files_sort` | Browser sort (`BrowserSort.name`) | name A–Z |
| `files_grid_view` | Grid vs list | list |
| `theme_mode` | light / dark / system | light |

Related maintenance actions:

- **Download folder**: set or reset it. For a custom folder, check that it's
  reachable, and fall back to the default if it becomes unreachable.
- **Private downloads**: show their usage, **move** them all into the chosen
  download folder (preserving structure, skipping `.part` files and existing
  destinations; uses copy + delete across filesystems), or **delete** them
  all (keeping in-flight `.part` files). Empty directories are pruned.
- **Cache**: change its location (re-adopting an existing DB) and show its
  size.
- **Thumbnails**: show their usage and clear them.
- **R2 config edit**: reseal the blob under the same DEK through a biometric
  unwrap, and hot-swap the session config so the client and engines pick up
  the new credentials. The stored secret is never revealed.
- **Wipe everything**: typed confirmation, then §3.6.

---

## 14. Error model

### 14.1 Failure taxonomy (`core/failures.dart`, sealed)

`NetworkFailure(isTimeout)`, `AuthFailure(clockSkewSuspected)`,
`PermissionFailure`, `NotFoundFailure`, `ConflictFailure`,
`StorageFullFailure`, `CryptoFailure`, `ExternalKeyFailure`,
`BiometricFailure(reason)`, `UnknownFailure`. `message` is developer detail
and never a secret; `cause` is kept for logging only.

### 14.2 S3 / transport mapping (`data/r2/r2_failure.dart`)

| Source | Maps to |
|---|---|
| `SignatureDoesNotMatch` | `AuthFailure(clockSkewSuspected)` |
| `InvalidAccessKeyId`, `InvalidSecurity`, `AuthorizationHeaderMalformed` | `AuthFailure` |
| `AccessDenied`, HTTP 403 | `PermissionFailure` |
| `NoSuchKey`, `NoSuchUpload`, `NoSuchBucket`, HTTP 404 | `NotFoundFailure` |
| `EntityTooLarge`, `MaxMessageLengthExceeded` | `StorageFullFailure` |
| `PreconditionFailed`, `InvalidPart(Order)`, `BucketAlready*`, HTTP 409 | `ConflictFailure` |
| `SlowDown`, `ServiceUnavailable`, `RequestTimeout`, `InternalError`, HTTP 429, 5xx | `NetworkFailure` (retryable) |
| dio timeouts | `NetworkFailure(isTimeout: true)` |
| connection error, bad cert, TLS handshake / socket error (e.g. wrong Account ID) | `NetworkFailure` |
| other | `UnknownFailure` |

Error bodies are parsed only when they contain `<Error`. Bodyless errors (for
example HEAD 404) are classified by status.

### 14.3 Human text

`core/failures_ui.dart` is **the single place** a failure becomes a sentence;
raw exception strings never reach the user. Biometric reasons each get a
sentence. `connectionDiagnostic` gives action-oriented text for the
connection test. In particular, a bucket-level 404 means "credentials
accepted, bucket not found at this endpoint: check the name or set the
jurisdiction endpoint".

---

## 15. Performance & optimizations

Collected here so the desktop port keeps them. Each one exists for a measured
or reasoned cause.

**Network**
- **Cache-primary browsing** (§5): zero R2 calls for navigation, search,
  sort, counts, or overview stats. Bucket scans only on explicit refresh.
- **Bounded probing**: on a scan, only new or changed (eTag-different)
  objects the engine doesn't know get a magic probe, at **128 B** each with
  **6 concurrent**. Unchanged rows are trusted.
- **Header-first key resolution** (512 B ranged GET) before any full
  encrypted download. Unopenable objects are never downloaded.
- **Video thumbnails** range-read only the container index plus one keyframe.
- **`UNSIGNED-PAYLOAD`** for multipart parts, so nothing is hashed or
  buffered just to sign it.
- **Multipart resume** at part granularity; **Range resume** for downloads.
- **Retry with full jitter** on transient failures only.

**Memory**
- Streamed downloads. Encrypt/decrypt in 64 KiB slices (file to file).
- Upload bodies are fed as **64 KiB views** (not copies) of a part, which is
  also what makes upload progress accurate.
- Bounded buffers: single PUT ≤ 8 MiB; one part (8 MiB+) per running upload.
- Native thumbnail decoders downsample *during* decode.
- Cache queries select only the columns needed (`fileKeySizes`).

**CPU / UI responsiveness**
- Argon2id lanes on worker isolates; file crypto in `Isolate.run`.
- Progress throttling: transfers 500 ms, encryption 100 ms. Encryption
  progress doesn't touch the sort key (avoiding list churn).
- Thumbnail presence is an in-memory `Set` (no `stat` on the render path).
- `RepaintBoundary` around progress bars and list tiles (UI detail; listed
  for completeness).

**Storage / correctness**
- Atomic writes: `.part` → rename for downloads; `.tmp` → rename for the
  encryption state; a single transaction for `replaceAll` on the cache.
- A serialised write chain for the encryption state store.
- The key-range prefix query (`U+10FFFF` upper bound) instead of `LIKE`.
- Sharded, hashed thumbnail paths.
- `NativeDatabase.createInBackground` for drift (SQLite off the UI isolate).
- Lazy DB open, so directory resolution can await secure storage.

---

## 16. Testing requirements

Port these suites (most are pure Dart and run unchanged on desktop) and add
desktop channel tests.

- **Unit**: SigV4 against the AWS vectors and key-encoding round-trips;
  Argon2id determinism; envelope seal/open and tamper; `file_crypto` v1/v2
  round-trip, tamper, truncation, reorder, legacy compat, and external
  primitives; the sync classifier truth table; multipart part-size maths;
  retry policy; cache DB queries; thumbnail policy and store; `Result`.
- **Integration**: the transfer engine against a mocked dio (progress,
  retry, resume, cancel, abort-on-cancel, auto-delete only when verified,
  never on failure, retried after a kill); wipe idempotence and interruption
  recovery; bucket-cache probe/trust/skip against a fake transport; browser
  and overview controllers against a fake cache service (`refresh()` scans,
  `refreshFromCache()` never does); encryption-index maintenance and the
  `reconcileFromScan` adopt/drop/stand-down rules; external-object open
  (header-first resolution, re-filing, stored and session key reuse,
  rotation/removal skipping); the thumbnail service (an **encrypted key
  produces no platform call at all**, plus the encrypted-mid-generation
  race); the CLI driven as a real subprocess.
- **Security (non-negotiable)**: after `wipe()`, secure-storage `readAll()`
  is empty, the hardware alias is gone, the app DB file does not exist, and
  the thumbnail cache is gone. The listing cache is explicitly exempt.
  **[DESKTOP]** Also assert that the documents dir is untouched by a desktop
  wipe.
- **Widget / golden**: onboarding validation, the three unlock-password
  outcomes, transfer states, thumbnail fallbacks (an encrypted object keeps
  its icon even with a stored file), and design-system goldens.
- **Gate before every commit**: `dart format . && flutter analyze && flutter
  test`, with analyzer warnings treated as errors.

---

## 17. Desktop adaptation

Everything in §3 is authoritative. Where a desktop OS can't provide a mobile
guarantee, this section names the gap. Per `CLAUDE.md` §13.2/§13.6, each
**[DECISION]** needs the owner's answer before implementation. None is
silently resolved here.

### 17.1 Platform mapping

| Concern | Mobile | Desktop requirement / candidate |
|---|---|---|
| Secure storage | Android Keystore-backed prefs / iOS Keychain (this-device-only) | `flutter_secure_storage`: Windows Credential Manager/DPAPI-protected, macOS Keychain, Linux libsecret. All are **user-session-bound, not hardware/biometric-gated**. The blob is still DEK-sealed, so storage only needs to be confidential at rest. |
| `KEK_bio` | Keystore AES-GCM / Secure Enclave ECIES | See §17.2 **[DECISION]** |
| Screenshot / capture blocking | `FLAG_SECURE`, switcher overlay | Windows `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`; macOS `NSWindow.sharingType = .none`; Linux has no general equivalent **[DECISION]** |
| Privacy overlay trigger | `inactive/paused/hidden` | On desktop, `inactive` fires on every focus loss. Blur on minimise/hide/OS-lock only, or on focus loss too? **[DECISION]** |
| Auto-lock trigger | Backgrounded for N seconds | Candidates: minimised/hidden for N seconds, idle for N seconds, **and immediately on OS session lock / sleep**. **[DECISION]** |
| OS backups | `allowBackup=false`; iOS exclusion | Windows: `path_provider`'s app-support dir is **Roaming** AppData, which can roam to other machines in managed environments, contrary to §3.7. Prefer Local AppData. macOS: exclude from Time Machine (`NSURLIsExcludedFromBackupKey`) and keep out of iCloud-synced folders. **[DECISION]** on locations. |
| Documents dir | Wiped (mobile) | **Never wiped.** It is the user's real Documents folder (already guarded). |
| Default download folder | App-private documents dir | On desktop that would be the real Documents folder. Choose a default (e.g. the OS Downloads folder) or require a choice. `privateDownloadsDir()` returns null on desktop, so the "private downloads" move/delete actions don't apply. **[DECISION]** |
| Source auto-delete | Document URI via channel | Plain path, already implemented |
| Storage permissions / SAF | `MANAGE_EXTERNAL_STORAGE`, tree-URI resolution | Not applicable; skip these code paths on desktop |
| Thumbnails | Native channel | §17.5 |
| Media playback | `video_player`, `just_audio` | Verify plugin support per desktop OS before relying on it; any replacement plugin needs owner approval (§2) |
| Keep-awake | `wakelock_plus` | Verify per OS |
| Clipboard | Copy only for share links | **Windows clipboard history and cloud clipboard** can retain or sync a presigned URL (a secret, §12). Mark copies as excluded from history/cloud (`ExcludeClipboardContentFromMonitorProcessing` / `CanIncludeInClipboardHistory = 0`) and consider auto-clearing. **[DECISION]** |
| Background transfers | Paused when killed | Same; optional tray/menu-bar residency **[DECISION]** |
| Delivery | `scripts/build.sh` → APK on the local server | Needs a desktop build/sign/publish path. Windows code signing, macOS notarisation. The existing script is APK-only. |

### 17.2 Hardware-bound key (`KEK_bio`) on desktop: **[DECISION]**

Required properties (from §3.3): the key is generated in and never leaves
secure hardware; using it requires a live biometric (or platform-equivalent)
check that **the OS enforces** (a boolean returned to the app is not enough);
re-enrollment invalidation is detected; the key can be deleted for the wipe;
and the channel contract and failure codes stay exactly as in §3.3, so
`CredentialStore` and `AuthController` are untouched.

Candidates (each must be verified against those properties before it's
chosen):

- **macOS**: the iOS design ports almost directly. A Secure Enclave P-256
  key with `.biometryCurrentSet` and ECIES (Apple Silicon / T2 Macs). Keychain
  access needs proper signing and entitlements.
- **Windows**: Windows Hello / TPM via CNG (the Microsoft Passport KSP or
  `KeyCredentialManager`). Hello keys are typically signing-oriented, so a
  decrypt-gated wrap may need a different construction. Confirm that it is
  OS-enforced, and check how re-enrollment shows up (it may not be
  detectable, which would mean a §4.3-style conservative mapping). A
  DPAPI-only or user-session-only key does **not** meet the property.
- **Linux**: no standard biometric-gated hardware key store. Likely
  **password-only** (no `dek_wrap_bio`, which `CredentialStore` already
  supports: `isConfigured` accepts either wrap). The UI must then not offer
  biometrics.
- **Machines without biometric hardware** (any OS): password-only, as above.

Whichever approach is chosen, the **recovery password wrap stays mandatory**,
and the tamper-wipe stays opt-in and fires only on a *confirmed* invalidation.

### 17.3 Single instance (new requirement)

Two concurrent desktop instances would both open the SQLite databases, run
two transfer and encryption engines over the same queue rows, and race on
`encryption_state.json`. The desktop app **must enforce a single instance**
(focus the existing window on a second launch). Mobile never needed this.

### 17.4 Same-user readable data

On desktop, other processes running as the same user can read the app
directory. The DEK-sealed stores are fine. Plaintext-at-rest items need an
explicit owner sign-off on desktop: the cache DB (object names), the app DB
(transfer queue paths, sync pairs, shared-link keys), `encryption_state.json`
(encrypted object names), and thumbnails (pictures of content).
**[DECISION]**: accept, or seal these under a session key.

### 17.5 Thumbnails on desktop

Implement `byteflow/thumbnails` natively (Windows: WIC + Media Foundation or
the shell thumbnail provider; macOS: ImageIO + AVFoundation, the same APIs as
iOS; Linux: optional). Or ship without it, since icon fallback is a supported
outcome. The rules in §10.2–§10.4 apply unchanged; in particular the encrypted
predicate must be checked before **any** platform call.

### 17.6 What carries over unchanged

The R2 client and signer, both databases and their queries, the transfer
engine, auto-delete (path mode), the encryption format, engine, rotation, and
external-key flow, the bucket cache and reconcile, the sync classifier, the
share-link logic, the error model, the wipe sequence (with the
documents-dir guard), logging and redaction, and the CLI.

---

## 18. Known gaps on mobile

Specified in `CLAUDE.md`, not (fully) built. Don't treat these as parity
requirements unless the owner asks for them in the desktop build.

| Area | Gap |
|---|---|
| Transfers | Multipart parts upload sequentially per task; `folderUpload/folderDownload/move/copy/delete` kinds are reserved, not executed; no grouped parent-progress for folder transfers; the progress throttle is ~2/s, not ~10/s |
| Folder ops | Folder move/delete run in the foreground with progress and a failure report, not as resumable transfer-engine jobs (`CLAUDE.md` §5.3) |
| Upload | No "Upload folder" action |
| Sync | No isolate, progress, or cancel on scan; no MD5 verification; no `.nomedia`; ignore list not editable |
| Sharing | No per-link/custom TTL, native share sheet, public-link option, or rotate-credentials guidance |
| Auth | Change login/destruction password and biometric re-wrap exist in the stores but aren't exposed; the strength meter is a heuristic, not zxcvbn; no launch-time key-invalidation probe |
| Encryption | Tasks retry whole (no mid-object resume) |

---

## 19. Appendix — inventories & constants

### 19.1 Persistent data inventory

| Item | Path / name | Secret? | Wipe | Survives uninstall |
|---|---|---|---|---|
| Hardware KEK | `r2vault_kek_bio` / `r2vault.kek.bio` | Key | Step 1 | No |
| Secure-storage entries | §19.2 | Mixed | Step 2 | No |
| App DB | `<app-support>/r2vault.sqlite` (+`-wal`, `-shm`) | No (paths, keys, timestamps) | Step 3 | No |
| Encryption state | `<app-support>/encryption_state.json` | No (object keys + epochs) | Step 4 | No |
| Thumbnails | `<app-support>/thumbs/xx/<sha256>.jpg` | Content likeness | Step 4 | No |
| Cache DB (default) | `<app-support>/…` | No (listing) | Step 4 (incidental) | No |
| Cache DB (custom) | `<chosen dir>/…` | No (listing) | **Never** | **Yes** |
| Crypto temps | `<temp>/enc_work`, `enc_preview`, `enc_upload` | Plaintext/ciphertext | Step 4 | No |
| Private downloads | app-private documents dir | Content | Step 4 (mobile only) | No |
| Custom downloads | `<chosen dir>` | Content | Never | Yes |

### 19.2 Secure-storage keys

| Key | Content |
|---|---|
| `credential_blob` | Sealed R2 config (DEK, AAD `r2vault.v1.credentials`) |
| `dek_wrap_bio` | DEK wrapped by the hardware KEK |
| `dek_wrap_pwd`, `dek_wrap_pwd_salt` | DEK wrapped by `KEK_pwd` (AAD `r2vault.v1.dek.pwd`) + Argon2id salt |
| `destruction_hash`, `destruction_salt` | Argon2id hash + independent salt |
| `wipe_pending` | Wipe tombstone |
| `passwords_set` | Onboarding flag |
| `enc_master_blob`, `enc_master_pending` | File master key(s) under the DEK (AAD `r2vault.v1.file.master`) |
| `enc_master_kdf_salt`, `enc_master_kdf_params` (+ `_pending_` variants) | Non-secret KDF info for v2 embedding |
| `enc_external_keys` | JSON `{ kdfSaltId: sealedKey }` under the file master key |
| Preference keys | §13 |

### 19.3 Constants

| Constant | Value |
|---|---|
| Argon2id (login, destruction, file master) | 64 MiB, t=3, p=4, 32-byte output, 16-byte salt |
| AES-GCM nonce / tag | 12 B / 16 B |
| File chunk size | 64 KiB |
| File nonce prefix | 7 B |
| Magic probe (scan) | 128 B, 6 concurrent |
| Key-header probe | 512 B |
| Single-PUT threshold | 8 MiB |
| Multipart part size | 8 MiB default, scaled to < 1000 parts, min 5 MiB |
| Upload stream slice | 64 KiB |
| Batch delete | 1000 keys/call |
| List page | 1000 keys |
| Presign max TTL | 604800 s (7 d) |
| Playback presign TTL | 6 h |
| Share TTL presets | 900 / 3600 (default) / 86400 / 604800 s |
| Auto-lock options | 30 / 60 / 120 (default) / 300 / 600 s |
| Transfer concurrency | default 3, options 1–5, clamp 1–8 |
| Encryption concurrency | default 2 |
| Retry | 5 attempts, base 500 ms, cap 30 s, full jitter |
| Progress throttle | transfers 500 ms; encryption 100 ms |
| Text preview cap | 2 MiB |
| Thumbnail | 144 px max edge, JPEG q80 |
| Typed-delete threshold | > 50 objects |
| Password minimum | 10 characters |
