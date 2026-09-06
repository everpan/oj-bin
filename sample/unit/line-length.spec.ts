// L2 纯函数分支：`lineLength` 的日期边界。
//
// L1（`sample/tests/admin.test.ts` 的 `POST /home/line`）只覆盖 week/month/year/其他
// 四个口径在**真实当前时钟**下的值；「闰年 2 月」「月末」「年初」这类**固定日期下的
// 期望值**只有 mock 层能确定性断言（L1 跑在真实时钟上，钉不死 2 月 29 日）。
//
// 原先这里的 role-list 分页用例（分页包装 / camelCase / pageSize+current）已删除：
// 与 L1 的 `GET /role-list` 契约用例完全重复，按「契约类归 L1」规则不再双写。

import { describe, it, expect, vi, afterEach } from "vitest";
import { lineLength } from "../src/admin/home/line/api";

afterEach(() => {
  vi.useRealTimers();
});

// 用本地时间分量构造，避免 runner 时区不同导致跨日（如 UTC-11 会把 10:00Z 算成前一天）。
function freeze(y: number, m: number, d: number) {
  vi.useFakeTimers();
  vi.setSystemTime(new Date(y, m - 1, d, 12, 0, 0));
}

describe("admin/home/line lineLength (L2 分支/边界)", () => {
  it("week → 7；未知/空 range → 0", () => {
    freeze(2026, 9, 6);
    expect(lineLength("week")).toBe(7);
    expect(lineLength("nope")).toBe(0);
    expect(lineLength("")).toBe(0);
  });

  it("month → 当天日号（含月末与月初）", () => {
    freeze(2026, 1, 31);
    expect(lineLength("month")).toBe(31);
    freeze(2026, 2, 1);
    expect(lineLength("month")).toBe(1);
  });

  it("year → 截至上月底累计天数（闰年 2 月计 29 天）", () => {
    freeze(2024, 3, 1); // 闰年：1 月 31 + 2 月 29 = 60
    expect(lineLength("year")).toBe(60);
    freeze(2026, 3, 1); // 平年：31 + 28 = 59
    expect(lineLength("year")).toBe(59);
  });

  it("year 在 1 月 → 0（上月底累计为空）", () => {
    freeze(2026, 1, 15);
    expect(lineLength("year")).toBe(0);
  });
});
