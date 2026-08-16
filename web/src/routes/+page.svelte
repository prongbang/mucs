<script lang="ts">
	import { dict, lang } from '$lib/i18n.svelte';
	import { efetch, post } from '$lib/api';
	import { CHUNK, decryptBytes, encryptFile } from '$lib/audio-crypto';

	type Stem = { name: string; bytes: number; download_url: string };
	type Job = {
		id: string;
		filename: string;
		status: 'queued' | 'running' | 'done' | 'failed';
		progress: number;
		model: string;
		two_stems: string | null;
		stems: Stem[];
		error: string | null;
		favorite: boolean;
		created_at: string;
		started_at: string | null;
		finished_at: string | null;
	};

	const MODELS = ['htdemucs', 'htdemucs_ft', 'htdemucs_6s', 'mdx_extra'] as const;

	const s = $derived(dict[lang.v]);

	// Static class strings — Tailwind can't see them if they're built at runtime.
	const PILL: Record<string, string> = {
		queued: 'bg-ink-700/60 text-ink-200 ring-ink-600',
		running: 'bg-accent/15 text-accent ring-accent/30',
		done: 'bg-emerald-400/10 text-emerald-300 ring-emerald-400/25',
		failed: 'bg-rose-500/10 text-rose-300 ring-rose-500/25'
	};
	const DOT: Record<string, string> = {
		vocals: 'bg-rose-400',
		drums: 'bg-amber-400',
		bass: 'bg-violet-400',
		other: 'bg-sky-400',
		guitar: 'bg-lime-400',
		piano: 'bg-fuchsia-400',
		no_vocals: 'bg-ink-400'
	};

	const SORTS = ['newest', 'oldest', 'name', 'name_desc'] as const;
	const PAGE = 20;

	let jobs = $state<Job[]>([]);
	let total = $state(0);
	let queueDepth = $state(0);
	let online = $state(true);
	let loaded = $state(false);

	let query = $state('');
	let sort = $state<(typeof SORTS)[number]>('newest');
	let favOnly = $state(false);
	let offset = $state(0);

	let model = $state('htdemucs');
	let twoStems = $state(false);

	let uploading = $state(false);
	let uploadPct = $state(0);
	let dragging = $state(false);
	let notice = $state<string | null>(null);

	let fileInput = $state<HTMLInputElement | null>(null);

	// Tick once a second so the running durations count up between polls.
	let now = $state(Date.now());

	function filters() {
		return {
			limit: PAGE,
			offset,
			sort,
			q: query.trim() || null,
			favorite: favOnly || null
		};
	}

	async function refresh() {
		try {
			// /healthz stays plaintext — it's the container's healthcheck too.
			const [j, h] = await Promise.all([post('/api/jobs/search', filters()), fetch('/healthz')]);
			if (j.ok) {
				const body = await j.json();
				jobs = body.jobs;
				total = body.total;
			}
			if (h.ok) queueDepth = (await h.json()).queued ?? 0;
			online = j.ok && h.ok;
		} catch {
			online = false;
		}
		loaded = true;
	}

	// Filter changes shouldn't wait out the poll interval, and typing shouldn't
	// fire a request per keystroke.
	$effect(() => {
		void [query, sort, favOnly, offset];
		const t = setTimeout(refresh, 200);
		return () => clearTimeout(t);
	});

	$effect(() => {
		let stopped = false;
		loadAudioKey();

		(async function poll() {
			while (!stopped) {
				await refresh();
				// Reads after an await aren't tracked, so this stays a plain loop:
				// fast while something is moving, lazy when the queue is idle.
				const busy = jobs.some((j) => j.status === 'running' || j.status === 'queued');
				await new Promise((r) => setTimeout(r, busy ? 1200 : 6000));
			}
		})();

		const tick = setInterval(() => (now = Date.now()), 1000);

		return () => {
			stopped = true;
			clearInterval(tick);
		};
	});

	function errorText(body: string, fallback: string) {
		try {
			return JSON.parse(body).error ?? fallback;
		} catch {
			return fallback;
		}
	}

	/// The audio key, fetched once over the encrypted channel. `null` means the
	/// service has no AUDIO_KEY configured and audio moves in the clear.
	let audioKey = $state<string | null>(null);
	let sealing = $state(false);

	async function loadAudioKey() {
		try {
			const r = await post('/api/audio-key', {});
			if (r.ok) {
				const body = await r.json();
				audioKey = body.key ?? null;
				if (audioKey && body.chunk !== CHUNK) {
					// Different chunk sizes would produce audio that decrypts to
					// garbage, so refuse rather than corrupt the upload.
					audioKey = null;
					notice = s.chunkMismatch;
				}
			}
		} catch {
			/* the poll will surface an offline service on its own */
		}
	}

	async function upload(file: File) {
		notice = null;
		uploading = true;
		uploadPct = 0;

		let payload: Blob = file;
		if (audioKey) {
			sealing = true;
			try {
				payload = await encryptFile(file, audioKey);
			} catch {
				sealing = false;
				uploading = false;
				notice = s.encryptFailed;
				return;
			}
			sealing = false;
		}

		const form = new FormData();
		// Keep the original name on the part: the server derives the stored key
		// and demucs' input extension from it.
		form.append('file', payload, file.name);
		form.append('model', model);
		if (twoStems) form.append('two_stems', 'vocals');

		// XHR rather than fetch: a 40MB track over a slow link needs a real
		// upload progress bar, and fetch still can't give one.
		const xhr = new XMLHttpRequest();
		xhr.open('POST', '/api/jobs');
		xhr.upload.onprogress = (e) => {
			if (e.lengthComputable) uploadPct = Math.round((e.loaded / e.total) * 100);
		};
		xhr.onload = () => {
			uploading = false;
			if (xhr.status >= 400) notice = errorText(xhr.responseText, s.uploadFailed(xhr.status));
			refresh();
		};
		xhr.onerror = () => {
			uploading = false;
			notice = s.noService;
		};
		xhr.send(form);
	}

	/// Encrypted stems arrive as ciphertext, so the browser has to unseal them
	/// and hand the user a Blob instead of just following the link.
	async function download(job: Job, stem: Stem) {
		if (!audioKey) return; // plain stems: let the anchor do its job
		notice = null;
		try {
			const r = await fetch(stem.download_url);
			if (!r.ok) throw new Error(String(r.status));
			const blob = await decryptBytes(await r.arrayBuffer(), audioKey);

			const base = job.filename.replace(/\.[^.]+$/, '');
			const ext = stem.download_url.split('.').pop() ?? 'mp3';
			const a = document.createElement('a');
			a.href = URL.createObjectURL(blob);
			a.download = `${base} - ${stem.name}.${ext}`;
			a.click();
			URL.revokeObjectURL(a.href);
		} catch {
			notice = s.decryptFailed;
		}
	}

	function pick(files: FileList | null) {
		const file = files?.[0];
		if (file) upload(file);
	}

	function onDrop(e: DragEvent) {
		e.preventDefault();
		dragging = false;
		if (!uploading) pick(e.dataTransfer?.files ?? null);
	}

	// Keyed by id, so the polling refresh underneath doesn't disturb the edit.
	let editing = $state<string | null>(null);
	let draft = $state('');

	async function saveName(job: Job) {
		// Enter commits and then blurs, so this runs twice for one edit.
		if (editing !== job.id) return;
		const name = draft.trim();
		editing = null;
		if (!name || name === job.filename) return;

		const r = await efetch(`/api/jobs/${job.id}`, {
			method: 'PATCH',
			body: JSON.stringify({ filename: name })
		});
		if (!r.ok) notice = errorText(await r.text(), s.renameFailed);
		refresh();
	}

	async function toggleFavorite(job: Job) {
		job.favorite = !job.favorite; // optimistic — the poll corrects it if the write fails
		const r = await efetch(`/api/jobs/${job.id}`, {
			method: 'PATCH',
			body: JSON.stringify({ favorite: job.favorite })
		});
		if (!r.ok) notice = errorText(await r.text(), s.favoriteFailed);
		refresh();
	}

	async function remove(job: Job) {
		if (!confirm(s.confirmDelete(job.filename))) return;
		// The body isn't read, but it has to exist: an encrypted empty string
		// decrypts to "", which lazynton can't tell from a failed decrypt.
		const r = await efetch(`/api/jobs/${job.id}`, { method: 'DELETE', body: '{}' });
		if (!r.ok) notice = errorText(await r.text(), s.deleteFailed);
		refresh();
	}

	function fmtBytes(n: number) {
		return n >= 1048576 ? `${(n / 1048576).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`;
	}

	function fmtDuration(ms: number) {
		const s = Math.max(0, Math.round(ms / 1000));
		return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
	}

	function timing(job: Job) {
		if (job.status === 'queued') return s.waiting;
		const start = Date.parse(job.started_at ?? job.created_at);
		const end = job.finished_at ? Date.parse(job.finished_at) : now;
		return `${job.status === 'running' ? s.elapsed : s.took} ${fmtDuration(end - start)}`;
	}

	let active = $derived(jobs.filter((j) => j.status === 'running' || j.status === 'queued').length);
</script>

<svelte:head><title>{s.title}</title></svelte:head>

<div class="mx-auto flex min-h-dvh max-w-3xl flex-col gap-8 px-5 py-10 sm:py-14">
	<!-- ------------------------------------------------------------ header -->
	<header class="flex items-baseline justify-between gap-4">
		<div>
			<h1 class="text-2xl font-semibold tracking-tight">mucs</h1>
			<p class="mt-1 text-sm text-ink-400">{s.tagline}</p>
		</div>

		<div class="flex shrink-0 items-center gap-2">
			<button
				onclick={() => lang.toggle()}
				class="rounded-full bg-ink-850 px-3 py-1.5 text-xs text-ink-200 ring-1 ring-ink-800 transition hover:text-ink-50 hover:ring-ink-600 focus-visible:ring-2 focus-visible:ring-accent focus-visible:outline-none"
			>
				{s.switchTo}
			</button>

			<div
				class="flex items-center gap-2 rounded-full bg-ink-850 px-3 py-1.5 text-xs ring-1 ring-ink-800"
				title={online ? s.serviceUp : s.serviceDown}
			>
				<span
					class="size-1.5 rounded-full {online ? 'bg-emerald-400' : 'bg-rose-500'}"
					class:animate-pulse={!online}
				></span>
				<span class="tnum text-ink-200">
					{online ? (queueDepth > 0 ? s.queueN(queueDepth) : s.idle) : s.offline}
				</span>
			</div>
		</div>
	</header>

	<!-- ------------------------------------------------------------ upload -->
	<section>
		<div
			role="button"
			tabindex="0"
			aria-label={s.dropAria}
			aria-busy={uploading}
			class="group relative w-full overflow-hidden rounded-2xl border border-dashed p-8 text-center transition
				focus-visible:ring-2 focus-visible:ring-accent focus-visible:outline-none
				{dragging ? 'border-accent bg-accent/5' : 'border-ink-700 bg-ink-900/60 hover:border-ink-600'}
				{uploading ? 'pointer-events-none opacity-70' : 'cursor-pointer'}"
			ondragover={(e) => {
				e.preventDefault();
				dragging = true;
			}}
			ondragleave={() => (dragging = false)}
			ondrop={onDrop}
			onclick={() => fileInput?.click()}
			onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && fileInput?.click()}
		>
			{#if uploading}
				<p class="text-sm text-ink-200">{sealing ? s.encrypting : s.uploading}</p>
				<p class="tnum mt-3 text-3xl font-semibold text-accent">{uploadPct}%</p>
			{:else}
				<p class="text-sm text-ink-50">
					{s.dropPrefix}<span class="text-accent underline underline-offset-4">{s.dropLink}</span>
				</p>
				<p class="mt-2 text-xs text-ink-400">{s.formats}</p>
			{/if}

			<!-- Upload progress doubles as the panel's bottom edge. -->
			<div
				class="absolute inset-x-0 bottom-0 h-0.5 bg-accent transition-[width] duration-200"
				style="width: {uploading ? uploadPct : 0}%"
			></div>
		</div>

		<input
			bind:this={fileInput}
			type="file"
			accept="audio/*,.mp3,.wav,.flac,.m4a,.ogg"
			class="hidden"
			onchange={(e) => {
				pick(e.currentTarget.files);
				e.currentTarget.value = '';
			}}
		/>

		<div class="mt-3 flex flex-wrap items-center gap-3 text-xs">
			<label class="flex items-center gap-2">
				<span class="text-ink-400">{s.model}</span>
				<select
					bind:value={model}
					class="rounded-lg bg-ink-850 px-2.5 py-1.5 text-ink-50 ring-1 ring-ink-700 focus-visible:ring-accent focus-visible:outline-none"
				>
					{#each MODELS as id (id)}
						<option value={id}>{id} — {s.models[id]}</option>
					{/each}
				</select>
			</label>

			<label class="flex cursor-pointer items-center gap-2 text-ink-400 select-none">
				<input
					type="checkbox"
					bind:checked={twoStems}
					class="size-3.5 accent-[var(--color-accent)]"
				/>
				{s.twoStems}
			</label>
		</div>

		{#if notice}
			<p
				class="mt-3 flex items-start gap-2 rounded-lg bg-rose-500/10 px-3 py-2 text-xs text-rose-200 ring-1 ring-rose-500/20"
			>
				<span class="grow">{notice}</span>
				<button class="shrink-0 text-rose-300/70 hover:text-rose-200" onclick={() => (notice = null)}>
					{s.close}
				</button>
			</p>
		{/if}
	</section>

	<!-- -------------------------------------------------------------- jobs -->
	<section class="flex-1">
		<h2 class="mb-3 flex items-center gap-2 text-xs font-medium tracking-wide text-ink-400 uppercase">
			{s.jobs}
			{#if active > 0}
				<span class="tnum rounded-full bg-accent/15 px-1.5 text-accent">{active}</span>
			{/if}
		</h2>

		<!-- ----------------------------------------------------------- toolbar -->
		<div class="mb-3 flex flex-wrap items-center gap-2 text-xs">
			<input
				type="search"
				bind:value={query}
				oninput={() => (offset = 0)}
				placeholder={s.searchPlaceholder}
				aria-label={s.searchPlaceholder}
				class="min-w-40 flex-1 rounded-lg bg-ink-850 px-3 py-1.5 text-ink-50 ring-1 ring-ink-700 placeholder:text-ink-600 focus-visible:ring-accent focus-visible:outline-none"
			/>

			<select
				bind:value={sort}
				onchange={() => (offset = 0)}
				aria-label={s.sortLabel}
				class="rounded-lg bg-ink-850 px-2.5 py-1.5 text-ink-50 ring-1 ring-ink-700 focus-visible:ring-accent focus-visible:outline-none"
			>
				{#each SORTS as id (id)}
					<option value={id}>{s.sorts[id]}</option>
				{/each}
			</select>

			<button
				onclick={() => {
					favOnly = !favOnly;
					offset = 0;
				}}
				aria-pressed={favOnly}
				class="rounded-lg px-2.5 py-1.5 ring-1 transition focus-visible:ring-2 focus-visible:ring-accent focus-visible:outline-none
					{favOnly
					? 'bg-amber-400/10 text-amber-300 ring-amber-400/30'
					: 'bg-ink-850 text-ink-400 ring-ink-700 hover:text-ink-200'}"
			>
				★ {s.favOnly}
			</button>
		</div>

		{#if !loaded}
			<p class="py-10 text-center text-sm text-ink-400">{s.loading}</p>
		{:else if jobs.length === 0}
			<p class="rounded-xl border border-ink-800 py-12 text-center text-sm text-ink-400">
				{query.trim() || favOnly ? s.noMatch : s.empty}
			</p>
		{:else}
			<ul class="flex flex-col gap-2.5">
				{#each jobs as job (job.id)}
					<li class="rounded-xl bg-ink-900/70 p-4 ring-1 ring-ink-800">
						<div class="flex items-start gap-3">
							<div class="min-w-0 flex-1">
								{#if editing === job.id}
									<input
										value={draft}
										oninput={(e) => (draft = e.currentTarget.value)}
										onblur={() => saveName(job)}
										onkeydown={(e) => {
											if (e.key !== 'Enter' && e.key !== 'Escape') return;
											if (e.key === 'Escape') draft = job.filename;
											e.currentTarget.blur();
											saveName(job); // committing here, not in onblur, keeps
											// the keyboard path working even when the
											// document isn't focused
										}}
										aria-label={s.renameAria(job.filename)}
										{@attach (el: HTMLInputElement) => el.select()}
										class="w-full rounded-md bg-ink-850 -ml-1.5 px-1.5 py-0.5 text-sm font-medium text-ink-50 ring-1 ring-ink-700 focus-visible:ring-accent focus-visible:outline-none"
									/>
								{:else}
									<button
										onclick={() => {
											editing = job.id;
											draft = job.filename;
										}}
										title={s.renameTitle}
										class="block w-full truncate rounded-md -ml-1.5 px-1.5 py-0.5 text-left text-sm font-medium transition hover:bg-ink-850 focus-visible:ring-2 focus-visible:ring-accent focus-visible:outline-none"
									>
										{job.filename}
									</button>
								{/if}
								<p class="tnum mt-1 flex flex-wrap items-center gap-x-2 text-xs text-ink-400">
									<span>{job.model}</span>
									{#if job.two_stems}<span>· {s.twoStemsTag}</span>{/if}
									<span>· {timing(job)}</span>
								</p>
							</div>

							<span
								class="shrink-0 rounded-full px-2.5 py-1 text-xs ring-1 ring-inset {PILL[job.status]}"
							>
								{s.status[job.status] ?? job.status}
							</span>

							<button
								onclick={() => toggleFavorite(job)}
								title={job.favorite ? s.favoriteRemove : s.favoriteAdd}
								aria-pressed={job.favorite}
								aria-label={s.favoriteAria(job.filename)}
								class="shrink-0 rounded-lg p-1.5 transition hover:bg-ink-800 focus-visible:ring-2 focus-visible:ring-accent focus-visible:outline-none
									{job.favorite ? 'text-amber-300' : 'text-ink-600 hover:text-amber-300'}"
							>
								<svg
									class="size-4"
									viewBox="0 0 20 20"
									fill={job.favorite ? 'currentColor' : 'none'}
									stroke="currentColor"
									stroke-width="1.5"
								>
									<path
										d="M10 2.8l2.2 4.5 5 .7-3.6 3.5.9 4.9L10 14.1l-4.5 2.3.9-4.9L2.8 8l5-.7z"
										stroke-linejoin="round"
									/>
								</svg>
							</button>

							<button
								onclick={() => remove(job)}
								disabled={job.status === 'running'}
								title={job.status === 'running' ? s.deleteBusy : s.deleteTitle}
								aria-label={s.deleteAria(job.filename)}
								class="shrink-0 rounded-lg p-1.5 text-ink-400 transition hover:bg-ink-800 hover:text-rose-300 disabled:pointer-events-none disabled:opacity-30"
							>
								<svg class="size-4" viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.5">
									<path d="M4 6h12M8.5 6V4.5h3V6M6.5 6l.6 9.5h5.8L13.5 6" stroke-linecap="round" />
								</svg>
							</button>
						</div>

						{#if job.status === 'running'}
							<div class="mt-3 h-1 overflow-hidden rounded-full bg-ink-800">
								<div
									class="h-full rounded-full bg-accent transition-[width] duration-500"
									style="width: {Math.max(2, job.progress)}%"
									class:animate-pulse={job.progress === 0}
								></div>
							</div>
							<p class="tnum mt-1.5 text-right text-xs text-ink-400">{job.progress}%</p>
						{/if}

						{#if job.error}
							<p class="mt-3 rounded-lg bg-rose-500/5 px-3 py-2 font-mono text-xs break-words text-rose-300/90">
								{job.error}
							</p>
						{/if}

						{#if job.stems.length > 0}
							<div class="mt-3 flex flex-wrap gap-2">
								{#each job.stems as stem (stem.name)}
									<a
										href={stem.download_url}
										onclick={(e) => {
											if (audioKey) {
												e.preventDefault();
												download(job, stem);
											}
										}}
										class="group flex items-center gap-2 rounded-lg bg-ink-850 py-1.5 pr-3 pl-2.5 text-xs ring-1 ring-ink-700 transition hover:bg-ink-800 hover:ring-ink-600"
									>
										<span class="size-1.5 rounded-full {DOT[stem.name] ?? 'bg-ink-400'}"></span>
										<span class="text-ink-50">{stem.name}</span>
										<span class="tnum text-ink-400">{fmtBytes(stem.bytes)}</span>
										<svg
											class="size-3.5 text-ink-400 transition group-hover:text-accent"
											viewBox="0 0 16 16"
											fill="none"
											stroke="currentColor"
											stroke-width="1.5"
											stroke-linecap="round"
										>
											<path d="M8 2.5v8m0 0L5 7.5m3 3 3-3M3 13h10" />
										</svg>
									</a>
								{/each}
							</div>
						{/if}
					</li>
				{/each}
			</ul>

			{#if total > PAGE}
				<div class="mt-4 flex items-center justify-between text-xs text-ink-400">
					<span class="tnum">{s.range(offset + 1, offset + jobs.length, total)}</span>
					<div class="flex gap-2">
						<button
							onclick={() => (offset = Math.max(0, offset - PAGE))}
							disabled={offset === 0}
							class="rounded-lg bg-ink-850 px-2.5 py-1.5 ring-1 ring-ink-700 transition hover:text-ink-50 disabled:pointer-events-none disabled:opacity-30"
						>
							{s.prev}
						</button>
						<button
							onclick={() => (offset += PAGE)}
							disabled={offset + jobs.length >= total}
							class="rounded-lg bg-ink-850 px-2.5 py-1.5 ring-1 ring-ink-700 transition hover:text-ink-50 disabled:pointer-events-none disabled:opacity-30"
						>
							{s.next}
						</button>
					</div>
				</div>
			{/if}
		{/if}
	</section>

	<footer class="text-center text-xs text-ink-600">{s.footer}</footer>
</div>
