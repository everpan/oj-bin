// L2 单测跑在 vite 解析器上，与 oj 运行时的导入规则本是两套。oj 的导入别名
// （`#x` = 本模块根、`#/m/x` = src 根）必须在这里镜像一份——否则被 spec 直接
// import 的模块内文件一旦用了别名，vitest 就会去按 Node 的 package.json#imports
// 解释 `#` 并报解析失败。
//
// 规则与实现同源（`src/bridge/module_loader.rs::resolve_alias` / `oj/src/checks.rs` S008）：
//   模块根 = 从引用方**目录**向上最近的含 manifest.yaml 的目录；
//   src 根 = 模块根的父目录；
//   `#` 后无斜杠 → 模块根锚点，`#/` → src 根锚点；后缀探针同序（as-is/+.ts/+.js/index）。
import { defineConfig } from "vitest/config";
import type { Plugin } from "vite";
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

/** 与 oj `resolve_relative` 同序的候选后缀。 */
function candidates(p: string): string[] {
  return [p, `${p}.ts`, `${p}.js`, join(p, "index.ts"), join(p, "index.js")];
}

/** 从目录向上最近的含 manifest.yaml 的祖先目录（= oj 的模块根）。 */
function moduleRootOf(from: string): string | null {
  for (let d = from; ; ) {
    if (existsSync(join(d, "manifest.yaml"))) return d;
    const up = dirname(d);
    if (up === d) return null;
    d = up;
  }
}

function ojImportAlias(): Plugin {
  return {
    name: "oj-import-alias",
    resolveId(source, importer) {
      if (!source.startsWith("#") || !importer) return null;
      const moduleRoot = moduleRootOf(dirname(importer));
      // 模块外（tests 目录 / 任务池）无锚点 → 交回 vite（与 oj 的报错语义一致）
      if (!moduleRoot) return null;
      const fromSrcRoot = source.startsWith("#/");
      const anchor = fromSrcRoot ? dirname(moduleRoot) : moduleRoot;
      const rel = fromSrcRoot ? source.slice(2) : source.slice(1);
      // 别名路径禁空段 / `.` / `..`（与 oj 同口径）
      if (!rel || rel.split("/").some((s) => s === "" || s === "." || s === "..")) {
        return null;
      }
      for (const c of candidates(resolve(anchor, rel))) {
        if (existsSync(c)) return c;
      }
      return null;
    },
  };
}

export default defineConfig({
  plugins: [ojImportAlias()],
});
