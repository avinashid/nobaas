import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Builds against this repo: @peerwire/react from ../src, @peerwire/js from packages/js (npm links it). In your
// own app, install both packages and leave out the alias and fs.allow.
export default defineConfig({
  root: fileURLToPath(new URL('.', import.meta.url)),
  plugins: [react()],
  resolve: { alias: { '@peerwire/react': fileURLToPath(new URL('../src/index.ts', import.meta.url)) } },
  server: { fs: { allow: [fileURLToPath(new URL('../..', import.meta.url))] } },
});
