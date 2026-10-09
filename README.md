# KeyGrant for Rust and Tauri

The `keygrant` crate licenses Tauri and native Rust desktop apps with [KeyGrant](https://keygrant.dev):
signed leases verified offline, activation, trials and renewals, failing in favour of the paying customer.

```sh
cargo add keygrant
```

How to use it is in [sdk-rust/README.md](sdk-rust/README.md), and the full guide is at
https://keygrant.dev/docs.

## Building from source

```sh
cd sdk-rust
cargo test
```

The tests reproduce `conformance/vectors.json`, the behaviour every KeyGrant SDK shares.

## Releases

Each release is a version tag (`0.2.0`). The tag runs `.github/workflows/publish.yml`, which tests and
packages the crate and publishes it to crates.io through crates.io's trusted publishing.

## License

MIT: see [LICENSE](LICENSE).
