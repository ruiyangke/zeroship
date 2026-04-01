import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
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
