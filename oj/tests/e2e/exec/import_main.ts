// 相对 import（显式扩展名，file 模式由 OjModuleLoader 解析）：
// 解析链跑通 → 抛出的消息携带 util 的 TAG。
import { TAG } from "./import_util.ts";
throw new Error("TAG=" + TAG);
