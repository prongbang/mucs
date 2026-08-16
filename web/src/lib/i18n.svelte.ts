export type Lang = 'th' | 'en';

export const dict = {
	th: {
		title: 'mucs — แยก stems',
		tagline: 'แยกเสียงร้อง กลอง เบส ออกจากเพลง ด้วย demucs',

		serviceUp: 'service ตอบปกติ',
		serviceDown: 'ต่อ service ไม่ติด',
		queueN: (n: number) => `${n} งานในคิว`,
		idle: 'ว่าง',
		offline: 'ออฟไลน์',

		dropAria: 'เลือกไฟล์เพลงเพื่ออัปโหลด',
		uploading: 'กำลังอัปโหลด…',
		dropPrefix: 'ลากไฟล์เพลงมาวาง หรือ ',
		dropLink: 'เลือกไฟล์',
		formats: 'mp3 · wav · flac · m4a — สูงสุด 256 MB',
		model: 'โมเดล',
		twoStems: 'แยกแค่ 2 ราง (vocals / no_vocals) — เร็วกว่า',
		close: 'ปิด',

		models: {
			htdemucs: 'สมดุลที่สุดบน CPU',
			htdemucs_ft: 'ละเอียดขึ้นนิดเดียว ช้ากว่า ~4×',
			htdemucs_6s: '6 ราง เพิ่ม guitar / piano',
			mdx_extra: 'คนละสถาปัตยกรรม ไว้เทียบผล'
		},

		jobs: 'งาน',
		loading: 'กำลังโหลด…',
		empty: 'ยังไม่มีงาน — อัปโหลดเพลงแรกได้เลย',
		status: { queued: 'รอคิว', running: 'กำลังแยก', done: 'เสร็จแล้ว', failed: 'ล้มเหลว' },
		twoStemsTag: '2 ราง',
		waiting: 'รอ worker ว่าง',
		elapsed: 'ผ่านไป',
		took: 'ใช้เวลา',

		deleteBusy: 'ลบไม่ได้ระหว่างประมวลผล',
		deleteTitle: 'ลบงานและไฟล์',
		deleteAria: (f: string) => `ลบงาน ${f}`,
		confirmDelete: (f: string) => `ลบ "${f}" และ stems ทั้งหมดออกจาก storage?`,
		deleteFailed: 'ลบไม่สำเร็จ',
		uploadFailed: (status: number) => `อัปโหลดไม่สำเร็จ (${status})`,
		noService: 'ต่อ service ไม่ติด — เช็คว่า demucs-service ที่ :8080 รันอยู่',

		footer: 'งานเดินทีละหนึ่ง — demucs กิน CPU ทุกคอร์อยู่แล้ว',
		switchTo: 'English'
	},

	en: {
		title: 'mucs — split stems',
		tagline: 'Split vocals, drums and bass out of a track with demucs',

		serviceUp: 'service is responding',
		serviceDown: "can't reach the service",
		queueN: (n: number) => `${n} in queue`,
		idle: 'idle',
		offline: 'offline',

		dropAria: 'choose an audio file to upload',
		uploading: 'Uploading…',
		dropPrefix: 'Drop a track here, or ',
		dropLink: 'choose a file',
		formats: 'mp3 · wav · flac · m4a — up to 256 MB',
		model: 'model',
		twoStems: 'only 2 stems (vocals / no_vocals) — faster',
		close: 'close',

		models: {
			htdemucs: 'best balance on CPU',
			htdemucs_ft: 'a little finer, ~4× slower',
			htdemucs_6s: '6 stems, adds guitar / piano',
			mdx_extra: 'different architecture, for comparison'
		},

		jobs: 'jobs',
		loading: 'Loading…',
		empty: 'No jobs yet — upload your first track',
		status: { queued: 'queued', running: 'separating', done: 'done', failed: 'failed' },
		twoStemsTag: '2 stems',
		waiting: 'waiting for a free worker',
		elapsed: 'elapsed',
		took: 'took',

		deleteBusy: "can't delete while it's processing",
		deleteTitle: 'delete job and files',
		deleteAria: (f: string) => `delete job ${f}`,
		confirmDelete: (f: string) => `Delete "${f}" and all its stems from storage?`,
		deleteFailed: 'delete failed',
		uploadFailed: (status: number) => `upload failed (${status})`,
		noService: "can't reach the service — check that demucs-service is running on :8080",

		footer: 'One job at a time — demucs already uses every core',
		switchTo: 'ไทย'
	}
} as const;

// English is the default, and so also what gets prerendered.
function initial(): Lang {
	if (typeof localStorage === 'undefined') return 'en';
	const saved = localStorage.getItem('lang');
	return saved === 'th' || saved === 'en' ? saved : 'en';
}

class LangStore {
	v = $state<Lang>(initial());

	set(next: Lang) {
		this.v = next;
		localStorage.setItem('lang', next);
	}

	toggle() {
		this.set(this.v === 'th' ? 'en' : 'th');
	}
}

export const lang = new LangStore();
