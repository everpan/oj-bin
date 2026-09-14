// WS 帧回显（v0.1.16）：帧型忠实回传——文本帧回文本帧（http.body 是 string）、
// 二进制帧回二进制帧（http.body 为 null，字节走 http.bodyBytes()，不做 UTF-8 破坏）。
export default {
  async message() {
    const text = http.body; // 二进制帧为 null
    ws.send(text === null ? await http.bodyBytes() : text);
  },
};
