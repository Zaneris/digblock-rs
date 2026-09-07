// Just enough PNG to ask a screenshot what is in it: 8-bit RGB/RGBA,
// non-interlaced, which is exactly what Playwright writes. A capture that
// is all one colour is the failure this exists to catch — a black canvas
// from a pipeline that never built, or an overlay that never lifted —
// and nothing else in the toolchain can see a pixel.
import zlib from "node:zlib";

export function decode(buf) {
	if (buf.readUInt32BE(0) !== 0x89504e47) throw new Error("not a PNG");
	let pos = 8, width = 0, height = 0, channels = 0, idat = [];
	while (pos < buf.length) {
		const len = buf.readUInt32BE(pos);
		const type = buf.toString("ascii", pos + 4, pos + 8);
		const data = buf.subarray(pos + 8, pos + 8 + len);
		if (type === "IHDR") {
			width = data.readUInt32BE(0);
			height = data.readUInt32BE(4);
			const depth = data[8], color = data[9], interlace = data[12];
			if (depth !== 8 || interlace !== 0) throw new Error(`unsupported PNG (depth ${depth}, interlace ${interlace})`);
			channels = { 2: 3, 6: 4, 0: 1, 4: 2 }[color];
			if (!channels) throw new Error(`unsupported PNG colour type ${color}`);
		} else if (type === "IDAT") idat.push(data);
		pos += 12 + len;
	}
	const raw = zlib.inflateSync(Buffer.concat(idat));
	const stride = width * channels;
	const rgba = new Uint8Array(width * height * 4);
	let prev = new Uint8Array(stride), src = 0;
	for (let y = 0; y < height; y++) {
		const filter = raw[src++];
		const line = new Uint8Array(raw.subarray(src, src + stride));
		src += stride;
		for (let i = 0; i < stride; i++) {
			const a = i >= channels ? line[i - channels] : 0;
			const b = prev[i];
			const c = i >= channels ? prev[i - channels] : 0;
			let p;
			switch (filter) {
				case 0: p = 0; break;
				case 1: p = a; break;
				case 2: p = b; break;
				case 3: p = (a + b) >> 1; break;
				case 4: { const q = a + b - c, pa = Math.abs(q - a), pb = Math.abs(q - b), pc = Math.abs(q - c);
					p = pa <= pb && pa <= pc ? a : pb <= pc ? b : c; break; }
				default: throw new Error(`bad PNG filter ${filter}`);
			}
			line[i] = (line[i] + p) & 0xff;
		}
		for (let x = 0; x < width; x++) {
			const o = (y * width + x) * 4, s = x * channels;
			rgba[o] = line[s];
			rgba[o + 1] = channels >= 3 ? line[s + 1] : line[s];
			rgba[o + 2] = channels >= 3 ? line[s + 2] : line[s];
			rgba[o + 3] = channels === 4 ? line[s + 3] : channels === 2 ? line[s + 1] : 255;
		}
		prev = line;
	}
	return { width, height, rgba };
}

/** Mean luminance (0..255), the share of near-black pixels, and how many distinct colours (capped). */
export function stats(png, { region } = {}) {
	const { width, height, rgba } = png;
	const [x0, y0, x1, y1] = region ?? [0, 0, width, height];
	let sum = 0, dark = 0, n = 0;
	const seen = new Set();
	for (let y = y0; y < y1; y++) {
		for (let x = x0; x < x1; x++) {
			const o = (y * width + x) * 4;
			const l = 0.2126 * rgba[o] + 0.7152 * rgba[o + 1] + 0.0722 * rgba[o + 2];
			sum += l;
			if (l < 8) dark++;
			if (seen.size < 4096) seen.add((rgba[o] << 16) | (rgba[o + 1] << 8) | rgba[o + 2]);
			n++;
		}
	}
	return { mean: sum / n, darkShare: dark / n, colours: seen.size, pixels: n };
}

/** Mean absolute luminance step between horizontally adjacent pixels: how
 *  much block-scale STRUCTURE a region carries. The sky is a smooth
 *  gradient and answers near zero however bright it is, terrain is
 *  textured and answers several levels — which is the difference between
 *  "something rendered" and "the terrain rendered". */
export function detail(png, { region } = {}) {
	const { width, height, rgba } = png;
	const [x0, y0, x1, y1] = region ?? [0, 0, width, height];
	const lum = (x, y) => {
		const o = (y * width + x) * 4;
		return 0.2126 * rgba[o] + 0.7152 * rgba[o + 1] + 0.0722 * rgba[o + 2];
	};
	let sum = 0, n = 0;
	for (let y = y0; y < y1; y++) {
		for (let x = x0; x < x1 - 1; x++) {
			sum += Math.abs(lum(x, y) - lum(x + 1, y));
			n++;
		}
	}
	return n ? sum / n : 0;
}

/** Mean absolute per-channel difference between two same-sized captures. */
export function diff(a, b, { region } = {}) {
	if (a.width !== b.width || a.height !== b.height) throw new Error("size mismatch");
	const [x0, y0, x1, y1] = region ?? [0, 0, a.width, a.height];
	let sum = 0, n = 0;
	for (let y = y0; y < y1; y++) {
		for (let x = x0; x < x1; x++) {
			const o = (y * a.width + x) * 4;
			for (let c = 0; c < 3; c++) sum += Math.abs(a.rgba[o + c] - b.rgba[o + c]);
			n += 3;
		}
	}
	return n ? sum / n : 0;
}
