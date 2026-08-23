// .route 声明的 TS 支持（编辑器不报错；dev server 不依赖此文件运行）。
declare global {
  interface Function {
    route?: string;
  }
}
export {};
