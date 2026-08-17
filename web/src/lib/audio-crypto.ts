/// Browser half of the chunked audio format in `src/crypto.rs` — read that for
/// the frame layout. lazynton-js hands lazyxchacha's raw byte API straight
/// through, so a chunk is never converted to text on the way in or out.

import { decryptBytes as openChunk, encryptBytes as sealChunk, ready } from 'lazynton-js';

const MAGIC = new TextEncoder().encode('mucsL2\0\0');
/// `index (u64 BE) || is_last (u8)`, sealed ahead of the audio in place of the
/// associated data lazyxchacha does not take.
const HEADER = 9;
const NONCE = 24;
const TAG = 16;
/// Nonce, tag and header: a frame carrying no audio at all is still this long.
const MIN_FRAME = NONCE + HEADER + TAG;
/// Must match `crypto::CHUNK` on the server; the API reports it so a mismatch
/// shows up as a config error rather than as corrupted audio.
export const CHUNK = 1024 * 1024;

/// Writes the header the server prepends, ahead of the audio, into `framed` —
/// a reordered or truncated stream has to fail to authenticate, not decode into
/// the wrong audio. Returns the exact slice to seal, since the final chunk is
/// short. Fills a caller-owned buffer because allocating a fresh zeroed
/// megabyte per chunk measured as costly as the encryption itself.
function frame(framed: Uint8Array, index: number, last: boolean, plain: Uint8Array) {
	new DataView(framed.buffer).setBigUint64(0, BigInt(index));
	framed[8] = last ? 1 : 0;
	framed.set(plain, HEADER);
	return framed.subarray(0, HEADER + plain.length);
}

function unframe(opened: Uint8Array, index: number): [Uint8Array, boolean] {
	if (opened.length < HEADER) throw new Error(`chunk ${index} is missing its header`);

	const sealedIndex = new DataView(opened.buffer, opened.byteOffset).getBigUint64(0);
	if (sealedIndex !== BigInt(index)) {
		throw new Error(`chunk ${index} was sealed as chunk ${sealedIndex}`);
	}
	const last = opened[8];
	if (last > 1) throw new Error(`chunk ${index} has a bogus last-flag ${last}`);

	return [opened.subarray(HEADER), last === 1];
}

/// True if `data` starts with the format's magic. Objects written before the
/// audio key was turned on are plaintext and have to pass through untouched.
/// Lives here so the magic has exactly one definition — a second copy went stale
/// the first time the format changed and silently fed ciphertext to the decoder.
export function isSealed(data: ArrayBuffer) {
	if (data.byteLength < MAGIC.length) return false;
	const head = new Uint8Array(data, 0, MAGIC.length);
	return MAGIC.every((b, i) => head[i] === b);
}

/// Encrypts a file into the wire format. Returns a Blob so XHR can still report
/// real upload progress, and so the bytes never all live in one ArrayBuffer.
export async function encryptFile(file: Blob, keyHex: string, chunk = CHUNK): Promise<Blob> {
	// lazyxchacha is WebAssembly; it has to be live before the sync calls below.
	await ready();
	const parts: BlobPart[] = [MAGIC];
	const total = Math.max(1, Math.ceil(file.size / chunk));
	const framed = new Uint8Array(HEADER + chunk);

	for (let i = 0; i < total; i++) {
		const slice = new Uint8Array(await file.slice(i * chunk, (i + 1) * chunk).arrayBuffer());
		const sealed = sealChunk(frame(framed, i, i === total - 1, slice), keyHex);
		if (!sealed.length) throw new Error(`chunk ${i} failed to encrypt`);

		const len = new Uint8Array(4);
		new DataView(len.buffer).setUint32(0, sealed.length);
		parts.push(len, sealed);
	}

	return new Blob(parts, { type: 'application/octet-stream' });
}

/// Decrypts a downloaded stem. Throws if the stream is tampered with, truncated,
/// or sealed under a different key.
export async function decryptBytes(data: ArrayBuffer, keyHex: string): Promise<Blob> {
	await ready();
	const bytes = new Uint8Array(data);
	const view = new DataView(data);

	if (bytes.length < MAGIC.length || !MAGIC.every((b, i) => bytes[i] === b)) {
		throw new Error('not in the mucs encrypted-audio format');
	}

	const parts: BlobPart[] = [];
	let at = MAGIC.length;

	for (let index = 0; ; index++) {
		if (at + 4 > bytes.length) throw new Error('encrypted audio is truncated');
		const len = view.getUint32(at);
		at += 4;
		if (len < MIN_FRAME || at + len > bytes.length) {
			throw new Error('encrypted audio is truncated');
		}
		// lazynton-js throws on a failed tag check, which is what we want here.
		const opened = openChunk(bytes.subarray(at, at + len), keyHex);
		at += len;

		const [plain, last] = unframe(opened, index);
		parts.push(plain as BlobPart);
		if (last) break;
	}

	return new Blob(parts);
}

