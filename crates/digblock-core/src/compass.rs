//! The world's one compass. Two callers name directions to a human — the
//! admin chat commands (`/tp north`, `/set-spawn-dir`) and the debug
//! overlay's reproduction line — and a second implementation of "which way
//! is north" would be a coin flip nobody could see was wrong.
//!
//! The convention is derived from the engine, not invented: the sun
//! rotates about world Z with sunrise on the −x horizon, so **east = −x,
//! west = +x**; and facing north puts sunrise on your right, where the
//! right-handed view's right is `forward × up` — so **north = +z,
//! south = −z**, which is also what `Dir::North` names. Derive north from
//! that cross product and never from a yaw's sign: the camera's +yaw
//! turns LEFT, and a compass once built on the opposite belief put
//! sunrise on the player's left.

use glam::Vec3;

/// Compass word → look vector (the four cardinals only — a chat command
/// takes a heading, not a diagonal).
pub fn cardinal(dir: &str) -> Option<Vec3> {
	Some(match dir {
		"n" | "north" => Vec3::new(0.0, 0.0, 1.0),
		"s" | "south" => Vec3::new(0.0, 0.0, -1.0),
		"e" | "east" => Vec3::new(-1.0, 0.0, 0.0),
		"w" | "west" => Vec3::new(1.0, 0.0, 0.0),
		_ => return None,
	})
}

/// Which of the eight sectors `dir` points into, indexing [`NAMES`] and
/// [`ABBREV`]: north first, then clockwise. `None` for a direction with no
/// horizontal component to name — straight up or down has no heading, and
/// answering "north" for it would be a lie rather than a rounding.
fn sector(dir: Vec3) -> Option<usize> {
	let (east, north) = (-dir.x, dir.z);
	// A component earns a name only when it carries real weight:
	// tan(22.5°) splits the eight sectors evenly.
	const T: f32 = 0.4142;
	let ns = (north.abs() > T * east.abs()).then_some(north > 0.0);
	let ew = (east.abs() > T * north.abs()).then_some(east > 0.0);
	Some(match (ns, ew) {
		(Some(true), None) => 0,
		(Some(true), Some(true)) => 1,
		(None, Some(true)) => 2,
		(Some(false), Some(true)) => 3,
		(Some(false), None) => 4,
		(Some(false), Some(false)) => 5,
		(None, Some(false)) => 6,
		(Some(true), Some(false)) => 7,
		(None, None) => return None,
	})
}

const NAMES: [&str; 8] = [
	"north",
	"northeast",
	"east",
	"southeast",
	"south",
	"southwest",
	"west",
	"northwest",
];

const ABBREV: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];

/// The rough inverse of [`cardinal`]: the reply to "face this way" is only
/// useful if it names the way. Eight points, because `/set-spawn-dir` with
/// no argument stores whatever the admin was actually looking at and
/// calling a diagonal "east" would be a plain lie. Empty for a heading-less
/// direction.
pub fn compass(dir: Vec3) -> &'static str {
	sector(dir).map_or("", |i| NAMES[i])
}

/// [`compass`] in one or two letters, for the debug overlay — where the
/// panel is a fixed column budget and "northwest" would cost nine of them.
/// `--` rather than empty, so the field keeps its width.
pub fn compass_abbrev(dir: Vec3) -> &'static str {
	sector(dir).map_or("--", |i| ABBREV[i])
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The pair really are inverses on the four cardinals — the property
	/// that makes `/set-spawn-dir north` and the reply agree.
	#[test]
	fn the_four_cardinals_round_trip() {
		for name in ["north", "south", "east", "west"] {
			let v = cardinal(name).unwrap();
			assert_eq!(compass(v), name);
		}
		assert_eq!(cardinal("northeast"), None, "diagonals are not headings");
	}

	/// East is wherever the sun comes up and north is the heading that
	/// puts it on your right — both DERIVED, from the sun and from the
	/// right-handed view's `forward × up`, so a compass that merely
	/// agreed with its own header could not pass.
	#[test]
	fn the_convention_is_the_engines() {
		let morning = crate::sun::Sun::with_tod(crate::sun::TOD_START);
		let sunrise = -morning.sun_dir().with_y(0.0).normalize();
		assert_eq!(compass(sunrise), "east", "sunrise");
		assert_eq!(compass(-sunrise), "west");
		let north = cardinal("north").unwrap();
		assert_eq!(
			north.cross(Vec3::Y),
			sunrise,
			"facing north, sunrise is on the right"
		);
		assert_eq!(compass(north), "north");
		assert_eq!(compass(-north), "south");
		let (dx, dy, dz) = crate::dir::Dir::North.offset();
		assert_eq!(
			north,
			Vec3::new(dx as f32, dy as f32, dz as f32),
			"Dir::North"
		);
	}

	/// Diagonals get named as diagonals, and the split is even: just inside
	/// a sector's half-angle off an axis the name is still that axis, just
	/// past it the name is the diagonal.
	#[test]
	fn eight_sectors_split_evenly() {
		let (north, east) = (cardinal("north").unwrap(), cardinal("east").unwrap());
		// Bearing from north, clockwise — in whichever axes the cardinals
		// stand, so the split is tested and not the convention twice.
		let at = |deg: f32| {
			let (s, c) = deg.to_radians().sin_cos();
			north * c + east * s
		};
		assert_eq!(compass(at(0.0)), "north");
		assert_eq!(compass(at(22.4)), "north");
		assert_eq!(compass(at(22.6)), "northeast");
		assert_eq!(compass(at(45.0)), "northeast");
		assert_eq!(compass(at(135.0)), "southeast");
		assert_eq!(compass(at(225.0)), "southwest");
		assert_eq!(compass(at(315.0)), "northwest");
	}

	/// The abbreviations index the same table, so the overlay and the chat
	/// reply can never name different directions.
	#[test]
	fn abbreviations_track_the_names() {
		// A name is exactly its abbreviation spelled out, letter by
		// letter — so the two tables cannot be transposed against each
		// other anywhere, diagonals included.
		for (i, name) in NAMES.iter().enumerate() {
			let spelled: String = ABBREV[i]
				.chars()
				.map(|c| match c {
					'N' => "north",
					'S' => "south",
					'E' => "east",
					'W' => "west",
					_ => panic!("{c} is not a cardinal letter"),
				})
				.collect();
			assert_eq!(*name, spelled, "row {i}");
		}
		let (north, east) = (cardinal("north").unwrap(), cardinal("east").unwrap());
		assert_eq!(compass_abbrev(north), "N");
		assert_eq!(compass_abbrev(north + east), "NE");
		assert_eq!(compass_abbrev(Vec3::Y), "--", "straight up has no heading");
		assert_eq!(compass(Vec3::Y), "");
	}
}
