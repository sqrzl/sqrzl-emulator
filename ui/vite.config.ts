import { defineConfig } from 'vite-plus';
import { askr } from '@askrjs/vite';
import { sqrzlMockApi } from './dev/mock-api/plugin';

export default defineConfig(({ mode }) => ({
  plugins:
    mode === 'mock'
      ? [sqrzlMockApi() as unknown as ReturnType<typeof askr>, askr()]
      : [askr()],
  lint: {
    ignorePatterns: ['dist/**', 'node_modules/**', 'coverage/**'],
  },
  fmt: {
    semi: true,
    singleQuote: true,
    trailingComma: 'es5',
    printWidth: 80,
    tabWidth: 2,
  },
  server: {
    port: 5173,
    open: true,
    proxy: {
      '/admin/v1': {
        target: 'http://127.0.0.1:9001',
        changeOrigin: true,
      },
      '/api': {
        target: 'http://127.0.0.1:9001',
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    sourcemap: true,
  },
}));
