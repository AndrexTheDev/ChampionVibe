import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vite';

export default defineConfig({
  root: fileURLToPath(new URL('./landing/', import.meta.url)),
  build: {
    outDir: fileURLToPath(new URL('./dist-site/', import.meta.url)),
    emptyOutDir: true,
  },
  server: {
    host: '0.0.0.0',
    allowedHosts: ['.e2b.app'],
    port: 4173,
    strictPort: true,
  },
});
