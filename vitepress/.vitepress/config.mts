import { defineConfig } from 'vitepress';
import { withMermaid } from 'vitepress-plugin-mermaid';
import { splitNav, sampleModules } from './generated/sidebar.mjs';

// 中文分词：minisearch 默认按空格切词，对中文无效，改用「单字 + 二元组」。
const tokenize = (text: string) => {
  const chars = Array.from(text.replace(/[\s\p{P}\p{S}]/gu, ''));
  const bigrams = chars.slice(0, -1).map((c, i) => c + chars[i + 1]);
  return Array.from(new Set([...chars, ...bigrams]));
};

export default withMermaid(defineConfig({
  title: 'oj 开发者手册',
  description: 'only-js（oj）从入门到掌握：JS/TS 写业务，Rust 兜底能力，插件可插拔。',
  lang: 'zh-CN',
  srcDir: 'src',
  markdown: { html: true },
  // mermaid 依赖的 fastdom 是 CJS 包，不预打包的话 dev 下会报
  // "does not provide an export named 'default'"。
  vite: { optimizeDeps: { include: ['mermaid', 'fastdom'] } },
  // 死链直接让构建失败：保证站点内链始终可点。
  // 例外：文档里大量 `curl http://localhost:9778/...` 示例是给读者本地跑的，
  // 构建机上没有服务，不能算死链。
  ignoreDeadLinks: [/^http:\/\/localhost/],
  themeConfig: {
    outline: [2, 3],
    search: {
      provider: 'local',
      options: {
        miniSearch: { options: { tokenize } },
      },
    },
    nav: [
      { text: '首页', link: '/' },
      { text: '学习路径', link: '/guide/01-what-is-oj' },
      { text: 'API 参考', link: '/reference/api-manual/' },
      { text: '模块地图', link: '/modules/' },
      { text: '示例实战', link: '/sample/' },
    ],
    sidebar: [
      {
        text: '开始',
        items: [
          { text: '这是什么', link: '/guide/01-what-is-oj' },
          { text: '构建与运行', link: '/guide/02-install-build' },
          { text: '第一个接口', link: '/guide/03-first-api' },
        ],
      },
      {
        text: '学习路径',
        items: [
          { text: '模块解剖', link: '/guide/04-module-anatomy' },
          { text: '全局对象速查', link: '/guide/05-globals-tour' },
          { text: '数据层与迁移', link: '/guide/06-data-layer' },
          { text: '鉴权与多租户', link: '/guide/07-auth-tenant' },
          { text: '实时与消息', link: '/guide/08-realtime' },
        ],
      },
      {
        text: '工程实践',
        items: [
          { text: '测试', link: '/guide/09-testing' },
          { text: '构建发布与运维', link: '/guide/10-build-release-ops' },
          { text: '内部实现走读', link: '/guide/11-internals' },
          { text: '排障 FAQ', link: '/guide/12-faq' },
        ],
      },
      {
        text: 'API 参考',
        items: [
          {
            text: 'JS API 手册',
            link: '/reference/api-manual/',
            collapsed: true,
            items: splitNav['/reference/api-manual'],
          },
          { text: 'bridge 与全局对象', link: '/reference/bridge' },
          { text: 'WebSocket', link: '/reference/websocket' },
          { text: 'MQ 与长任务', link: '/reference/mq-tasks' },
          { text: '数据迁移', link: '/reference/migration' },
          { text: '测试手册', link: '/reference/testing' },
          { text: 'OIDC 接入', link: '/reference/oidc-integration' },
          { text: 'OIDC 实现', link: '/reference/oidc-implementation' },
          { text: '插件开发', link: '/reference/plugin-development' },
          { text: '内置 API 与鉴权', link: '/reference/builtin-api-auth' },
          { text: '基准测试', link: '/reference/benchmarks' },
        ],
      },
      {
        text: '手册原文',
        items: [
          {
            text: '开发指南',
            link: '/reference/dev-guide/',
            collapsed: true,
            items: splitNav['/reference/dev-guide'],
          },
          {
            text: '用户手册',
            link: '/reference/user-manual/',
            collapsed: true,
            items: splitNav['/reference/user-manual'],
          },
          { text: '运维手册', link: '/reference/ops-manual' },
          { text: 'oj 开发 skill', link: '/reference/devkit-skill' },
        ],
      },
      {
        text: '架构与模块地图',
        items: [
          { text: '模块地图（索引）', link: '/modules/' },
          { text: '00 · 总览', link: '/modules/00-overview' },
          { text: '01 · 核心运行时', link: '/modules/01-core-bridge' },
          { text: '02 · 配置模型', link: '/modules/02-config' },
          { text: '03 · HTTP 服务', link: '/modules/03-server-http' },
          { text: '04 · CLI 与装配', link: '/modules/04-oj-cli' },
          { text: '05 · FFI 与插件', link: '/modules/05-ffi-and-plugins' },
          { text: '06 · 工具链', link: '/modules/06-toolchain' },
          { text: '07 · 模块数据层', link: '/modules/07-data-layer' },
          { text: '08 · 测试体系', link: '/modules/08-testing' },
        ],
      },
      {
        text: '示例实战',
        items: [
          { text: 'sample 项目', link: '/sample/' },
          { text: '模块导览', link: '/sample/modules-tour' },
          // 模块专题（sample/src/*/README.md）由 sync-docs.mjs 生成，勿手加。
          ...sampleModules,
        ],
      },
      {
        text: '附录',
        items: [
          { text: '术语表', link: '/appendix/glossary' },
          { text: '历史与未收录文档', link: '/appendix/history-index' },
        ],
      },
    ],
    socialLinks: [{ icon: 'github', link: 'https://github.com/everpan/only-js' }],
    footer: {
      message: '内容由 scripts/sync-docs.mjs 从仓库文档同步生成',
    },
  },
}));
