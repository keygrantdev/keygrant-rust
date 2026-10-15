# Changelog

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
