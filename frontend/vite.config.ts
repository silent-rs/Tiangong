/// <reference types="vitest/config" />
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from 'path'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      '@': path.resolve(import.meta.dirname, './src'),
    },
  },
  test: {
    environment: 'jsdom',
  },
  // Tauri 需要使用 1337 端口或自定义端口
  server: {
    port: 5173,
    strictPort: true,
    // 绑定所有接口（IPv4 + IPv6）：node 18+ 默认只绑 ::1，Tauri 用 127.0.0.1 探测
    // 时会一直 Waiting for your frontend dev server。host:true 同时绑 0.0.0.0 和 [::]。
    host: true,
    watch: {
      // 3. tell vite to ignore watching `src-tauri`
      ignored: ['**/src-tauri/**'],
    },
  },
  build: {
        rollupOptions: {
      // 多页面：index.html 为桌面主界面，config.html 为浏览器配置页
      // （`tiangong config` 从嵌入资源提供）。
      input: {
        main: path.resolve(import.meta.dirname, 'index.html'),
        config: path.resolve(import.meta.dirname, 'config.html'),
      },
      output: {
        manualChunks(id) {
          if (id.includes('react-dom') || id.includes('/react/')) return 'react-core'
          // lucide-react/dynamic 的按需图标（dynamicIconImports 与各 icons/*）保持独立分包，
          // 否则约 1600 个图标会被全部并入 ui-components，失去按需加载并超出体积阈值。
          if (id.includes('lucide-react') && !id.includes('dynamicIconImports') && !id.includes('/icons/'))
            return 'ui-components'
          if (id.includes('md-editor-rt')) return 'markdown'
          if (id.includes('@tauri-apps/api')) return 'tauri'
          if (id.includes('zustand') || id.includes('clsx') || id.includes('tailwind-merge')) return 'utils'
        },
      },
    },
    chunkSizeWarningLimit: 600, // 警告阈值提高到 600KB
  },
})
