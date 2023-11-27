import { defineConfig } from 'vite';

// https://vitejs.dev/config/
export default defineConfig({
	//	plugins: [react()],
	server: {
		port: 3031
	},
	build: {
		manifest: true
	}

});
