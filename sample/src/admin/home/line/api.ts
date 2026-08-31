// range → 数组长度（对齐 fake/home.fake.ts 的日期逻辑；mock 值是随机数，此处取确定性值便于测试）
export function lineLength(range: string): number {
  const now = new Date();
  if (range === "week") return 7;
  if (range === "month") return now.getDate();
  if (range === "year") {
    let days = 0;
    for (let m = 0; m < now.getMonth(); m++) {
      days += new Date(now.getFullYear(), m + 1, 0).getDate();
    }
    return days;
  }
  return 0;
}

async function post(): Promise<void> {
  const b = http.body as { range?: string } | null;
  const n = lineLength(String(b?.range ?? ""));
  json.ok(Array.from({ length: n }, (_, i) => 100 + ((i * 137) % 901))); // 100–1000，同 mock 区间
}
post.route = "/home/line";
export default { post };
