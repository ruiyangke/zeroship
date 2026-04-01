import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import path from 'path'

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  server: {
    proxy: {
      '/api': 'http://localhost:3333',
      '/_stats': 'http://localhost:3333',
      '/_health': 'http://localhost:3333',
      '/_apps': 'http://localhost:3333',
      '/_usage': 'http://localhost:3333',
    }
  },
  build: {
    outDir: 'dist',
  }
})
