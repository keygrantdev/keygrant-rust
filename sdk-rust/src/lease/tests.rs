//! The lease module's own tests: base64, verification, and the payload's bytes.

use super::*;

#[test]
fn decodes_as_atob_does() {
    assert_eq!(b64url_decode("QQ"), Some(b"A".to_vec()));
    assert_eq!(b64url_decode("QQ=="), Some(b"A".to_vec()));
    assert_eq!(b64url_decode("QQ="), Some(b"A".to_vec()));
    // Leftover bits are discarded, not refused.
    assert_eq!(b64url_decode("QR"), Some(b"A".to_vec()));
    assert_eq!(b64url_decode("Q"), None);
    assert_eq!(b64url_decode("Q="), None);
    assert_eq!(b64url_decode("!!!"), None);
    assert_eq!(b64url_decode("Q=Q="), None);
    assert_eq!(b64url_decode("-_-_"), Some(vec![0xfb, 0xff, 0xbf]));
    assert_eq!(b64url_decode("+/+/"), Some(vec![0xfb, 0xff, 0xbf]));
    assert_eq!(b64url_decode(""), Some(vec![]));
}

#[test]
fn decodes_a_tokens_parts_only_when_canonical() {
    assert_eq!(b64url_decode_canonical("QQ"), Some(b"A".to_vec()));
    assert_eq!(
        b64url_decode_canonical("-_-_"),
        Some(vec![0xfb, 0xff, 0xbf])
    );
    assert_eq!(b64url_decode_canonical(""), Some(vec![]));
    // Padding, the standard alphabet, white space and stray bits each give
    // a second spelling of the same bytes: refused.
    for other in ["QQ==", "QQ=", "+/+/", "Q Q", "QR", "Q", "!!!"] {
        assert_eq!(b64url_decode_canonical(other), None, "{other:?}");
    }
}

#[test]
fn a_signature_with_stray_bits_is_malformed_and_a_payload_bad_claims() {
    use ed25519_dalek::{Signer, SigningKey};
    let signing = SigningKey::from_bytes(&[3; 32]);
    let key =
        KeySet::from(PublicKey::from_bytes(signing.verifying_key().as_bytes()).expect("a key"));
    let header = b64url_encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
    let payload = b64url_encode(
        br#"{"iss":"keygrant","sub":"K","product":"p","aid":"a","model":"trial","lim":1,"iat":0,"exp":9999999999}"#,
    );
    let input = format!("{header}.{payload}");
    let signature = b64url_encode(&signing.sign(input.as_bytes()).to_bytes());
    assert!(verify_lease(&format!("{input}.{signature}"), &key, 0).is_valid());
    // The last of 86 characters holds 2 bits and 4 that must be zero, so a
    // canonical one is A, Q, g or w: `B` has stray bits (malformed), and
    // another of the four is canonical but not the signature.
    let (body, last) = signature.split_at(signature.len() - 1);
    assert!(["A", "Q", "g", "w"].contains(&last), "{last}");
    let other = if last == "A" { "Q" } else { "A" };
    let with_last = |c: &str| verify_lease(&format!("{input}.{body}{c}"), &key, 0).failure();
    assert_eq!(with_last("B"), Some(VerifyFailure::Malformed));
    assert_eq!(with_last(other), Some(VerifyFailure::BadSignature));
    assert_eq!(
        verify_lease(&format!("{input}.{signature}="), &key, 0).failure(),
        Some(VerifyFailure::Malformed)
    );
    // A payload written with padding was not signed as the server writes it.
    let padded = format!("{header}.{payload}=");
    let padded_signature = b64url_encode(&signing.sign(padded.as_bytes()).to_bytes());
    assert_eq!(
        verify_lease(&format!("{padded}.{padded_signature}"), &key, 0).failure(),
        Some(VerifyFailure::BadClaims)
    );
}

#[test]
fn encodes_round_trip() {
    for len in 0..40u8 {
        let bytes: Vec<u8> = (0..len)
            .map(|i| i.wrapping_mul(37).wrapping_add(11))
            .collect();
        assert_eq!(b64url_decode(&b64url_encode(&bytes)), Some(bytes));
    }
}

#[test]
fn a_payload_with_a_byte_order_mark_reads_as_the_reference_reads_it() {
    use ed25519_dalek::{Signer, SigningKey};
    let signing = SigningKey::from_bytes(&[5; 32]);
    let key =
        KeySet::from(PublicKey::from_bytes(signing.verifying_key().as_bytes()).expect("a key"));
    let claims = r#"{"iss":"keygrant","sub":"K","product":"p","aid":"a","model":"perpetual","lim":1,"iat":0,"exp":9999999999}"#;
    let token = |payload: &str| {
        let input = format!(
            "{}.{}",
            b64url_encode(b"{}"),
            b64url_encode(payload.as_bytes())
        );
        let signature = b64url_encode(&signing.sign(input.as_bytes()).to_bytes());
        format!("{input}.{signature}")
    };
    // TextDecoder drops one leading byte-order mark: valid, as in the reference.
    assert!(verify_lease(&token(&format!("\u{FEFF}{claims}")), &key, 0).is_valid());
    // A second is text JSON.parse refuses.
    assert_eq!(
        verify_lease(&token(&format!("\u{FEFF}\u{FEFF}{claims}")), &key, 0).failure(),
        Some(VerifyFailure::BadClaims)
    );
}
