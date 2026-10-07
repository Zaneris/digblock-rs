use super::*;

/// The exporter and this parser are two programs agreeing on one format, and a line either
/// misreads is a player whose save never comes back. The lines are written the way the
/// exporter writes them: lowercase claim, the master's uppercase GUID.
#[test]
fn an_exported_line_reads_back_as_the_claim_it_names() {
	let text = "# exported 2026-10-07\n\
		00112233445566778899aabbccddeeff F7A89819-53C1-4BA7-A8BC-BE1D68E2B936\n\n";
	let claims = parse(text).unwrap();
	let claim: [u8; 16] = core::array::from_fn(|i| (i as u8) * 0x11);
	assert_eq!(
		claims.get(&claim).map(|a| a.to_string()).as_deref(),
		Some("f7a89819-53c1-4ba7-a8bc-be1d68e2b936")
	);
	// Anything else stops the boot rather than losing a line.
	for bad in [
		"00112233 F7A89819-53C1-4BA7-A8BC-BE1D68E2B936",
		"00112233445566778899aabbccddeeff",
	] {
		assert!(parse(bad).is_err(), "{bad:?}");
	}
}
