import tailwindcss from '@tailwindcss/vite';
import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [
		tailwindcss(),
		sveltekit({
			compilerOptions: {
				// Force runes mode for the project, except for libraries. Can be removed in svelte 6.
				runes: ({ filename }) => filename.split(/[/\\]/).includes('node_modules') ? undefined : true
			},

			// Plain static output — `build/` gets baked into the Rust binary by
			// rust-embed, so the service ships the console with it.
			adapter: adapter()
		})
	],

	// The console talks to the Rust service on :8080. Proxying keeps every fetch
	// same-origin, so there's no CORS layer to add on the server side.
	server: {
		proxy: {
			'/api': 'http://127.0.0.1:8080',
			'/healthz': 'http://127.0.0.1:8080'
		}
	}
});
