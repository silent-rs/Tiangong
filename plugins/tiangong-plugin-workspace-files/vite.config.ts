import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';
import { viteSingleFile } from 'vite-plugin-singlefile';
// 单文件产物：宿主 Shadow 容器动态注入，iframe 兼容模式也可用 srcdoc 加载。
// shadow 容器以经典脚本执行，关闭 modulepreload 避免产物携带 import.meta。
export default defineConfig({
  base: './',
  plugins: [vue(), viteSingleFile()],
  build: {
    modulePreload: false,
  },
});
