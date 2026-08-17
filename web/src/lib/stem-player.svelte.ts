import { decryptBytes, isSealed } from './audio-crypto';

/// Plays a job's stems as one multitrack: every stem starts on the same
/// AudioContext clock, so muting `vocals` leaves the rest exactly where it was.
/// Separate `<audio>` elements drift apart within seconds, which defeats the
/// point of having separated them.

export type Track = {
	name: string;
	buffer: AudioBuffer;
	gain: GainNode;
	muted: boolean;
	volume: number;
};

export class StemPlayer {
	jobId = $state<string | null>(null);
	loading = $state(false);
	error = $state<string | null>(null);
	playing = $state(false);
	/// Longest stem — they're all the same length in practice, but a truncated
	/// one shouldn't shorten the scrubber.
	duration = $state(0);
	position = $state(0);
	tracks = $state<Track[]>([]);

	#ctx: AudioContext | null = null;
	#sources: AudioBufferSourceNode[] = [];
	/// Context time the current run started, and the offset it started from.
	#startedAt = 0;
	#startedFrom = 0;
	/// An interval rather than requestAnimationFrame: rAF stops in a background
	/// tab, which would freeze the scrubber and — worse — miss the end of the
	/// track while the audio kept running.
	#timer: ReturnType<typeof setInterval> | null = null;

	get open() {
		return this.jobId !== null;
	}

	/// Fetches and decodes every stem. Called from a click, so the AudioContext
	/// is created inside a user gesture and won't be born suspended.
	async load(jobId: string, stems: { name: string; download_url: string }[], keyHex: string | null) {
		this.close();
		this.jobId = jobId;
		this.loading = true;
		this.error = null;

		// ponytail: decodes every stem to memory — ~85 MB per 4-minute stereo
		// stem. Fine for four stems; stream it if that ever stops being true.
		const ctx = new AudioContext();
		this.#ctx = ctx;

		try {
			const tracks = await Promise.all(
				stems.map(async (s) => {
					const res = await fetch(`${s.download_url}?inline=true`);
					if (!res.ok) throw new Error(`${s.name}: HTTP ${res.status}`);

					let bytes = await res.arrayBuffer();

					// Go by what arrived rather than by whether a key was passed:
					// handing ciphertext to the decoder just reports "unable to
					// decode audio data", which says nothing about the real cause.
					if (isSealed(bytes)) {
						if (!keyHex) throw new Error(`${s.name}: encrypted, but no audio key`);
						bytes = await (await decryptBytes(bytes, keyHex)).arrayBuffer();
					}

					const buffer = await ctx.decodeAudioData(bytes);
					const gain = ctx.createGain();
					gain.connect(ctx.destination);
					return { name: s.name, buffer, gain, muted: false, volume: 1 };
				})
			);

			// A different job's player may have been opened while this was loading.
			if (this.jobId !== jobId) return;

			this.tracks = tracks;
			this.duration = Math.max(...tracks.map((t) => t.buffer.duration));
			this.loading = false;
		} catch (e) {
			this.error = e instanceof Error ? e.message : 'could not load the stems';
			this.loading = false;
		}
	}

	play(from = this.position) {
		const ctx = this.#ctx;
		if (!ctx || !this.tracks.length) return;
		this.#stopSources();

		if (from >= this.duration) from = 0;
		// A hair in the future so every source starts on the same tick rather
		// than each one whenever its start() call happens to run.
		const at = ctx.currentTime + 0.06;

		for (const t of this.tracks) {
			const src = ctx.createBufferSource();
			src.buffer = t.buffer;
			src.connect(t.gain);
			src.start(at, from);
			this.#sources.push(src);
		}

		this.#startedAt = at;
		this.#startedFrom = from;
		this.playing = true;
		ctx.resume();
		this.#tick();
	}

	pause() {
		this.position = this.#now();
		this.#stopSources();
		this.playing = false;
		this.#stopTimer();
	}

	toggle() {
		this.playing ? this.pause() : this.play();
	}

	seek(seconds: number) {
		this.position = Math.min(Math.max(0, seconds), this.duration);
		if (this.playing) this.play(this.position);
	}

	setMuted(track: Track, muted: boolean) {
		track.muted = muted;
		track.gain.gain.value = muted ? 0 : track.volume;
	}

	setVolume(track: Track, volume: number) {
		track.volume = volume;
		if (!track.muted) track.gain.gain.value = volume;
	}

	/// Mute everything else, or unmute everything if this one already had it.
	solo(track: Track) {
		const alone = this.tracks.every((t) => (t === track ? !t.muted : t.muted));
		for (const t of this.tracks) this.setMuted(t, alone ? false : t !== track);
	}

	close() {
		this.#stopSources();
		this.#stopTimer();
		this.#ctx?.close();
		this.#ctx = null;
		this.tracks = [];
		this.jobId = null;
		this.playing = false;
		this.position = 0;
		this.duration = 0;
		this.error = null;
		this.loading = false;
	}

	#now() {
		if (!this.#ctx || !this.playing) return this.position;
		return Math.min(this.#startedFrom + (this.#ctx.currentTime - this.#startedAt), this.duration);
	}

	#tick() {
		this.#stopTimer();
		this.#timer = setInterval(() => {
			if (!this.playing) return this.#stopTimer();
			this.position = Math.max(0, this.#now());
			if (this.position >= this.duration) {
				this.pause();
				this.position = 0;
			}
		}, 100);
	}

	#stopTimer() {
		if (this.#timer !== null) clearInterval(this.#timer);
		this.#timer = null;
	}

	#stopSources() {
		for (const s of this.#sources) {
			try {
				s.stop();
			} catch {
				/* already stopped */
			}
		}
		this.#sources = [];
	}
}
