import { escapeHtml } from "escape-goat";
function post() {
  const b = http.body;
  if (!b.account_id || !b.amount || !b.no) {
    json.fail(400, "account_id, amount, no required");
    return;
  }
  const no = escapeHtml(String(b.no)); // 裸 specifier 参与请求处理（UC-15）
  db.exec("insert into orders (no, account_id, amount) values (?, ?, ?)", [
    no,
    b.account_id,
    b.amount
  ]).then(()=>json.ok({
      created: true,
      no
    })).catch((e)=>json.fail(500, String(e)));
}
function get() {
  const id = Number(http.param("id", 0));
  db.query("select id, no, account_id, amount from orders where account_id = ?", [
    id
  ]).then((r)=>json.ok(r)).catch((e)=>json.fail(500, String(e)));
}
export default {
  get,
  post
};
//# sourceMappingURL=data:application/json;base64,eyJ2ZXJzaW9uIjozLCJzb3VyY2VzIjpbImZpbGU6Ly8vVXNlcnMvZXZlci9naXQvZ29sYW5nL21kbS1iYXNlLXJ1c3Qvc2FtcGxlL3NyYy9vcmRlci9hY2NvdW50L2FwaS50cyJdLCJzb3VyY2VzQ29udGVudCI6WyJpbXBvcnQgeyBlc2NhcGVIdG1sIH0gZnJvbSBcImVzY2FwZS1nb2F0XCI7XG5cbmZ1bmN0aW9uIHBvc3QoKTogdm9pZCB7XG4gIGNvbnN0IGIgPSBodHRwLmJvZHkgYXMgeyBhY2NvdW50X2lkPzogbnVtYmVyOyBhbW91bnQ/OiBudW1iZXI7IG5vPzogc3RyaW5nIH07XG4gIGlmICghYi5hY2NvdW50X2lkIHx8ICFiLmFtb3VudCB8fCAhYi5ubykge1xuICAgIGpzb24uZmFpbCg0MDAsIFwiYWNjb3VudF9pZCwgYW1vdW50LCBubyByZXF1aXJlZFwiKTtcbiAgICByZXR1cm47XG4gIH1cbiAgY29uc3Qgbm8gPSBlc2NhcGVIdG1sKFN0cmluZyhiLm5vKSk7IC8vIOijuCBzcGVjaWZpZXIg5Y+C5LiO6K+35rGC5aSE55CG77yIVUMtMTXvvIlcbiAgZGIuZXhlYyhcImluc2VydCBpbnRvIG9yZGVycyAobm8sIGFjY291bnRfaWQsIGFtb3VudCkgdmFsdWVzICg/LCA/LCA/KVwiLFxuICAgICAgICAgIFtubywgYi5hY2NvdW50X2lkLCBiLmFtb3VudF0pXG4gICAgLnRoZW4oKCkgPT4ganNvbi5vayh7IGNyZWF0ZWQ6IHRydWUsIG5vIH0pKVxuICAgIC5jYXRjaCgoZSkgPT4ganNvbi5mYWlsKDUwMCwgU3RyaW5nKGUpKSk7XG59XG5cbmZ1bmN0aW9uIGdldCgpOiB2b2lkIHtcbiAgY29uc3QgaWQgPSBOdW1iZXIoaHR0cC5wYXJhbShcImlkXCIsIDApKTtcbiAgZGIucXVlcnkoXCJzZWxlY3QgaWQsIG5vLCBhY2NvdW50X2lkLCBhbW91bnQgZnJvbSBvcmRlcnMgd2hlcmUgYWNjb3VudF9pZCA9ID9cIiwgW2lkXSlcbiAgICAudGhlbigocikgPT4ganNvbi5vayhyKSlcbiAgICAuY2F0Y2goKGUpID0+IGpzb24uZmFpbCg1MDAsIFN0cmluZyhlKSkpO1xufVxuXG5leHBvcnQgZGVmYXVsdCB7IGdldCwgcG9zdCB9O1xuIl0sIm5hbWVzIjpbXSwibWFwcGluZ3MiOiJBQUFBLFNBQVMsVUFBVSxRQUFRLGNBQWM7QUFFekMsU0FBUztFQUNQLE1BQU0sSUFBSSxLQUFLLElBQUk7RUFDbkIsSUFBSSxDQUFDLEVBQUUsVUFBVSxJQUFJLENBQUMsRUFBRSxNQUFNLElBQUksQ0FBQyxFQUFFLEVBQUUsRUFBRTtJQUN2QyxLQUFLLElBQUksQ0FBQyxLQUFLO0lBQ2Y7RUFDRjtFQUNBLE1BQU0sS0FBSyxXQUFXLE9BQU8sRUFBRSxFQUFFLElBQUksNEJBQTRCO0VBQ2pFLEdBQUcsSUFBSSxDQUFDLGdFQUNBO0lBQUM7SUFBSSxFQUFFLFVBQVU7SUFBRSxFQUFFLE1BQU07R0FBQyxFQUNqQyxJQUFJLENBQUMsSUFBTSxLQUFLLEVBQUUsQ0FBQztNQUFFLFNBQVM7TUFBTTtJQUFHLElBQ3ZDLEtBQUssQ0FBQyxDQUFDLElBQU0sS0FBSyxJQUFJLENBQUMsS0FBSyxPQUFPO0FBQ3hDO0FBRUEsU0FBUztFQUNQLE1BQU0sS0FBSyxPQUFPLEtBQUssS0FBSyxDQUFDLE1BQU07RUFDbkMsR0FBRyxLQUFLLENBQUMsc0VBQXNFO0lBQUM7R0FBRyxFQUNoRixJQUFJLENBQUMsQ0FBQyxJQUFNLEtBQUssRUFBRSxDQUFDLElBQ3BCLEtBQUssQ0FBQyxDQUFDLElBQU0sS0FBSyxJQUFJLENBQUMsS0FBSyxPQUFPO0FBQ3hDO0FBRUEsZUFBZTtFQUFFO0VBQUs7QUFBSyxFQUFFIn0=
