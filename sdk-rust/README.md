# keygrant (Rust)

KeyGrant licensing for **Tauri** and native **Rust** desktop apps.

A licence is proven to the app by a signed JWT lease (JWS, EdDSA / Ed25519)
that the app verifies **offline** against the product's public key. The app
only phones home to activate, to renew a lease near or past its end, to tell
the server about a newer major version, or to report a running trial's launch
(at most once a day).

**Licensing failure fails in favour of the paying customer.** An outage, no
network, a timeout, a 5xx, a 429, an ambiguous 4xx or a body the SDK cannot use
never locks out a valid lease. Only the server's explicit verdicts (revoked,
upgrade required, update required, another device) and the lease's own end do.

This crate is a port of the Electron SDK (`@keygrant/electron`),
which is the reference, and behaves the same way:

- the same wire protocol;
- the same state file, in format and default location, so an app moving from
  Electron to Tauri keeps its licence;
- the same fingerprint for the same machine;
- the same answers to the shared conformance vectors (`conformance/vectors.json`).

The full guide is at https://keygrant.dev/docs.

## Install

```toml
[dependencies]
keygrant = "0.6"
```

or `cargo add keygrant`.

It needs Rust 1.86 or later. Its dependencies are `ed25519-dalek`, `sha2`,
`serde`/`serde_json` (with `float_roundtrip`, so a number in a lease is read
as the same double as JavaScript reads it; Cargo turns the feature on for
the app's own `serde_json` too) and `ureq` 3 (rustls); `winreg` and `whoami` on Windows,
`hostname` elsewhere. The crate itself forbids `unsafe`.

## Embedding the public keys

The product's **public** keys are the `keys` of
`GET https://api.keygrant.dev/v1/products/<slug>/pubkey` (public, no API key),
and the dashboard's Signing key tab copies the same set (Copy key set). Save
it next to your app when you build and embed it:

```rust
const PUBLIC_KEYS: &str = include_str!("../keygrant-keys.json"); // {"keys":[...]}
```

`public_jwk` takes the key set (`{"keys":[...]}`, or the whole `/pubkey`
answer), a JSON list of JWKs, or one key, a set of one: its JWK or its `x`
member alone, as before. A JWK that carries the private key (`d`) is refused:
an app that ships it lets anyone forge leases.

**Why a set.** A lease names the key that signed it (`kid` in its header, the
key's RFC 7638 thumbprint, which the SDK computes from the key itself). It is
verified by that key; a lease that names none (every lease signed before key
sets) by each key in turn; and one naming a key the build does not carry is
refused (`UnknownKey`). When the product rotates its signing key, it first
*stages* the next key, published beside the current one while every lease is
still signed with the current key. Ship builds carrying both; once they are
out, the product *switches*, and those builds verify every lease signed after
it. Embed the whole set each time you build.

Each activation and validate reports the ids of the keys the build
carries (`kids`), so the dashboard can count the devices still on builds
without the staged key before you switch.

## Plain Rust

```rust
use keygrant::{License, LicenseConfig, LicenseState};

const PUBLIC_KEYS: &str = include_str!("../keygrant-keys.json");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let license = License::new(
        LicenseConfig::new("sluice", "https://api.keygrant.dev", PUBLIC_KEYS)
            // The running app's major version. Set it: an unbound key is bound
            // to the major its first activation runs. A 0.x app (major 0) runs
            // as major 1, as the server reads its version.
            .major(env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(1))
            .device_name("Rae's MacBook Pro"),
    )?;

    // At launch: offline-first, and never an error. A running trial reports
    // its launch from here, at most once a day, in the background: there is
    // no need to call start_trial() on every launch.
    let status = license.status(false);
    if status.permits_use() {
        // Run the app. `status.test` is true on a TEST licence.
    } else {
        // Show the activation screen; when the customer enters a key:
        match license.activate("SLUICE-AAAA-BBBB-CCCC-DDDD") {
            Ok(()) => println!("activated"),
            Err(error) => println!("could not activate: {}", error.code()),
        }
    }

    // Free this device's seat:
    if !license.deactivate()?.released {
        // KeyGrant could not confirm it: the seat may still be held. Say so
        // (the developer's dashboard, or support, can release it).
        println!("this device's seat may still be in use");
    }
    Ok(())
}
```

## Tauri

Every method blocks: on the network (always bounded), and on the disk. In a
Tauri command, run it on the blocking pool with
`tauri::async_runtime::spawn_blocking`, never on the main thread. `License` is
`Send + Sync`: create one at start-up and manage it as `Arc<License>`. Its
calls run one at a time however many commands call it.

```rust
use std::sync::Arc;

use keygrant::{License, LicenseConfig, LicenseStatus};

const PUBLIC_KEYS: &str = include_str!("../keygrant-keys.json");

#[tauri::command]
async fn license_status(
    license: tauri::State<'_, Arc<License>>,
    force_online: bool,
) -> Result<LicenseStatus, String> {
    let license = Arc::clone(&license);
    tauri::async_runtime::spawn_blocking(move || license.status(force_online))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn activate(license: tauri::State<'_, Arc<License>>, key: String) -> Result<(), String> {
    let license = Arc::clone(&license);
    tauri::async_runtime::spawn_blocking(move || license.activate(&key))
        .await
        .map_err(|e| e.to_string())?
        // "invalid-key", "limit", "revoked", "upgrade", "outdated", "network",
        // "invalid-lease", the seven words of a user-locked key (see below)
        // or "storage": the same words as the Electron SDK.
        .map_err(|e| e.code().to_owned())
}

#[tauri::command]
async fn start_trial(license: tauri::State<'_, Arc<License>>) -> Result<LicenseStatus, String> {
    let license = Arc::clone(&license);
    tauri::async_runtime::spawn_blocking(move || license.start_trial())
        .await
        .map_err(|e| e.to_string())
}

fn main() {
    let license = License::new(
        LicenseConfig::new("sluice", "https://api.keygrant.dev", PUBLIC_KEYS)
            .major(env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(1)),
    )
    .expect("the licence configuration is the developer's to fix");

    tauri::Builder::default()
        .manage(Arc::new(license))
        .invoke_handler(tauri::generate_handler![license_status, activate, start_trial])
        .run(tauri::generate_context!())
        .expect("error while running the app");
}
```

`LicenseStatus` serialises exactly as the Electron SDK's (camelCase, absent
fields left out, `test` and `stalled` only when true, `entitlements` only when
there are any, `source` and `licensedTo` only from a licence file), so a front end written for the Electron SDK reads it
unchanged:

```json
{ "state": "trial", "trialEndsAt": 1790432000000, "trialDaysLeft": 5 }
```

`DeactivateResult` serialises as the Electron SDK's (`{"released": true}`),
`ImportedLicenseFile` as its `{"kind": ..., "licensedTo": ...}`, and
`ActivateError`, `ImportError` and `Error` implement `Serialize` as their word.

## Licence files: drag and drop

A licence can come as a file (`.kglicense`) the customer drops onto the app or
opens from its licence screen. A **key file** carries their key: importing it
activates the key online, exactly as typing it would. An **offline licence
file** licenses the app with no network at all, for good, on machines that
never connect; it has no seat limit and cannot be revoked, so the only way to
stop one that leaked is an app update that lists its id:

```rust
use std::sync::Arc;

use std::io::Read;
use std::path::Path;

use keygrant::license_file::MAX_LICENSE_FILE_BYTES;
use keygrant::{ImportedLicenseFile, License, LicenseConfig, LICENSE_FILE_EXTENSION};
use tauri::{DragDropEvent, Emitter, WindowEvent};

const PUBLIC_KEYS: &str = include_str!("../keygrant-keys.json");

// The front end hands over a file's bytes: from a drop on a web view, or an
// <input type="file" accept=".kglicense">, as Array.from(new Uint8Array(await file.arrayBuffer())).
#[tauri::command]
async fn import_license_file(
    license: tauri::State<'_, Arc<License>>,
    bytes: Vec<u8>,
) -> Result<ImportedLicenseFile, String> {
    let license = Arc::clone(&license);
    tauri::async_runtime::spawn_blocking(move || license.import_license_file(bytes))
        .await
        .map_err(|e| e.to_string())?
        // "malformed", "not-a-license-file", "bad-signature", "unknown-key",
        // "wrong-product", "revoked-file", "upgrade", "outdated"; for a key file
        // what activate() answers; "storage" or "busy": try again.
        .map_err(|e| e.code().to_owned())
}

/// A file's first `limit` bytes at most.
fn read_at_most(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(limit as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn main() {
    let license = Arc::new(
        License::new(
            LicenseConfig::new("sluice", "https://api.keygrant.dev", PUBLIC_KEYS)
                .major(3)
                // Ids of files that leaked: an update adds them here.
                .revoked_license_files(["lf_8Hq2"]),
        )
        .expect("the licence configuration is the developer's to fix"),
    );
    let on_drop = Arc::clone(&license);
    tauri::Builder::default()
        .manage(license)
        .invoke_handler(tauri::generate_handler![import_license_file])
        .on_window_event(move |window, event| {
            // A file dropped on the window, read here: the SDK takes content, never a path.
            if let WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) = event {
                let dropped = paths.iter().find(|path| {
                    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                        e.eq_ignore_ascii_case(LICENSE_FILE_EXTENSION.trim_start_matches('.'))
                    })
                });
                if let Some(path) = dropped.cloned() {
                    let license = Arc::clone(&on_drop);
                    let window = window.clone();
                    tauri::async_runtime::spawn_blocking(move || {
                        // Never more than a licence file can be: a larger one is refused unread.
                        let imported = read_at_most(&path, MAX_LICENSE_FILE_BYTES + 1)
                            .map_err(|e| e.to_string())
                            .and_then(|bytes| {
                                license.import_license_file(bytes).map_err(|e| e.code().to_owned())
                            });
                        let _ = window.emit("license-file", imported);
                    });
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running the app");
}
```

In plain Rust, read the file and hand over its bytes:
`license.import_license_file(std::fs::read(path)?)`.

`import_license_file` takes the file's content (`&str`, `String`, `&[u8]` or
`Vec<u8>`), never its path, and verifies it offline: signed by a key of
`public_jwk`, for this product, and, for an offline file, covering this build's
major. It answers `Ok(ImportedLicenseFile { kind, licensed_to })` (`kind` is
`LicenseFileKind::Key` or `Offline`), or `Err(ImportError::File(_))` with one
of the `LicenseFileError`s `Malformed`, `NotALicenseFile`, `BadSignature`,
`UnknownKey`, `WrongProduct`, `RevokedFile`, `Upgrade`, `Outdated`, nothing
stored. A key file's activation answers as `activate()` does
(`Err(ImportError::License(_))`). `Err(ImportError::Busy)` when a licence call
before it has not finished, and `Err(ImportError::Storage(_))` when the state
cannot be read or the file cannot be saved: try again. `UnknownKey` means the
file was signed by a key this build does not carry: a newer one, after the
product rotated its key, which an update of the app carries. A key file's notes
print its key, which can be typed meanwhile.

An offline file is kept and checked again on every `status()`, which then
answers `Licensed` with `source: Some(LicenseSource::File)`, the key,
`licensed_to` and the file's entitlements, never asking the server,
`refresh()` included. Show "Licensed to Ada Lovelace" from `licensed_to`. A
status from a file has no seat to deactivate: offer `remove_license_file()`
instead (`deactivate()` leaves the file alone). If the device also holds a
key, a file that covers this build wins, and the key keeps working once the
file is removed, revoked or outgrown. A file that does not cover this build
answers only when no key and no running trial does.

**Keep every key that signed a file in your builds.** A file names the key that
signed it and never expires, so a key that has signed one stays in the
product's key set, and in `public_jwk`, after it stops signing leases. A build
without it reads such a file as `Invalid` / `UnknownKey`.

## User-locked keys

A licence model can let only the person a key was sold to activate a NEW
device on it: the activation names the address on the key and, for a key that
mails a code to that address, the code too. Only a new device is ever asked;
`status()`, `refresh()`, a renewal, a seat the key already holds and everything
offline are as they were, so an app that is activated keeps running through a
KeyGrant or mail outage.

`activate_with` carries what the person typed (`ActivateOptions`; each member
is optional, and neither is ever stored):

```rust
use keygrant::{ActivateOptions, LicenseError};

let options = ActivateOptions::new().email(&entered_email).code(&entered_code);
if let Err(error) = license.activate_with(&entered_key, &options) {
    match error.license_error() {
        // Ask for the address, then activate again with it.
        Some(LicenseError::UserEmailRequired | LicenseError::UserEmailMismatch) => {}
        // A code was just mailed to the address: ask for it.
        Some(LicenseError::UserCodeRequired | LicenseError::UserCodeInvalid) => {}
        Some(LicenseError::TooManyCodes | LicenseError::TooManyAttempts) => {}
        Some(LicenseError::TryAgain) => {}
        _ => {}
    }
}
```

| Error (its word) | Means | The app |
|---|---|---|
| `UserEmailRequired` (`user-email-required`) | the key is locked and no address came | ask for the address |
| `UserEmailMismatch` (`user-email-mismatch`) | not the key's address (nothing is said of the real one) | ask again |
| `UserCodeRequired` (`user-code-required`) | the address was right and a code was just mailed to it | ask for the code |
| `UserCodeInvalid` (`user-code-invalid`) | the code is wrong, expired (10 minutes) or used, or the key's codes are void for the hour after five wrong ones | ask again, or activate with the address alone for a new code |
| `TooManyCodes` (`too-many-codes`) | three codes were mailed for the key this hour | wait, or use the code sent |
| `TooManyAttempts` (`too-many-attempts`) | ten wrong addresses this hour, or the key's codes void | try again later |
| `TryAgain` (`try-again`) | the code could not be mailed: nothing was activated | retry; the one error here that is nobody's mistake |

The address is trimmed (it is compared without regard to case) and a blank is
none. The code has its ends trimmed and its spaces dropped (`123 456` is
`123456`), and must be exactly six ASCII digits. An address longer than 254
UTF-16 units, or a code that is not six digits, answers `UserEmailMismatch` or
`UserCodeInvalid` at once, for any key, asking the server nothing and so
costing the key none of its wrong tries; the key is judged first, then the
address, then the code. A key that is not locked ignores both once they reach
the server. `import_license_file_with(content, &options)` passes them on to a
key file's key. `deactivate()`, `refresh()` and `start_trial()` take neither.

## Buying from the app

Give a customer who holds no key a Buy button that opens your Stripe Payment
Link (with the app's own way to open a URL: tauri-plugin-opener, the `open`
crate, ...). The checkout emails them their key, and they type it into the
app:

```rust
// Once they have the key from the email:
license.activate(&entered_key)?;
```

For a customer who already holds a key, `upgrade_url(link)` adds this
install's activation to the link, so an upgrade sale finds the key it
upgrades.

## The API

| Call | What it does |
|---|---|
| `License::new(config)` | Checks the configuration (product slug, `http(s)` API base, public key, a home folder for the default storage). Reads and asks nothing. |
| `status(force_online)` | The licence's status. Offline-first; asks the server only when it should (below). Never fails: storage that cannot be read just now is `Unknown`. |
| `refresh()` | `status(true)`: asks the server at once, whatever the waits say. Call it when the customer says they have paid. |
| `activate(key)` | Activates the key on this device. The key is trimmed and upper-cased first. `Err(ActivateError::License(_))` or `Err(ActivateError::Storage(_))`. |
| `activate_with(key, &options)` | `activate`, with the address and code a person typed for a user-locked key (`ActivateOptions`). See [User-locked keys](#user-locked-keys). |
| `deactivate()` | Releases the seat and drops the local licence, keeping the trial evidence. `Ok(DeactivateResult { released })`: `released` is true when KeyGrant answered the release with a 200, or when there was no activation to release; false when it could not be asked or answered anything else (a 5xx, a 429, a proxy's page), so the seat may still be held and the app should say so. The local licence is dropped either way. An activation that is another device's (a state file copied or restored from another PC: a device refusal that stands for this device, or a lease, expired or not, held to a machine id this device can tell is not its own) is never released, as releasing it would lock that PC out: it is dropped here, and the answer is `released: true`. `Err(Error::Busy)` behind a call that has not finished, and `Err(Error::Storage(_))` when the state cannot be read, release nothing. `Err(Error::Storage(_))` when the state cannot be saved comes after the release was asked for: the seat may already be free while the device still holds the key, so try again (the release is idempotent: the retry answers `released: true` and drops the key). |
| `start_trial()` | Starts (or re-syncs) this device's trial. Never fails; the server's trial is answered even when it cannot be saved. An answer whose trial lease this build cannot verify (signed with a key the product rotated to ahead of this build) never replaces a stored trial lease it can: the stored one is kept, so the running trial does not end. An answer with no lease (the trial is over) is taken as it is. |
| `import_license_file(content)` | Takes a licence file's content, verified offline: an offline file is stored and licenses the app with no network; a key file's key is activated as `activate` would. See [Licence files](#licence-files-drag-and-drop). |
| `import_license_file_with(content, &options)` | `import_license_file`, passing `ActivateOptions` on to a key file's activation. |
| `remove_license_file()` | Drops the stored offline licence file; a key the device holds keeps working. `Err(Error::Busy)` or `Err(Error::Storage(_))`. |
| `upgrade_url(link)` | The payment link with this device's **activation id** (never the key) as `client_reference_id`, so an upgrade checkout extends the key the customer holds. `Ok(None)` without an activation. |

### Configuration

| `LicenseConfig` | Default | |
|---|---|---|
| `product` | (required) | The product slug. |
| `api_base_url` | (required) | `https://api.keygrant.dev`. |
| `public_jwk` | (required) | The product's key set (`{"keys":[...]}`), a list of JWKs, one JWK, or its `x` (`KeySet::parse`). |
| `major` | `1` | The running app's major version. `0` (a 0.x app) runs as `1`, as the server reads a version. |
| `device_name` | none | A label for the device list; cut to 120 UTF-16 units. |
| `device_hold` | `true` | See below. |
| `http_timeout` | 15 s | The bound on every request that names no shorter one. Kept by the SDK whatever the HTTP adapter does. |
| `revoked_license_files` | none | The ids of licence files this build refuses (an offline file that leaked), matched exactly. |
| `adapters` | defaults | `storage`, `registry`, `clock`, `fingerprint`, `http`. |

Each has a builder method of the same name (`.major(3)`, `.storage(...)`, ...).

### Statuses

| `state` | `reason` | What to do |
|---|---|---|
| `Licensed` | | Run. `test` is true on a TEST licence (refuse those in a release build if you like). With `source: Some(LicenseSource::File)`, from a stored offline licence file: show `licensed_to`. |
| `Trial` | | Run; `trial_days_left` and `trial_ends_at` say how long. A trial runs only while its signed trial lease is valid, to the end that lease was signed to: the end stored in the state file is unsigned, so editing it extends nothing. |
| `Expired` | `Offline` | The lease ran out and the server could not be reached. Ask the customer to go online; `refresh()`. |
| `Expired` | `Trial` | The trial has ended (or there is no valid trial lease to run it on): offer to buy. |
| `Invalid` | `Revoked` | The key was revoked, expired, or this device was released. |
| `Invalid` | `Upgrade` | This build is newer than the key covers: offer the upgrade (`upgrade_url`). From a licence file (`source` `File`): newer than the file covers; the customer needs a file (or a key) for this version. |
| `Invalid` | `Outdated` | This build is older than the key covers: update the app. From a licence file, older than the file covers: update the app. |
| `Invalid` | `Device` | The licence was activated on another device (this machine changed, or the state file was copied): activate again here. |
| `Invalid` | `Tampered` | The stored lease is not for this licence. Activate again. From a licence file, the stored file is not one (the state file was edited): import it again, or remove it. |
| `Invalid` | `UnknownKey` | The stored lease is signed by a key this build does not carry. Online, the server renews it with the key it signs with now; when it stays so online, the build does not carry the product's signing key: update the app. From a licence file, no renewal comes: update the app, or ask for the same licence's file signed again. |
| `Invalid` | `RevokedFile` | The stored offline licence file is one this build lists as revoked (`source` is `File`). |
| `Unlicensed` | | No licence and no trial. |
| `Unknown` | `Storage` | The state file could not be read just now (an antivirus scan holding it). Keep running and ask again shortly; never show the activation screen for it. |

`status.permits_use()` is true for `Licensed`, `Trial` and `Unknown`;
`status.is_entitled()` for `Licensed` and `Trial`. `stalled: true` means an
earlier call on this `License` has not finished (a storage adapter stuck), so
the answer came from what is stored, with no ask and no write.

### When `status()` goes online

Offline-first. It asks the server (a validate) only:

1. on a valid lease, when this build is a newer major than the server last
   heard on an open-ended lease, or when the lease is near its end (less than
   7 days left, or less than half its life if shorter). Bounded to 5 seconds;
   the valid lease is the answer when that gets no verdict;
2. on a lease past its end or otherwise invalid here, to let the server heal it;
3. on a stored refusal that still stands for this build.

And it asks for the trial again (as `start_trial()` does, bounded to 5 seconds)
when the trial's lease does not verify here while the stored end is still ahead:
a lease signed before the product's signing key was rotated, or one signed to
end short of the trial's end. A trial the answer brings back that this build can
verify runs; otherwise the local answer stands (the trial is over for now) and
the wait starts. Never behind a stuck call, and never from `start_trial()`'s own
answer.

Each only once the wait after the last ask is over: 1 minute after an ask
nothing answered (no network, a timeout), 1 hour after one the server
answered without a lease (a refusal, a 5xx, a 429, an unusable body), or
without a trial this build can verify. Any lease ends the wait; a clock set
back asks at once; `refresh()` always asks.

A running trial reports its launch (`trial`, the body
`start_trial()` sends) at most once a day, a `start_trial()` answer counting
as one: its stamp saved first (nothing is sent when it cannot be), then the
request made on a thread of its own, bounded to 5 seconds, never waited on,
its answer not read and every failure ignored.

### One call at a time

Calls on one `License` run one after another, so `status()` from several
threads asks the server once and never writes back a stale state. A call waits
for the one before it, but at most `http_timeout` + 15 s from when that one
started. Storage adapters are not cut short (a write cut short could be left
half-written), so a call stuck on one is left to finish, and the calls behind
it run read-only until it does: `status()` answers from storage with
`stalled: true`, `activate()` answers `network`, `start_trial()` the stored
trial, and `deactivate()` fails with `Error::Busy`.

## Holding a licence to its device: `device_hold`

The default fingerprint is the operating system's machine id, hashed so it
never leaves the machine: `MachineGuid` (Windows, 64-bit registry view),
`IOPlatformUUID` (macOS, `/usr/sbin/ioreg`), `/etc/machine-id` or
`/var/lib/dbus/machine-id` (Linux; an id on a tmpfs/ramfs, or written at this
boot, counts as none). With no id to read, the host name, platform and
architecture are hashed instead. A read that does not finish in time gives no
fingerprint: the device is not judged that time, and the next call reads again.

A lease whose seat is keyed by a machine id is held to that machine: a licence
file copied to another machine is refused (`Device`), by the app and by the
server. An honest device is never refused for how it is identified: host-name
seats are never held, and a run that cannot tell who the device is never judges.

Set `.device_hold(false)` for a product deployed on non-persistent, pooled VDI
with roaming profiles, where every session may be another machine: the SDK
then reports no kind for its fingerprint, judges no lease by its device and
sends the server no fingerprints. The device limit still counts seats.

## Test licences

A TEST licence (made from the dashboard, with a `kg_test_` API key, or by a
Stripe test-mode checkout) licenses the app like any other and lapses on its
own within 30 days. `status.test` is true for one; a release build that wants
nothing to do with them refuses them:

```rust
let status = license.status(false);
if status.test && !cfg!(debug_assertions) {
    // treat as unlicensed
}
```

## Entitlements

A licence can grant more than the base product: an edition, a number of seats,
an add-on. The lease signs them (its `ent` claim), and the status hands them to
the app as `entitlements`, read offline like everything else on the lease:

```rust
use keygrant::Entitlement;

let status = license.status(false);
let pro = status.entitlements.get("edition").and_then(Entitlement::as_str) == Some("pro");
let seats = status.entitlements.get("seats").and_then(Entitlement::as_f64).unwrap_or(1.0);
```

- **Type:** `BTreeMap<String, Entitlement>`. An `Entitlement` is
  `Bool(bool)`, `Number(f64)` or `Text(String)`, read with `as_bool()`,
  `as_f64()` and `as_str()`. A number is a double, read correctly rounded (the
  nearest double to what the lease writes, as JavaScript reads it):
  `5` and `5.0` are the same entitlement, and compare equal. It serialises as
  the bare JSON value, a
  whole number without a fraction as JavaScript writes it
  (`"entitlements": {"edition": "pro", "seats": 5}`).
- **When it holds them:** on a `Licensed` status (from the licence's lease:
  after an activation, a renewal or a `refresh()`, straight from the device,
  or behind a call that has not finished) and on a `Trial` status (from the
  trial's lease), when that lease grants at least one.
- **When it is empty:** on every other status (`Expired`, `Invalid` for any
  reason, a stored refusal included, `Unlicensed`, `Unknown`), and when the
  lease grants none. Then the JSON leaves `entitlements` out, so a lookup
  reads it either way.
- **A malformed claim never costs the licence:** a member that is not a flag,
  a number or a text is left out, and an `ent` that is not an object at all
  reads as none. The SDK enforces none of the server's caps on them (how many,
  how long): the signature is what it trusts.
- **A copy:** the status owns its map, so changing it changes nothing the next
  status says.

To ask whether the licence grants an entitlement, name its code:

```rust
if license.has("cloud_sync") {
    enable_cloud_sync();
}
license.require("export")?; // Err(MissingEntitlement { entitlement: "export" })
```

`has(code)` is true for a flag that is on, a number that is not zero, or a text
that is not empty; false for anything else, and for a code the lease does not
grant (matched exactly, case included). It is answered offline from the stored
lease only, as `status()` would answer from what is stored, with no request and
no write: a licence that does not license this build, or none at all, grants
nothing; a running trial grants what its trial lease does. `require(code)` is
`Err(MissingEntitlement)`, naming the code, when `has` would be false.
`keygrant::grants(&status.entitlements, code)` answers the same of a status
already in hand.

## Where the state is kept

Unless you give your own `storage` adapter, one `<product>.json` per product,
in a folder of this user's on this machine (never the working directory, never
a roaming folder):

- Windows: `%LOCALAPPDATA%\KeyGrant`, else `~\AppData\Local\KeyGrant`
- macOS: `~/Library/Application Support/KeyGrant`
- Linux and others: `$XDG_CONFIG_HOME/keygrant`, else `~/.config/keygrant`

That is the Electron SDK's default too, and the file format is the same, field
for field. Writes go to a temp file beside it, flushed, then renamed over it (a
rename Windows refuses as busy is tried again for about 1.5 s); on macOS and
Linux the folder is flushed after. Only a missing file is "no state", and then
the newest whole temp file beside it is read first. Any other read failure is
tried again for about a second and then fails (`status()` answers `Unknown`). A
file that does not parse is set aside as `<product>.json.<ms>.corrupt` (the
newest three kept) and an empty state written in its place. Each save stamps a
revision (`rev`) and checks it again just before the rename, so two instances of
the app never lose each other's fields; an older answer from the server never
replaces a newer one. A state file in the legacy location,
`<working directory>/.keygrant/<product>.json`, is copied over once.

The home folder and the variables are used as the system gives them, a name
that is not valid Unicode included (the reference decodes it lossily, which
names another folder). Only a machine with no home folder at all is a
`ConfigError` (`StorageLocation`), and then only when no `storage` is given.

`FileStorage::with_options(path, FileStorageOptions { migrate_from, .. })` puts
the state elsewhere, migrating it from an old path.

## Adapters

Every platform effect goes through a trait, all `Send + Sync`:

- `StorageAdapter` (`read(read_only)`, `write`, optional `update(merge)`):
  default `FileStorage`.
- `RegistryStash` (`read_trial_start`, `write_trial_start`): an
  uninstall-surviving trial start. Default `NoopRegistry`; on Windows,
  `WindowsRegistryStash::new(product)` keeps it under
  `HKCU\Software\KeyGrant\<product>`, value `TrialStart`, in the format
  every KeyGrant SDK keeps, so a trial another one stashed for the product is
  seen: ms since the epoch, written as a `REG_QWORD`, read from a `REG_QWORD`
  or a `REG_SZ` of decimal digits. Anything else (another type, 0 or less,
  2^53 or more) is no evidence, and a start of that kind is not written.
- `Clock` (`now` in ms): default `SystemClock`.
- `Fingerprinter` (`get`, optional `identify`): default `DefaultFingerprinter`.
  A fingerprinter with only `get()` says nothing of what it hashes, so its seats
  are never held to their device; implement `identify` and report
  `DeviceKind::Machine` for one that should be.
- `HttpAdapter` (`post(url, body, timeout)`): default `UreqHttp` (rustls with
  the Mozilla root store). `UreqHttp::new()` goes through the HTTP proxy the
  environment names: the first of `ALL_PROXY`, `HTTPS_PROXY` and `HTTP_PROXY`
  (each upper case, then lower) that names an `http` or `https` proxy, with
  the hosts `NO_PROXY` (or `no_proxy`) lists going direct. A SOCKS proxy is
  passed over for the next variable, or for none (direct, as the reference's
  `fetch` always goes): the adapter speaks no SOCKS, so a `socks5h` or
  `socks4a` proxy would fail every request (no activation, no renewal) and a
  `socks5` one would go direct past the HTTP proxy beside it. A 4xx or 5xx
  is an answer, never an error. `UreqHttp::with_agent` takes an agent of your
  own (another TLS set-up, the platform verifier); whatever that agent's own
  setting, every request the adapter makes reads an HTTP status as an answer
  (ureq's default would make every refusal "no network").

**Adapters must not panic.** Where panics unwind (Rust's default), the SDK
catches an adapter's panic: a storage adapter's as a state that cannot be read
or saved (`status()`, `refresh()` and `start_trial()` answer, `activate()` and
`deactivate()` fail with `Storage`), a stash's as no stash, and an HTTP
adapter's or a fingerprinter's as no answer and "cannot tell". A clock that
panics panics the caller. Under `panic = "abort"`, which Tauri's guide to
smaller binaries recommends, any panic, an adapter's included, ends the
process at once: return errors instead.

## Tests

```sh
cargo test
```

`tests/conformance.rs` loads `../conformance/vectors.json` and reproduces every
answer in it but the `majors` vectors a `u32` major cannot hold: those marked
`jsOnly` (a fractional major) and the negative major (the harness checks that it
skips only those); a group in the file that the harness does not answer fails
the run. Each answer comes from the function the SDK itself decides with, not a
copy of it: what a validate's answer saves and says (`renewal_outcome`) and
whether `status()` asks while it holds a valid lease (`asks_on_valid_lease`),
who the device is (`device_from`) and who the default fingerprinter says it is
(`identity_of`), what a save writes as the time last seen (`saved_last_seen`),
the status a stored licence lease or trial gives with no verdict from the
server, entitlements included (`lease_status`, `trial_lease_of` and
`trial_status_of`), and the double an entitlement number reads as
(`lease::entitlements_of`), as well as the lease checks, the patches, the merge,
the clock guard, the wire and the machine-id reads, and the groups after them
(`major_of`, `referenced_link` and `trial_report_due`, answered in
`tests/conformance/later.rs`; the key sets, in `tests/conformance/keys.rs`;
and the licence files, `license_file_token`, `verify_license_file`,
`check_license_file` and `license_file_status`, in
`tests/conformance/files.rs`). A
status's entitlement numbers are compared as numbers, as the file says (`5` and
`5.0` alike), and exactly: the `entitlementNumbers` group pins the bits each
number written as the server writes it must read as, and the harness checks
first that every number in the file reads as the nearest double, as the
reference reads it, so a reader rounding otherwise cannot misread the vectors
and the leases alike and agree with itself.
`KEYGRANT_VECTORS=<path> cargo test --test conformance` checks the port against
another set (vectors just regenerated from the reference). The other suites
drive the public API against fake adapters (a fake clock, in-memory storage and
file system, a scripted server and fixed fingerprinters), with the time bounds
shortened where a test waits them out.

A lease's signature and payload must be canonical base64url (no padding, the
URL-safe alphabet, no stray bits), as the reference requires; a trial lease
must say `model: "trial"`.

## Parity with the reference

The behaviour is the reference's. Where this crate differs, it is for one of
the reasons below.

**Idiomatic Rust, same behaviour.**

- The API blocks instead of returning promises. A call the reference rejects
  returns an `Err`: `activate()` gives `ActivateError::Storage` where the
  reference rejects with the file's error, `deactivate()` gives
  `Error::Storage` or `Error::Busy` ("a previous licence call has not
  finished"), `upgrade_url()` gives `Error::Storage` or `Error::InvalidUrl`,
  and `import_license_file()` gives `ImportError::Busy` or
  `ImportError::Storage` where the reference rejects, and its refusals as
  `ImportError::File` and `ImportError::License` where the reference resolves
  `{ ok: false, error }`.
- Rust has no optional arguments: where the reference takes
  `activate(key, { email, code })` and `importLicenseFile(content, { email, code })`,
  this crate keeps `activate(key)` and `import_license_file(content)` and adds
  `activate_with(key, &options)` and `import_license_file_with(content, &options)`,
  with `ActivateOptions` for the object. The reference also reads a code kept
  as a number; here a code is text, as in every typed port.
- A running trial's launch report is sent on a thread of its own (the
  reference sends it as a promise it never awaits). The thread shares the
  `License`'s adapters, all `Send + Sync`; a test waits for it through a
  hidden method that is not part of the API.
- The SDK's own time bounds are kept by running the HTTP adapter and the
  fingerprinter on a helper thread and abandoning it at the bound, as the
  reference abandons a promise. An abandoned call runs to its end in the
  background. A read of the fingerprinter is shared: a call that comes while
  one runs waits on it, up to its own bound, when that read was given at
  least the call's timeout and is not past its own bound. A call given longer
  (an activation behind a status) reads for itself, and a read past its bound
  is not waited on again: the next call reads anew, as the reference's would.
  A fingerprinter that never returns, which only a custom one can be (the
  default bounds its own reads), so strands a thread for good on every read
  of the device: up to three a `status()`, one to check a lease, two when a
  renewal is due or the lease has run out offline, three when the server
  answers with a lease. At a `status()` every ten minutes that is some 2,000
  threads a week. A custom fingerprinter must return within the timeout it
  is given.
- An adapter that panics is caught where a JavaScript adapter's throw would be
  (see "Adapters must not panic" above): a storage adapter's panic is a
  storage error, a stash's is no stash, an HTTP adapter's is no answer, a
  fingerprinter's is "cannot tell". Only where panics unwind; under
  `panic = "abort"` the process ends.
- `LicenseError` has no `not-configured`: the reference declares it and never
  produces it.

**Better fit for the platform.**

- Windows' `MachineGuid` is read in-process through the registry API
  (`winreg`) instead of by running `reg.exe`: the same value, the same
  fingerprint, without a process to spawn (an endpoint-security tool blocking
  `reg.exe` does not affect it). A value that is not a `REG_SZ` is no id, as
  `reg query`'s output for it holds none. The key is opened with
  `KEY_WOW64_64KEY`, the 64-bit view `reg.exe /reg:64` read: a 32-bit (i686)
  build must keep it, or it reads the 32-bit view, finds no `MachineGuid`, and
  every machine falls back to its host name (a test pins the flag).
- The host name the host-name fingerprint hashes is read as Node reads it. On
  Windows Node calls `GetHostNameW`, whose answer is
  `GetComputerNameExW(ComputerNameDnsHostname)`, read here through `whoami`
  (on a cluster node running under a cluster's network name, both give the
  virtual name, where the "physical" name other crates read would differ).
  Elsewhere both read `gethostname()`.
- On Linux the machine-id read as a whole is bounded rather than each file read
  in it; a hung read of `/proc/self/mountinfo` gives "cannot tell" (the device
  not judged) instead of "not in memory".

**Stricter where the reference fails late or not at all.**

- `License::new` checks its configuration: a public key that cannot be read,
  a JWK holding a private key, a non-`http(s)` API base (trimmed first, as a
  base read from a file often ends in a newline), a product slug that is
  empty or holds a path separator, URL delimiter, white space or control
  character, or no home folder for the default storage is a `ConfigError`. The
  reference takes any of these and fails at the first lease it verifies (every
  `status()` then rejects) or not at all.

**More forgiving where the customer would have paid for it.**

- The default HTTP adapter goes through an HTTP proxy the environment names
  (see Adapters), where the reference's `fetch` reads no proxy and goes
  direct: on a network whose only way out is that proxy, activations and
  renewals still land. A SOCKS proxy is passed over, so it goes direct as the
  reference does.
- The `RegistryStash` methods cannot fail: a stash that cannot be read is no
  evidence, and one that cannot be written loses no trial, as in the reference
  with a stash that throws.
- A stored field of the wrong type reads as absent instead of failing; a
  fractional millisecond is cut to a whole one; a stored JSON value that is not
  an object reads as an empty state. Fields this SDK does not know are kept and
  written back, as the reference's object spread keeps them. So a stored
  `licenseFile` that is not text is no licence file here, where the reference
  reads it as `Invalid` / `Tampered`: either way it licenses nothing.

**Unreachable in practice.**

- A lease claim that is a whole number beyond the 64-bit range is bad claims
  here; the reference's schema accepts any whole double. The server never
  signs one.
- The default fingerprinter reads the host name once a call, for its
  alternate and, on a machine with no id, its fingerprint (`identity_of`, the
  `identities` vectors). The reference reads it a second time for that
  fingerprint, after the machine-id read, so only a rename between the two
  reads would tell them apart.
- A lease payload, and a licence file's, is read as the reference reads it
  (one leading byte-order mark dropped, as `TextDecoder` drops it; bytes that
  are not UTF-8 replaced), then parsed as JSON. Three payloads `JSON.parse` takes and `serde_json` does
  not are bad claims here and may be valid there: a string holding a lone
  UTF-16 surrogate escape (`"\ud800"`), a number too large for a double
  (`1e400`, which `JSON.parse` reads as `Infinity` and the schema allows in
  `ent`), and nesting deeper than 128 levels. The server never signs one: its
  signer refuses an entitlement name or text with a lone surrogate and an
  entitlement number that is not finite, `ent` is flat, and every other claim
  is one the server writes itself.
- `upgrade_url` edits the link without a full WHATWG URL parser: it takes an
  absolute `http`/`https` URL with a host (the reference takes any absolute
  URL), lower-cases the scheme and host, drops a default port, gives an empty
  path `/`, writes the query and fragment percent-encoded as the URL parser
  writes them, and then edits the query as text, as the reference does. It
  does not punycode a host or percent-encode a path, as a Stripe payment link
  never needs either.

**The machine-id vectors.** The `machineIds` group scripts the reference's
reads, `reg.exe` among them. This crate reads `MachineGuid` through the
registry API instead, so the harness maps a scripted `reg.exe` answer onto
what the registry would hold (the value on the `MachineGuid REG_SZ` line, or
the failure) and pins this crate's own step from the value to the id. The Linux
and macOS cases run through the same reader the SDK uses, unchanged.
