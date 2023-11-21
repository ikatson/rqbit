import { defineConfig } from 'vite';

// https://vitejs.dev/config/
export default defineConfig({
	server: {
		port: 3031
	},
	build: {
		manifest: true
	}

});
