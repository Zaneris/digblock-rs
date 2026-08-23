// The decoder every browser assertion reads a frame through, against
// images whose bytes are known before they are encoded. It is worth a test
// of its own because a bug here makes the suite MORE likely to pass:
// garbled pixels raise the colour count and lower the dark share, which is
// exactly what `expectRendered` keys on, so a broken unfilter would be
// reported as a rendered frame.
//
// `node --test lib/png.test.mjs` — no browser, no bundle (`just web-unit`).
import test from "node:test";
import assert from "node:assert/strict";
import zlib from "node:zlib";
import { decode, stats, diff, detail } from "./png.mjs";

const PAETH = (a, b, c) => {
	const p = a + b - c, pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
	return pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
};

const COLOUR_TYPE = { 1: 0, 3: 2, 4: 6 };

/** The luminance weights do not sum to exactly one in binary, so white
 *  answers a hair under its own value: compare a luminance, never `===`. */
const near = (actual, want, msg) => assert.ok(Math.abs(actual - want) < 1e-6, `${msg}: ${actual} is not ${want}`);

/** The scanlines of `img`, each filtered by `filter` — the encoder half of
 *  what the decoder undoes, written from the spec rather than from it. */
function filtered({ width, height, channels, bytes }, filter) {
	const stride = width * channels;
	const out = Buffer.alloc((stride + 1) * height);
	let prev = new Uint8Array(stride);
	for (let y = 0; y < height; y++) {
		const line = bytes.subarray(y * stride, (y + 1) * stride);
		out[y * (stride + 1)] = filter;
		for (let i = 0; i < stride; i++) {
			const a = i >= channels ? line[i - channels] : 0;
			const b = prev[i];
			const c = i >= channels ? prev[i - channels] : 0;
			const p = filter === 0 ? 0 : filter === 1 ? a : filter === 2 ? b : filter === 3 ? (a + b) >> 1 : PAETH(a, b, c);
			out[y * (stride + 1) + 1 + i] = (line[i] - p) & 0xff;
		}
		prev = line;
	}
	return out;
}

function chunk(type, data) {
	const head = Buffer.alloc(8);
	head.writeUInt32BE(data.length, 0);
	head.write(type, 4, "ascii");
	const crc = Buffer.alloc(4);
	crc.writeUInt32BE(zlib.crc32(Buffer.concat([Buffer.from(type, "ascii"), data])), 0);
	return Buffer.concat([head, data, crc]);
}

/** A real PNG of `img`, filtered one way throughout. `idatParts` splits the
 *  compressed stream across several IDAT chunks, which is how a browser
 *  writes anything larger than a thumbnail. */
function encode(img, filter, { idatParts = 1 } = {}) {
	const ihdr = Buffer.alloc(13);
	ihdr.writeUInt32BE(img.width, 0);
	ihdr.writeUInt32BE(img.height, 4);
	ihdr[8] = 8; // bit depth
	ihdr[9] = COLOUR_TYPE[img.channels];
	const z = zlib.deflateSync(filtered(img, filter));
	const per = Math.ceil(z.length / idatParts);
	const idat = [];
	for (let o = 0; o < z.length; o += per) idat.push(chunk("IDAT", z.subarray(o, o + per)));
	return Buffer.concat([
		Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
		chunk("IHDR", ihdr),
		...idat,
		chunk("IEND", Buffer.alloc(0)),
	]);
}

const BLACK = [0, 0, 0], WHITE = [255, 255, 255], RED = [255, 0, 0], GREEN = [0, 255, 0], BLUE = [0, 0, 255];

/** `rows` of `[r,g,b]` triples → an image of `channels` channels: grey
 *  takes the green, and alpha is a per-pixel ramp so a channel-stride bug
 *  cannot hide behind a constant. */
function image(rows, channels) {
	const width = rows[0].length, height = rows.length;
	const bytes = Buffer.alloc(width * height * channels);
	rows.forEach((row, y) => row.forEach((px, x) => {
		const o = (y * width + x) * channels;
		if (channels === 1) bytes[o] = px[1];
		else {
			bytes[o] = px[0];
			bytes[o + 1] = px[1];
			bytes[o + 2] = px[2];
			if (channels === 4) bytes[o + 3] = 200 + ((x + y) % 56);
		}
	}));
	return { width, height, channels, bytes };
}

/** What `decode` should answer for `img`, in its always-RGBA output. */
function expected({ width, height, channels, bytes }) {
	const rgba = new Uint8Array(width * height * 4);
	for (let i = 0; i < width * height; i++) {
		const s = i * channels, o = i * 4;
		rgba[o] = bytes[s];
		rgba[o + 1] = channels >= 3 ? bytes[s + 1] : bytes[s];
		rgba[o + 2] = channels >= 3 ? bytes[s + 2] : bytes[s];
		rgba[o + 3] = channels === 4 ? bytes[s + 3] : 255;
	}
	return rgba;
}

// Every colour a later assertion needs, laid out so no two adjacent pixels
// share a value: that is what makes the four predictors disagree.
const MIXED = [
	[BLACK, WHITE, BLACK, WHITE],
	[WHITE, BLACK, WHITE, BLACK],
	[RED, GREEN, BLUE, WHITE],
];

test("every filter type round-trips to the exact bytes it was given", () => {
	let swept = 0;
	for (const channels of [4, 3, 1]) {
		const img = image(MIXED, channels);
		const plain = filtered(img, 0);
		for (let filter = 0; filter <= 4; filter++) {
			const png = decode(encode(img, filter));
			assert.equal(png.width, img.width);
			assert.equal(png.height, img.height);
			assert.deepEqual(png.rgba, expected(img), `filter ${filter}, ${channels} channels`);
			// Without this the sweep could pass five times over the same
			// unfiltered bytes and prove nothing about the unfilter.
			if (filter > 0) {
				assert.notDeepEqual(filtered(img, filter), plain, `filter ${filter} changed nothing to undo`);
			}
			swept++;
		}
	}
	assert.equal(swept, 15);
});

test("a stream split across IDAT chunks decodes as one", () => {
	const img = image(MIXED, 4);
	assert.deepEqual(decode(encode(img, 4, { idatParts: 3 })).rgba, expected(img));
});

test("what is not a PNG, and what this decoder does not claim to read", () => {
	assert.throws(() => decode(Buffer.alloc(64)), /not a PNG/);
	const img = image(MIXED, 4);
	const bad = encode(img, 0);
	bad[8 + 8 + 8] = 16; // IHDR's bit depth, past the 8-byte signature and chunk header
	assert.throws(() => decode(bad), /unsupported PNG/);
});

test("stats reads the image the decoder produced", () => {
	const flat = (px) => decode(encode(image([[px, px], [px, px]], 4), 0));
	assert.deepEqual(
		{ ...stats(flat(BLACK)) },
		{ mean: 0, darkShare: 1, colours: 1, pixels: 4 },
		"a black field is the failure stats exists to catch",
	);
	const white = stats(flat(WHITE));
	near(white.mean, 255, "a white field is as bright as the format goes");
	assert.equal(white.darkShare, 0);
	assert.equal(white.colours, 1);

	const mixed = stats(decode(encode(image(MIXED, 4), 3)));
	assert.equal(mixed.pixels, 12);
	assert.equal(mixed.colours, 5, "black, white, red, green and blue");
	assert.equal(mixed.darkShare, 4 / 12, "the four black pixels, and no others");
	assert.ok(mixed.mean > 0 && mixed.mean < 255);

	// A region is a window on the same image, not a rescaling of it.
	const top = stats(decode(encode(image(MIXED, 4), 1)), { region: [0, 0, 4, 2] });
	assert.equal(top.pixels, 8);
	assert.equal(top.colours, 2);
	assert.equal(top.darkShare, 0.5);
});

test("diff is per-channel, ignores alpha, and is zero for a repeat", () => {
	const black = decode(encode(image([[BLACK, BLACK]], 4), 0));
	const white = decode(encode(image([[WHITE, WHITE]], 4), 0));
	assert.equal(diff(black, black), 0);
	assert.equal(diff(black, white), 255, "black against white is the whole range");
	// Alpha is not part of what a capture SHOWS, so two frames with the
	// same colours and different alpha are the same frame — otherwise the
	// K test's control interval would measure the compositor.
	const opaque = image([[RED], [RED]], 4);
	const a = decode(encode(opaque, 0));
	const b = decode(encode({ ...opaque, bytes: withAlpha(opaque, 7) }, 0));
	assert.equal(diff(a, b), 0);
	assert.throws(() => diff(black, a), /size mismatch/);
});

test("detail separates a flat field from a textured one", () => {
	const flat = decode(encode(image([[RED, RED, RED, RED]], 4), 0));
	const checker = decode(encode(image([[BLACK, WHITE, BLACK, WHITE]], 4), 0));
	const ramp = decode(encode(image([[[0, 0, 0], [0, 10, 0], [0, 20, 0], [0, 30, 0]]], 4), 0));
	assert.equal(detail(flat), 0, "nothing steps between two identical pixels");
	near(detail(checker), 255, "black beside white is the largest step there is");
	assert.ok(detail(ramp) > detail(flat) && detail(ramp) < detail(checker), "a gradient sits between them");
});

function withAlpha(img, alpha) {
	const bytes = Buffer.from(img.bytes);
	for (let i = 3; i < bytes.length; i += 4) bytes[i] = alpha;
	return bytes;
}
