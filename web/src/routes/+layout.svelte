<script lang="ts">
	import './layout.css';
	import favicon from '$lib/assets/favicon.svg';
	import { lang } from '$lib/i18n.svelte';

	let { children } = $props();

	// <html lang> can't be set from svelte:head, and it's what screen readers
	// and the browser's own translate prompt read.
	$effect(() => {
		document.documentElement.lang = lang.v;
	});
</script>

<svelte:head>
	<link rel="icon" href={favicon} />
	<meta name="theme-color" content="#08090b" />
</svelte:head>

{@render children()}
