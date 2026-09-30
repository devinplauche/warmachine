import { defineConfig } from 'vite';
import tailwindcss from '@tailwindcss/vite';

// https://vitejs.dev/config
export default defineConfig({
  define: {
    'process.env.GOOSE_TUNNEL': JSON.stringify(process.env.GOOSE_TUNNEL !== 'no' && process.env.GOOSE_TUNNEL !== 'none'),
    // Build-time on-prem flag (set WARMACHINE_DESKTOP_ONPREM=1 when packaging
    // the on-prem desktop variant). Baked in at build time like the Rust
    // `onprem` cargo feature: it cannot be flipped at runtime.
    __WARMACHINE_DESKTOP_ONPREM__:
      JSON.stringify(process.env.WARMACHINE_DESKTOP_ONPREM === '1'),
  },

  plugins: [tailwindcss()],

  // Vite caches a copy of @aaif/goose-acp-client and doesn't notice when we rebuild it
  // locally, so it serves stale code until you clear node_modules/.vite by hand.
  // Excluding it makes Vite always read the latest ui/goose-acp-client/dist build.
  // Dev-server only — release builds ignore optimizeDeps.
  optimizeDeps: {
    exclude: ['@aaif/goose-acp-client'],
  },

  build: {
    target: 'esnext'
  },
});
