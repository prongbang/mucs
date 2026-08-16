import { E2eeSession, wrapFetch } from 'lazynton-js';

/// The console is served by the service it talks to, so an empty base keeps the
/// handshake — and every encrypted call — on whatever host the page came from.
/// Nothing here touches crypto or storage until the first request, which is what
/// makes it safe to construct while the page is being prerendered.
export const session = new E2eeSession('', { storageKey: 'mucs:e2ee' });

/// fetch with the lazynton envelope: request bodies go out encrypted, successful
/// responses come back decrypted. Error responses are plaintext JSON and pass
/// through untouched, so `await r.text()` still reads them.
export const efetch = wrapFetch(session);

/// Reads are POSTs: lazynton needs a body to decrypt, and a GET hasn't got one.
export function post(path: string, body: unknown) {
	return efetch(path, { method: 'POST', body: JSON.stringify(body) });
}
