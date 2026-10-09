# Changelog

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
