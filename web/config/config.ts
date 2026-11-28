import { defineConfig } from '@umijs/max';

export default defineConfig({
  title: 'Lustre 在线冷热分层控制台',
  metas: [{ name: 'viewport', content: 'width=device-width, initial-scale=1' }],
  history: { type: 'hash' },
  publicPath: '/console/',
  hash: true,
  request: {},
  proxy: {
    '/api': { target: 'http://127.0.0.1:8080' },
  },
  routes: [
    { path: '/', redirect: '/policies' },
    { path: '/policies', component: '@/pages/Policies' },
    { path: '/resources', component: '@/pages/Resources' },
  ],
});
