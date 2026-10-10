# Changelog

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
