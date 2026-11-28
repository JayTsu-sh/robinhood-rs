import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  use: {
    baseURL: process.env.RBH_CONSOLE_TEST_URL ?? 'http://127.0.0.1:8080',
  },
});
