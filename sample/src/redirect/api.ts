// json.redirect 原语演示：3xx + Location + 短超文本注记（RFC 9110 §15.4 SHOULD；HEAD 为空 body）。
// 典型场景：把请求 302 到 blob.url() 给出的 S3 预签名 URL，浏览器两跳直取对象。
function get(): void {
  json.redirect.found(String(http.param("to", "https://example.com/")));
}
export default { get };
