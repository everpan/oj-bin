#!/usr/bin/env bash
# 导出 criterion 基准中位数——docs/perf/baseline-*.md 的机器可读伴生数据。
#
# 用法：
#   scripts/perf-baseline.sh            # 打印当前 target/criterion 的中位数表
#   scripts/perf-baseline.sh --save     # 存为 docs/perf/data/<UTC 日期>.txt（下次优化前先 --save 留底）
#
# 下一次对比：cargo bench --bench bridge 后跑本脚本，与上一份快照 diff；
# criterion 自身的 change: 段给的是与**上一次运行**的 Δ（会随重跑滚动），快照才是固定锚点。
set -euo pipefail
cd "$(dirname "$0")/.."

dump() {
  python3 - <<'EOF'
import glob, json, os
rows = []
for f in glob.glob('target/criterion/*/*/new/estimates.json'):
    parts = os.path.relpath(f, 'target/criterion').split(os.sep)
    group, bench = parts[0], parts[1]
    med = json.load(open(f))['median']['point_estimate']
    rows.append((f'{group}/{bench}', med))
for name, med in sorted(rows):
    print(f'{name}\t{round(med)}')
EOF
}

if [ "${1:-}" = "--save" ]; then
  mkdir -p docs/perf/data
  file="docs/perf/data/$(date -u +%Y-%m-%d).txt"
  {
    echo "# oj-bin criterion 中位数快照（单位 ns）"
    echo "# date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "# host: $(uname -sm)"
    echo "# ref:  $(git rev-parse --short HEAD)"
    dump
  } > "$file"
  echo "saved: $file ($(grep -vc '^#' "$file") benches)"
else
  dump
fi
