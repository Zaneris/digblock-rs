use super::*;

/// The challenge has to match what the master computes, or every sign-in
/// fails at redemption with nothing to see. RFC 7636 Appendix B publishes
/// this exact pair. Pinned once, now that both launchers share the code.
#[test]
fn the_pkce_challenge_matches_the_rfc_test_vector() {
	let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
	assert_eq!(
		s256(verifier),
		"E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
	);
}

#[test]
fn base64url_is_unpadded_and_url_safe() {
	assert_eq!(b64url(b""), "");
	assert_eq!(b64url(b"f"), "Zg");
	assert_eq!(b64url(b"fo"), "Zm8");
	assert_eq!(b64url(b"foo"), "Zm9v");
	assert_eq!(b64url(b"foob"), "Zm9vYg");
	// The two bytes that differ from standard base64: no '+' or '/'.
	assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
	assert!(!b64url(&[0xfb, 0xff, 0xfe]).contains('='));
}

#[test]
fn urlencode_leaves_only_the_unreserved_set() {
	assert_eq!(urlencode("plain-_.~"), "plain-_.~");
	assert_eq!(urlencode("a b"), "a%20b");
	assert_eq!(urlencode("+/=&?%"), "%2B%2F%3D%26%3F%25");
}
