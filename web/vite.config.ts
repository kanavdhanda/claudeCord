import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// The hub (`claudecord serve --dev`) listens here; the dev server forwards API and sign-in calls to it so the cookie stays same-site.
const hub = process.env.CLAUDECORD_HUB ?? 'http://127.0.0.1:8787'

export default defineConfig({
  plugins: [react()],
  server: { port: 5173, proxy: { '/api': hub, '/auth': hub } },
  // The build is embedded in the Rust program, so its files have fixed names and there is exactly one script and one stylesheet.
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    cssCodeSplit: false,
    rollupOptions: {
      output: {
        entryFileNames: 'app.js',
        chunkFileNames: 'app-[name].js',
        assetFileNames: 'app[extname]',
        codeSplitting: false,
      },
    },
  },
})
