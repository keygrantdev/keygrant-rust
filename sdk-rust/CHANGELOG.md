# Changelog

## 0.7.0

- Metered uses: `License::use_meter(meter, n)` spends `n` uses of a meter the key's licence model counts (1,000
  exports for good, 50 renders each billing period) and answers a `UseResult { ok, left, offline, error }`, with
  `error` a `UseError`: `InvalidAmount`, `NoMeter`, `UsedUp` (with `left`), `OfflineLimit` (with `left`),
  `NotLicensed`, `Storage`, or the server's verdict on the key, the build or the device (`Revoked`, `Upgrade`,
  `Outdated`, `Device`). Online it is exact and atomic. When KeyGrant gives no answer (no network, a timeout, a
  5xx, a 429) the device spends from its offline allowance for the meter, never past what it last heard is
  left, and answers `ok` with `offline` true; past the allowance it is `OfflineLimit`. A spend the allowance
  would refuse asks the server, whatever the last outage left behind, once a minute while it stays out of reach;
  one it covers waits a minute after an outage, doubling to an hour. It panics for no outcome (what no adapter can
  tell it, a panicking storage adapter or clock, is `Storage`), blocks and takes no cancellation, and its request
  waits at most 5 seconds (less when `http_timeout` is shorter). `n` is a `u64`: anything outside 1 to
  1,000,000,000 is `InvalidAmount`, asking nothing. A meter never takes a licence away: nothing in `status()`
  reads one to decide whether the app is licensed. `UseResult` and `UseError` serialise as the Electron SDK's.
- `LicenseStatus::meters`, by meter code: `left` (the newest figure, the lease's or the last `use_meter` answer's,
  net of this device's unsynced spends), `total`, `offline` (this device's allowance, from the same figure),
  `unsynced` and `resets`. Offline, from the lease and the stored figures; on a `Licensed` status from a key's
  lease that names meters only, empty on every other status. The new types are `MeterStatus`, `UseResult`,
  `UseError`, `SpendEntry`, `SpendKind`, `MeterFigure` and `BaseFigure`; a `LicenseStatus` built with a struct
  literal must name `meters`, and `LeaseClaims` has `met`.
- The spends a device made with no answer are kept in the stored state (`StoredState::spends`, with the newest
  figures a `use_meter` answer gave in `meter_figures`), merged across processes as the rest of the state is, and
  reported with the next `validate` (a check-in or renewal), `use` or `deactivate`, which now also names the
  device's fingerprints when it carries a report. They are dropped only when the server says it recorded them; a
  device back online reports at its next `status()`. A journal never fails a renewal or a deactivation. Entries
  not reported within 180 days are dropped, and at most 64 are kept. A spend the SDK could not save (a disk that
  refuses its writes) is kept in memory, counted in `left` and `unsynced`, and reported once it has been saved.
  `StoredState` has `spends` and `meter_figures`, read leniently: a journal that is not a list, or an entry of
  another shape, is read as empty or left out.
- A lease's `met` claim is read leniently, as `ent` is: a member that is not a well-formed meter (a figure missing,
  negative, fractional, past 2^53 - 1 or out of a double's range) is dropped, never the lease. `lease::parse_json`
  reads a lease payload, and every server answer's body, so that a number out of a double's range fails nothing:
  before, an answer holding one (`1e400`) read as no body at all. Such a number is kept as the largest double of its
  sign, so an entitlement holding one now reads as that number where the lease was refused `bad-claims`.
- A spend's id is 16 bytes from the platform's secure random source: the crate depends on `getrandom` again.

## 0.6.0

- User-locked keys: `License::activate_with(key, &ActivateOptions)` and
  `License::import_license_file_with(content, &ActivateOptions)`. A licence model can let only the person a key
  was sold to activate a new device on it, known by the address on the key (`email`), and, for a key that mails
  a code to that address, by the code too. `ActivateOptions` holds both, each optional
  (`ActivateOptions::new().email("...").code("...")`); `activate(key)` and `import_license_file(content)` are as
  they were and send neither. Only a new device is ever asked: `refresh()`, `status()`, a renewal, a seat the key
  already holds and offline behaviour are unchanged. Nothing about the address or the code is stored.
- `LicenseError` has seven more cases, the refusals an app can act on: `UserEmailRequired` (ask for the
  address), `UserEmailMismatch`, `UserCodeRequired` (a code was just mailed: ask for it), `UserCodeInvalid`,
  `TooManyCodes`, `TooManyAttempts` and `TryAgain` (the code could not be sent: nothing was activated, ask
  again). Their words are `user-email-required`, `user-email-mismatch`, `user-code-required`,
  `user-code-invalid`, `too-many-codes`, `too-many-attempts` and `try-again`, so a Tauri front end reads them
  from `ActivateError::code()` as it reads the others. A `match` over every `LicenseError` has seven more cases.
- An address longer than 254 UTF-16 units, or a code that is not exactly six ASCII digits (spaces inside it
  are dropped first), is refused by the SDK itself, for any key, before the server is asked
  (`UserEmailMismatch`, `UserCodeInvalid`). The conformance vectors' two new groups, `typedFor` and
  `userLockErrors`, hold this to the reference.

## 0.5.0

- Licence files: `License::import_license_file(content)` takes a `.kglicense` file's content (`&str`,
  `String`, `&[u8]` or `Vec<u8>`, never a path) and verifies it offline. A key file activates its key, as
  `activate(key)` would. An offline licence file is stored and licenses the app with no network, ever:
  `status()` answers it with `source: Some(LicenseSource::File)` and `licensed_to`, before any key or
  trial, and `refresh()` asks nothing for it. It answers `Ok(ImportedLicenseFile { kind, licensed_to })`
  or an `ImportError` (`File(LicenseFileError)`, `License(LicenseError)`, `Storage`, `Busy`).
  `remove_license_file()` drops it; `deactivate()` leaves it.
- `LicenseConfig::revoked_license_files` lists the ids of files this build refuses: a stored one reads as
  `Invalid` with the new reason `LicenseReason::RevokedFile`. A `match` over every `LicenseReason` has
  one more case to handle.
- `LicenseStatus` has `source` and `licensed_to` (none on every status not from a file), so code that
  builds one with a struct literal must name them; `StoredState` has `license_file`, the stored offline
  file's token.
- `LICENSE_FILE_EXTENSION` (`.kglicense`) for an open dialog's filter, `LICENSE_FILE_MEDIA_TYPE`, and
  the `license_file` module (`license_file_token`, `verify_license_file`, `check_license_file`,
  `license_file_status`), which the conformance vectors' three new groups hold to the reference.

## 0.4.0

- `License::purchase_url` is removed, with `Error::Random` and the `getrandom` dependency, and the SDK no
  longer asks for a bought key at `status()`, `refresh()` or `start_trial()`. To sell from the app, open your
  Payment Link from your own Buy button; the customer types the key from the delivery email (`activate`). A
  purchase secret an earlier version stored is ignored, and left out the next time the SDK saves its state.
- `StoredState` no longer has `purchase_token`, `purchase_token_at` or `claim_asked_at`: a storage adapter or
  test code that sets them stops compiling, and the same names among its `extra` fields are never written.

## 0.3.0

- Key sets: `LicenseConfig::public_jwk` takes the product's key set (`{"keys":[...]}`, as `/pubkey`
  answers it), a list of JWKs, or one key as before (a set of one), and `KeySet` reads them. A lease
  naming its key (`kid`) is verified by that key, one naming none by each key in turn.
- `lease::verify_lease` takes a `&KeySet` (`KeySet::from(public_key)` for one key), and
  `PublicKey::kid` gives a key's id.
- A lease signed by a key the build does not carry is `Invalid` with the new reason
  `LicenseReason::UnknownKey` (`VerifyFailure::UnknownKey`), rather than `Tampered`.
- Each activation, validate and claim sends the ids of the keys the build carries (`kids`).
- `License::has(code)` and `License::require(code)` answer, offline from the stored lease, whether the
  licence grants an entitlement; `require` returns `MissingEntitlement`. `grants` answers it of a
  status's entitlements.
- A public key is read as every KeyGrant SDK reads one: a JWK whose `use` is not "sig", whose `key_ops`
  do not name "verify" or whose `alg` is not "EdDSA" or "Ed25519" is refused, and `x` is canonical
  base64url (trailing padding allowed; no white space, standard alphabet or stray bits). An object whose
  `keys` member is not a list is refused rather than read as one JWK.
- A public key of 32 bytes that are no point on the curve is no longer refused, as every KeyGrant SDK
  reads one: it verifies no lease.
