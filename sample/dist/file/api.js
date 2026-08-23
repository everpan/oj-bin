// catch-all 路由（设计 §3）：{*path} 至少吃一段。
// /v1/api/file/a/b/c → path="a/b/c"；/v1/api/file → 404。
function get() {
  json.ok({
    segs: http.param("path", "").split("/")
  });
}
export default {
  get
};
//# sourceMappingURL=data:application/json;base64,eyJ2ZXJzaW9uIjozLCJzb3VyY2VzIjpbImZpbGU6Ly8vVXNlcnMvZXZlci9naXQvZ29sYW5nL21kbS1iYXNlLXJ1c3Qvc2FtcGxlL3NyYy9maWxlL2FwaS50cyJdLCJzb3VyY2VzQ29udGVudCI6WyIvLyBjYXRjaC1hbGwg6Lev55Sx77yI6K6+6K6hIMKnM++8ie+8mnsqcGF0aH0g6Iez5bCR5ZCD5LiA5q6144CCXG4vLyAvdjEvYXBpL2ZpbGUvYS9iL2Mg4oaSIHBhdGg9XCJhL2IvY1wi77ybL3YxL2FwaS9maWxlIOKGkiA0MDTjgIJcbmZ1bmN0aW9uIGdldCgpOiB2b2lkIHtcbiAganNvbi5vayh7IHNlZ3M6IGh0dHAucGFyYW0oXCJwYXRoXCIsIFwiXCIpLnNwbGl0KFwiL1wiKSB9KTtcbn1cbmdldC5yb3V0ZSA9IFwieypwYXRofVwiO1xuZXhwb3J0IGRlZmF1bHQgeyBnZXQgfTtcbiJdLCJuYW1lcyI6W10sIm1hcHBpbmdzIjoiQUFBQSxxQ0FBcUM7QUFDckMsd0RBQXdEO0FBQ3hELFNBQVM7RUFDUCxLQUFLLEVBQUUsQ0FBQztJQUFFLE1BQU0sS0FBSyxLQUFLLENBQUMsUUFBUSxJQUFJLEtBQUssQ0FBQztFQUFLO0FBQ3BEO0FBQ0EsSUFBSSxLQUFLLEdBQUc7QUFDWixlQUFlO0VBQUU7QUFBSSxFQUFFIn0=
