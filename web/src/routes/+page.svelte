<script lang="ts">
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
		created_at: string;
		started_at: string | null;
		finished_at: string | null;
	};

	const MODELS: [string, string][] = [
		['htdemucs', 'สมดุลที่สุดบน CPU'],
		['htdemucs_ft', 'ละเอียดขึ้นนิดเดียว ช้ากว่า ~4×'],
		['htdemucs_6s', '6 ราง เพิ่ม guitar / piano'],
		['mdx_extra', 'คนละสถาปัตยกรรม ไว้เทียบผล']
	];

	// Static class strings — Tailwind can't see them if they're built at runtime.
	const PILL: Record<string, string> = {
		queued: 'bg-ink-700/60 text-ink-200 ring-ink-600',
		running: 'bg-accent/15 text-accent ring-accent/30',
		done: 'bg-emerald-400/10 text-emerald-300 ring-emerald-400/25',
		failed: 'bg-rose-500/10 text-rose-300 ring-rose-500/25'
	};
	const STATUS_TH: Record<string, string> = {
		queued: 'รอคิว',
		running: 'กำลังแยก',
		done: 'เสร็จแล้ว',
		failed: 'ล้มเหลว'
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

	let jobs = $state<Job[]>([]);
	let queueDepth = $state(0);
	let online = $state(true);
	let loaded = $state(false);

	let model = $state('htdemucs');
	let twoStems = $state(false);

	let uploading = $state(false);
	let uploadPct = $state(0);
	let dragging = $state(false);
	let notice = $state<string | null>(null);

	let fileInput = $state<HTMLInputElement | null>(null);

	// Tick once a second so the "กำลังแยก" durations count up between polls.
	let now = $state(Date.now());

	async function refresh() {
		try {
			const [j, h] = await Promise.all([fetch('/api/jobs?limit=50'), fetch('/healthz')]);
			if (j.ok) jobs = (await j.json()).jobs;
			if (h.ok) queueDepth = (await h.json()).queued ?? 0;
			online = j.ok && h.ok;
		} catch {
			online = false;
		}
		loaded = true;
	}

	$effect(() => {
		let stopped = false;

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

	function upload(file: File) {
		notice = null;
		uploading = true;
		uploadPct = 0;

		const form = new FormData();
		form.append('file', file);
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
			if (xhr.status >= 400) notice = errorText(xhr.responseText, `อัปโหลดไม่สำเร็จ (${xhr.status})`);
			refresh();
		};
		xhr.onerror = () => {
			uploading = false;
			notice = 'ต่อ service ไม่ติด — เช็คว่า demucs-service ที่ :8080 รันอยู่';
		};
		xhr.send(form);
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

	async function remove(job: Job) {
		if (!confirm(`ลบ "${job.filename}" และ stems ทั้งหมดออกจาก storage?`)) return;
		const r = await fetch(`/api/jobs/${job.id}`, { method: 'DELETE' });
		if (!r.ok) notice = errorText(await r.text(), 'ลบไม่สำเร็จ');
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
		if (job.status === 'queued') return 'รอ worker ว่าง';
		const start = Date.parse(job.started_at ?? job.created_at);
		const end = job.finished_at ? Date.parse(job.finished_at) : now;
		return `${job.status === 'running' ? 'ผ่านไป' : 'ใช้เวลา'} ${fmtDuration(end - start)}`;
	}

	let active = $derived(jobs.filter((j) => j.status === 'running' || j.status === 'queued').length);
</script>

<svelte:head><title>mucs — แยก stems</title></svelte:head>

<div class="mx-auto flex min-h-dvh max-w-3xl flex-col gap-8 px-5 py-10 sm:py-14">
	<!-- ------------------------------------------------------------ header -->
	<header class="flex items-baseline justify-between gap-4">
		<div>
			<h1 class="text-2xl font-semibold tracking-tight">mucs</h1>
			<p class="mt-1 text-sm text-ink-400">แยกเสียงร้อง กลอง เบส ออกจากเพลง ด้วย demucs</p>
		</div>

		<div
			class="flex shrink-0 items-center gap-2 rounded-full bg-ink-850 px-3 py-1.5 text-xs ring-1 ring-ink-800"
			title={online ? 'service ตอบปกติ' : 'ต่อ service ไม่ติด'}
		>
			<span
				class="size-1.5 rounded-full {online ? 'bg-emerald-400' : 'bg-rose-500'}"
				class:animate-pulse={!online}
			></span>
			<span class="tnum text-ink-200">
				{online ? (queueDepth > 0 ? `${queueDepth} งานในคิว` : 'ว่าง') : 'ออฟไลน์'}
			</span>
		</div>
	</header>

	<!-- ------------------------------------------------------------ upload -->
	<section>
		<div
			role="button"
			tabindex="0"
			aria-label="เลือกไฟล์เพลงเพื่ออัปโหลด"
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
				<p class="text-sm text-ink-200">กำลังอัปโหลด…</p>
				<p class="tnum mt-3 text-3xl font-semibold text-accent">{uploadPct}%</p>
			{:else}
				<p class="text-sm text-ink-50">
					ลากไฟล์เพลงมาวาง หรือ <span class="text-accent underline underline-offset-4">เลือกไฟล์</span>
				</p>
				<p class="mt-2 text-xs text-ink-400">mp3 · wav · flac · m4a — สูงสุด 256 MB</p>
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
				<span class="text-ink-400">โมเดล</span>
				<select
					bind:value={model}
					class="rounded-lg bg-ink-850 px-2.5 py-1.5 text-ink-50 ring-1 ring-ink-700 focus-visible:ring-accent focus-visible:outline-none"
				>
					{#each MODELS as [id, hint] (id)}
						<option value={id}>{id} — {hint}</option>
					{/each}
				</select>
			</label>

			<label class="flex cursor-pointer items-center gap-2 text-ink-400 select-none">
				<input
					type="checkbox"
					bind:checked={twoStems}
					class="size-3.5 accent-[var(--color-accent)]"
				/>
				แยกแค่ 2 ราง (vocals / no_vocals) — เร็วกว่า
			</label>
		</div>

		{#if notice}
			<p
				class="mt-3 flex items-start gap-2 rounded-lg bg-rose-500/10 px-3 py-2 text-xs text-rose-200 ring-1 ring-rose-500/20"
			>
				<span class="grow">{notice}</span>
				<button class="shrink-0 text-rose-300/70 hover:text-rose-200" onclick={() => (notice = null)}>
					ปิด
				</button>
			</p>
		{/if}
	</section>

	<!-- -------------------------------------------------------------- jobs -->
	<section class="flex-1">
		<h2 class="mb-3 flex items-center gap-2 text-xs font-medium tracking-wide text-ink-400 uppercase">
			งาน
			{#if active > 0}
				<span class="tnum rounded-full bg-accent/15 px-1.5 text-accent">{active}</span>
			{/if}
		</h2>

		{#if !loaded}
			<p class="py-10 text-center text-sm text-ink-400">กำลังโหลด…</p>
		{:else if jobs.length === 0}
			<p class="rounded-xl border border-ink-800 py-12 text-center text-sm text-ink-400">
				ยังไม่มีงาน — อัปโหลดเพลงแรกได้เลย
			</p>
		{:else}
			<ul class="flex flex-col gap-2.5">
				{#each jobs as job (job.id)}
					<li class="rounded-xl bg-ink-900/70 p-4 ring-1 ring-ink-800">
						<div class="flex items-start gap-3">
							<div class="min-w-0 flex-1">
								<p class="truncate text-sm font-medium" title={job.filename}>{job.filename}</p>
								<p class="tnum mt-1 flex flex-wrap items-center gap-x-2 text-xs text-ink-400">
									<span>{job.model}</span>
									{#if job.two_stems}<span>· 2 ราง</span>{/if}
									<span>· {timing(job)}</span>
								</p>
							</div>

							<span
								class="shrink-0 rounded-full px-2.5 py-1 text-xs ring-1 ring-inset {PILL[job.status]}"
							>
								{STATUS_TH[job.status] ?? job.status}
							</span>

							<button
								onclick={() => remove(job)}
								disabled={job.status === 'running'}
								title={job.status === 'running' ? 'ลบไม่ได้ระหว่างประมวลผล' : 'ลบงานและไฟล์'}
								aria-label="ลบงาน {job.filename}"
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
		{/if}
	</section>

	<footer class="text-center text-xs text-ink-600">
		งานเดินทีละหนึ่ง — demucs กิน CPU ทุกคอร์อยู่แล้ว
	</footer>
</div>
