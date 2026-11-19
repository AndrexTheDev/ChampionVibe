import { defineConfig } from 'vite';

export default defineConfig({
  clearScreen: false,
  server: {
    host: '0.0.0.0',
    port: 1420,
    strictPort: true,
    // Development-only: the Arena preview uses an ephemeral host.
    allowedHosts: true,
    watch: {
      ignored: ['**/src-tauri/**', '**/license-issuer/**', '**/crates/**'],
    },
  },
});
