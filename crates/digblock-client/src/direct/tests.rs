use super::*;

/// A page reload or a relaunch reads the identity back from storage, and anything but the same
/// player coming out of it silently strands their character on the server.
#[test]
fn a_stored_player_comes_back_as_the_same_player() {
	let mut me = DirectIdentity::new(core::array::from_fn(|i| i as u8 * 7));
	me.name = Some("Ada_L".into());
	let back = DirectIdentity::from_stored(&me.key_hex(), me.name.clone())
		.expect("what was stored must read back");
	assert_eq!(back.token(), me.token());
	assert_eq!(back.view(), me.view());
	// Damage reads as no player, so the launcher mints a new one rather than a wrong one.
	assert!(DirectIdentity::from_stored(&me.key_hex()[..62], None).is_none());
	assert!(DirectIdentity::from_stored(&"zz".repeat(32), None).is_none());
}
