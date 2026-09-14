// WS 二进制帧回显（v0.1.16）：收到的帧（文本或二进制）原字节回传。
// 二进制帧时 http.body 为 null（不做 UTF-8 破坏），字节一律走 http.bodyBytes()。
export default {
  async message() {
    ws.send(await http.bodyBytes());
  },
};
