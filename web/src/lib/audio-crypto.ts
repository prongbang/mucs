/// Browser half of the chunked audio format in `src/crypto.rs` — read that for
/// the frame layout. AES-GCM because WebCrypto does it in hardware; a JS
/// XChaCha20 would have to chew through a 256 MB upload by hand.

const MAGIC = new TextEncoder().encode('mucsE1\0\0');
const NONCE = 12;
const TAG = 16;
/// Must match `crypto::CHUNK` on the server; the API reports it so a mismatch
/// shows up as a config error rather than as corrupted audio.
export const CHUNK = 1024 * 1024;

function importKey(keyHex: string) {
	const raw = Uint8Array.from(keyHex.match(/../g)!.map((b) => parseInt(b, 16)));
	return crypto.subtle.importKey('raw', raw, 'AES-GCM', false, ['encrypt', 'decrypt']);
}

/// index (u64 BE) || is_last — binds each chunk to its position, so a stream
/// that has been reordered or cut short fails to authenticate.
function aad(index: number, last: boolean) {
	const a = new Uint8Array(9);
	new DataView(a.buffer).setBigUint64(0, BigInt(index));
	a[8] = last ? 1 : 0;
	return a;
}

function nonceFor(index: number) {
	const n = new Uint8Array(NONCE);
	new DataView(n.buffer).setBigUint64(4, BigInt(index));
	return n;
}

/// Encrypts a file into the wire format. Returns a Blob so XHR can still report
/// real upload progress, and so the bytes never all live in one ArrayBuffer.
export async function encryptFile(file: Blob, keyHex: string, chunk = CHUNK): Promise<Blob> {
	const key = await importKey(keyHex);
	const parts: BlobPart[] = [MAGIC];
	const total = Math.max(1, Math.ceil(file.size / chunk));

	for (let i = 0; i < total; i++) {
		const slice = new Uint8Array(await file.slice(i * chunk, (i + 1) * chunk).arrayBuffer());
		const last = i === total - 1;
		const nonce = nonceFor(i);
		const sealed = new Uint8Array(
			await crypto.subtle.encrypt(
				{ name: 'AES-GCM', iv: nonce, additionalData: aad(i, last) },
				key,
				slice
			)
		);

		const len = new Uint8Array(4);
		new DataView(len.buffer).setUint32(0, sealed.length);
		parts.push(len, nonce, sealed);
	}

	return new Blob(parts, { type: 'application/octet-stream' });
}

/// Decrypts a downloaded stem. Throws if the stream is tampered with, truncated,
/// or sealed under a different key.
export async function decryptBytes(data: ArrayBuffer, keyHex: string): Promise<Blob> {
	const key = await importKey(keyHex);
	const bytes = new Uint8Array(data);
	const view = new DataView(data);

	if (bytes.length < MAGIC.length || !MAGIC.every((b, i) => bytes[i] === b)) {
		throw new Error('not in the mucs encrypted-audio format');
	}

	const parts: BlobPart[] = [];
	let at = MAGIC.length;

	for (let index = 0; ; index++) {
		if (at + 4 + NONCE > bytes.length) throw new Error('encrypted audio is truncated');
		const len = view.getUint32(at);
		at += 4;
		const nonce = bytes.subarray(at, at + NONCE);
		at += NONCE;
		if (len < TAG || at + len > bytes.length) throw new Error('encrypted audio is truncated');
		const sealed = bytes.subarray(at, at + len);
		at += len;

		// Same two-step as the server: the final chunk is sealed under a
		// different tag, and only a genuine one authenticates either way.
		let plain: ArrayBuffer | null = null;
		let last = false;
		for (const attempt of [false, true]) {
			try {
				plain = await crypto.subtle.decrypt(
					{ name: 'AES-GCM', iv: nonce, additionalData: aad(index, attempt) },
					key,
					sealed
				);
				last = attempt;
				break;
			} catch {
				/* try the other flag */
			}
		}
		if (!plain) throw new Error(`chunk ${index} failed to authenticate`);

		parts.push(plain);
		if (last) break;
	}

	return new Blob(parts);
}
