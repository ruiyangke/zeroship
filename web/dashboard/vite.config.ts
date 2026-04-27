import { defineConfig, loadEnv } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import path from 'path'

// Local-dev proxy: routes calls from the dashboard's dev server
// (typically :5173) to the platform binaries running on their
// standard ports. Override via env if your setup differs:
//   VITE_PROXY_CONTROL=http://localhost:9090
//   VITE_PROXY_GATEWAY=http://localhost:8000
//   VITE_PROXY_AGENT=http://localhost:4444

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), 'VITE_')
  const control = env.VITE_PROXY_CONTROL ?? 'http://localhost:9090'
  const gateway = env.VITE_PROXY_GATEWAY ?? 'http://localhost:8000'
  const agent = env.VITE_PROXY_AGENT ?? 'http://localhost:4444'

  return {
    plugins: [react(), tailwindcss()],
    resolve: {
      alias: {
        '@': path.resolve(__dirname, './src'),
      },
    },
    server: {
      proxy: {
        // Control plane (CRUD + admin)
        '/api': control,
        '/_stats': control,
        '/_health': control,
        '/_apps': control,
        '/_usage': control,
        // Gateway (deployed apps' user-facing URLs — for the preview iframe)
        '/apps': gateway,
        // Agent SSE chat
        '/agent': agent,
      },
    },
    build: {
      outDir: 'dist',
    },
  }
})
