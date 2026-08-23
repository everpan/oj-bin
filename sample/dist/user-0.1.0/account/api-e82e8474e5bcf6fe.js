import { positiveId, requireRole } from "../_shared/validate.js";
function get() {
  const id = Number(http.param("id", 0));
  const rows = id > 0 ? db.query("select id, name, role from account where id = ?", [
    id
  ]) : db.query("select id, name, role from account", []);
  rows.then((r)=>json.ok(r)).catch((e)=>json.fail(500, String(e)));
}
function post() {
  const b = http.body;
  if (!b.name) {
    json.fail(400, "name required");
    return;
  }
  const role = (()=>{
    try {
      return requireRole(b.role ?? "user");
    } catch (e) {
      return "";
    }
  })();
  if (!role) {
    json.fail(400, "role must be admin|user");
    return;
  }
  db.exec("insert into account (name, role) values (?, ?)", [
    b.name,
    role
  ]).then(()=>json.ok({
      created: true
    })).catch((e)=>json.fail(500, String(e)));
}
function put() {
  const b = http.body;
  const id = (()=>{
    try {
      return positiveId(b.id);
    } catch  {
      return 0;
    }
  })();
  if (!id || !b.name) {
    json.fail(400, "id and name required");
    return;
  }
  db.exec("update account set name = ? where id = ?", [
    b.name,
    id
  ]).then(()=>json.ok({
      updated: true
    })).catch((e)=>json.fail(500, String(e)));
}
function del() {
  const id = positiveId(http.param("id", 0));
  db.exec("delete from account where id = ?", [
    id
  ]).then(()=>json.ok({
      deleted: true
    })).catch((e)=>json.fail(500, String(e)));
}
function patch() {
  const b = http.body;
  const role = requireRole(b.role);
  db.exec("update account set role = ? where id = ?", [
    role,
    positiveId(b.id)
  ]).then(()=>json.ok({
      patched: true
    })).catch((e)=>json.fail(500, String(e)));
}
function head() {
  get();
}
function options() {
  json.ok({
    methods: [
      "get",
      "post",
      "put",
      "del",
      "patch",
      "head",
      "options"
    ]
  });
}
export default {
  get,
  post,
  put,
  del,
  patch,
  head,
  options
};
//# sourceMappingURL=data:application/json;base64,eyJ2ZXJzaW9uIjozLCJzb3VyY2VzIjpbImZpbGU6Ly8vVXNlcnMvZXZlci9naXQvZ29sYW5nL21kbS1iYXNlLXJ1c3Qvc2FtcGxlL3NyYy91c2VyL2FjY291bnQvYXBpLnRzIl0sInNvdXJjZXNDb250ZW50IjpbImltcG9ydCB7IHBvc2l0aXZlSWQsIHJlcXVpcmVSb2xlIH0gZnJvbSBcIi4uL19zaGFyZWQvdmFsaWRhdGVcIjtcblxuZnVuY3Rpb24gZ2V0KCk6IHZvaWQge1xuICBjb25zdCBpZCA9IE51bWJlcihodHRwLnBhcmFtKFwiaWRcIiwgMCkpO1xuICBjb25zdCByb3dzID0gaWQgPiAwXG4gICAgPyBkYi5xdWVyeShcInNlbGVjdCBpZCwgbmFtZSwgcm9sZSBmcm9tIGFjY291bnQgd2hlcmUgaWQgPSA/XCIsIFtpZF0pXG4gICAgOiBkYi5xdWVyeShcInNlbGVjdCBpZCwgbmFtZSwgcm9sZSBmcm9tIGFjY291bnRcIiwgW10pO1xuICByb3dzLnRoZW4oKHIpID0+IGpzb24ub2socikpLmNhdGNoKChlKSA9PiBqc29uLmZhaWwoNTAwLCBTdHJpbmcoZSkpKTtcbn1cblxuZnVuY3Rpb24gcG9zdCgpOiB2b2lkIHtcbiAgY29uc3QgYiA9IGh0dHAuYm9keSBhcyB7IG5hbWU/OiBzdHJpbmc7IHJvbGU/OiBzdHJpbmcgfTtcbiAgaWYgKCFiLm5hbWUpIHsganNvbi5mYWlsKDQwMCwgXCJuYW1lIHJlcXVpcmVkXCIpOyByZXR1cm47IH1cbiAgY29uc3Qgcm9sZSA9ICgoKSA9PiB7IHRyeSB7IHJldHVybiByZXF1aXJlUm9sZShiLnJvbGUgPz8gXCJ1c2VyXCIpOyB9IGNhdGNoIChlKSB7IHJldHVybiBcIlwiOyB9IH0pKCk7XG4gIGlmICghcm9sZSkgeyBqc29uLmZhaWwoNDAwLCBcInJvbGUgbXVzdCBiZSBhZG1pbnx1c2VyXCIpOyByZXR1cm47IH1cbiAgZGIuZXhlYyhcImluc2VydCBpbnRvIGFjY291bnQgKG5hbWUsIHJvbGUpIHZhbHVlcyAoPywgPylcIiwgW2IubmFtZSwgcm9sZV0pXG4gICAgLnRoZW4oKCkgPT4ganNvbi5vayh7IGNyZWF0ZWQ6IHRydWUgfSkpXG4gICAgLmNhdGNoKChlKSA9PiBqc29uLmZhaWwoNTAwLCBTdHJpbmcoZSkpKTtcbn1cblxuZnVuY3Rpb24gcHV0KCk6IHZvaWQge1xuICBjb25zdCBiID0gaHR0cC5ib2R5IGFzIHsgaWQ/OiBudW1iZXI7IG5hbWU/OiBzdHJpbmcgfTtcbiAgY29uc3QgaWQgPSAoKCkgPT4geyB0cnkgeyByZXR1cm4gcG9zaXRpdmVJZChiLmlkKTsgfSBjYXRjaCB7IHJldHVybiAwOyB9IH0pKCk7XG4gIGlmICghaWQgfHwgIWIubmFtZSkgeyBqc29uLmZhaWwoNDAwLCBcImlkIGFuZCBuYW1lIHJlcXVpcmVkXCIpOyByZXR1cm47IH1cbiAgZGIuZXhlYyhcInVwZGF0ZSBhY2NvdW50IHNldCBuYW1lID0gPyB3aGVyZSBpZCA9ID9cIiwgW2IubmFtZSwgaWRdKVxuICAgIC50aGVuKCgpID0+IGpzb24ub2soeyB1cGRhdGVkOiB0cnVlIH0pKVxuICAgIC5jYXRjaCgoZSkgPT4ganNvbi5mYWlsKDUwMCwgU3RyaW5nKGUpKSk7XG59XG5cbmZ1bmN0aW9uIGRlbCgpOiB2b2lkIHtcbiAgY29uc3QgaWQgPSBwb3NpdGl2ZUlkKGh0dHAucGFyYW0oXCJpZFwiLCAwKSk7XG4gIGRiLmV4ZWMoXCJkZWxldGUgZnJvbSBhY2NvdW50IHdoZXJlIGlkID0gP1wiLCBbaWRdKVxuICAgIC50aGVuKCgpID0+IGpzb24ub2soeyBkZWxldGVkOiB0cnVlIH0pKVxuICAgIC5jYXRjaCgoZSkgPT4ganNvbi5mYWlsKDUwMCwgU3RyaW5nKGUpKSk7XG59XG5cbmZ1bmN0aW9uIHBhdGNoKCk6IHZvaWQge1xuICBjb25zdCBiID0gaHR0cC5ib2R5IGFzIHsgaWQ/OiBudW1iZXI7IHJvbGU/OiBzdHJpbmcgfTtcbiAgY29uc3Qgcm9sZSA9IHJlcXVpcmVSb2xlKGIucm9sZSk7XG4gIGRiLmV4ZWMoXCJ1cGRhdGUgYWNjb3VudCBzZXQgcm9sZSA9ID8gd2hlcmUgaWQgPSA/XCIsIFtyb2xlLCBwb3NpdGl2ZUlkKGIuaWQpXSlcbiAgICAudGhlbigoKSA9PiBqc29uLm9rKHsgcGF0Y2hlZDogdHJ1ZSB9KSlcbiAgICAuY2F0Y2goKGUpID0+IGpzb24uZmFpbCg1MDAsIFN0cmluZyhlKSkpO1xufVxuXG5mdW5jdGlvbiBoZWFkKCk6IHZvaWQgeyBnZXQoKTsgfVxuXG5mdW5jdGlvbiBvcHRpb25zKCk6IHZvaWQge1xuICBqc29uLm9rKHsgbWV0aG9kczogW1wiZ2V0XCIsIFwicG9zdFwiLCBcInB1dFwiLCBcImRlbFwiLCBcInBhdGNoXCIsIFwiaGVhZFwiLCBcIm9wdGlvbnNcIl0gfSk7XG59XG5cbmV4cG9ydCBkZWZhdWx0IHsgZ2V0LCBwb3N0LCBwdXQsIGRlbCwgcGF0Y2gsIGhlYWQsIG9wdGlvbnMgfTtcbiJdLCJuYW1lcyI6W10sIm1hcHBpbmdzIjoiQUFBQSxTQUFTLFVBQVUsRUFBRSxXQUFXLFFBQVEsc0JBQXNCO0FBRTlELFNBQVM7RUFDUCxNQUFNLEtBQUssT0FBTyxLQUFLLEtBQUssQ0FBQyxNQUFNO0VBQ25DLE1BQU0sT0FBTyxLQUFLLElBQ2QsR0FBRyxLQUFLLENBQUMsbURBQW1EO0lBQUM7R0FBRyxJQUNoRSxHQUFHLEtBQUssQ0FBQyxzQ0FBc0MsRUFBRTtFQUNyRCxLQUFLLElBQUksQ0FBQyxDQUFDLElBQU0sS0FBSyxFQUFFLENBQUMsSUFBSSxLQUFLLENBQUMsQ0FBQyxJQUFNLEtBQUssSUFBSSxDQUFDLEtBQUssT0FBTztBQUNsRTtBQUVBLFNBQVM7RUFDUCxNQUFNLElBQUksS0FBSyxJQUFJO0VBQ25CLElBQUksQ0FBQyxFQUFFLElBQUksRUFBRTtJQUFFLEtBQUssSUFBSSxDQUFDLEtBQUs7SUFBa0I7RUFBUTtFQUN4RCxNQUFNLE9BQU8sQ0FBQztJQUFRLElBQUk7TUFBRSxPQUFPLFlBQVksRUFBRSxJQUFJLElBQUk7SUFBUyxFQUFFLE9BQU8sR0FBRztNQUFFLE9BQU87SUFBSTtFQUFFLENBQUM7RUFDOUYsSUFBSSxDQUFDLE1BQU07SUFBRSxLQUFLLElBQUksQ0FBQyxLQUFLO0lBQTRCO0VBQVE7RUFDaEUsR0FBRyxJQUFJLENBQUMsa0RBQWtEO0lBQUMsRUFBRSxJQUFJO0lBQUU7R0FBSyxFQUNyRSxJQUFJLENBQUMsSUFBTSxLQUFLLEVBQUUsQ0FBQztNQUFFLFNBQVM7SUFBSyxJQUNuQyxLQUFLLENBQUMsQ0FBQyxJQUFNLEtBQUssSUFBSSxDQUFDLEtBQUssT0FBTztBQUN4QztBQUVBLFNBQVM7RUFDUCxNQUFNLElBQUksS0FBSyxJQUFJO0VBQ25CLE1BQU0sS0FBSyxDQUFDO0lBQVEsSUFBSTtNQUFFLE9BQU8sV0FBVyxFQUFFLEVBQUU7SUFBRyxFQUFFLE9BQU07TUFBRSxPQUFPO0lBQUc7RUFBRSxDQUFDO0VBQzFFLElBQUksQ0FBQyxNQUFNLENBQUMsRUFBRSxJQUFJLEVBQUU7SUFBRSxLQUFLLElBQUksQ0FBQyxLQUFLO0lBQXlCO0VBQVE7RUFDdEUsR0FBRyxJQUFJLENBQUMsNENBQTRDO0lBQUMsRUFBRSxJQUFJO0lBQUU7R0FBRyxFQUM3RCxJQUFJLENBQUMsSUFBTSxLQUFLLEVBQUUsQ0FBQztNQUFFLFNBQVM7SUFBSyxJQUNuQyxLQUFLLENBQUMsQ0FBQyxJQUFNLEtBQUssSUFBSSxDQUFDLEtBQUssT0FBTztBQUN4QztBQUVBLFNBQVM7RUFDUCxNQUFNLEtBQUssV0FBVyxLQUFLLEtBQUssQ0FBQyxNQUFNO0VBQ3ZDLEdBQUcsSUFBSSxDQUFDLG9DQUFvQztJQUFDO0dBQUcsRUFDN0MsSUFBSSxDQUFDLElBQU0sS0FBSyxFQUFFLENBQUM7TUFBRSxTQUFTO0lBQUssSUFDbkMsS0FBSyxDQUFDLENBQUMsSUFBTSxLQUFLLElBQUksQ0FBQyxLQUFLLE9BQU87QUFDeEM7QUFFQSxTQUFTO0VBQ1AsTUFBTSxJQUFJLEtBQUssSUFBSTtFQUNuQixNQUFNLE9BQU8sWUFBWSxFQUFFLElBQUk7RUFDL0IsR0FBRyxJQUFJLENBQUMsNENBQTRDO0lBQUM7SUFBTSxXQUFXLEVBQUUsRUFBRTtHQUFFLEVBQ3pFLElBQUksQ0FBQyxJQUFNLEtBQUssRUFBRSxDQUFDO01BQUUsU0FBUztJQUFLLElBQ25DLEtBQUssQ0FBQyxDQUFDLElBQU0sS0FBSyxJQUFJLENBQUMsS0FBSyxPQUFPO0FBQ3hDO0FBRUEsU0FBUztFQUFlO0FBQU87QUFFL0IsU0FBUztFQUNQLEtBQUssRUFBRSxDQUFDO0lBQUUsU0FBUztNQUFDO01BQU87TUFBUTtNQUFPO01BQU87TUFBUztNQUFRO0tBQVU7RUFBQztBQUMvRTtBQUVBLGVBQWU7RUFBRTtFQUFLO0VBQU07RUFBSztFQUFLO0VBQU87RUFBTTtBQUFRLEVBQUUifQ==
