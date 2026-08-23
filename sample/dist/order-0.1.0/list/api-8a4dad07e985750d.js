import { requireRole } from "../../user-0.1.0/_shared/validate.js";
function get() {
  const role = requireRole(http.param("role", "admin")); // 跨模块相对导入（UC-13）
  db.query(`select o.id, o.no, o.amount, a.name as account_name, a.role
     from orders o join account a on a.id = o.account_id
     where a.role = ? order by o.id`, [
    role
  ]).then((r)=>json.ok(r)).catch((e)=>json.fail(500, String(e)));
}
export default {
  get
};
//# sourceMappingURL=data:application/json;base64,eyJ2ZXJzaW9uIjozLCJzb3VyY2VzIjpbImZpbGU6Ly8vVXNlcnMvZXZlci9naXQvZ29sYW5nL21kbS1iYXNlLXJ1c3Qvc2FtcGxlL3NyYy9vcmRlci9saXN0L2FwaS50cyJdLCJzb3VyY2VzQ29udGVudCI6WyJpbXBvcnQgeyByZXF1aXJlUm9sZSB9IGZyb20gXCIuLi8uLi91c2VyL19zaGFyZWQvdmFsaWRhdGVcIjtcblxuZnVuY3Rpb24gZ2V0KCk6IHZvaWQge1xuICBjb25zdCByb2xlID0gcmVxdWlyZVJvbGUoaHR0cC5wYXJhbShcInJvbGVcIiwgXCJhZG1pblwiKSk7IC8vIOi3qOaooeWdl+ebuOWvueWvvOWFpe+8iFVDLTEz77yJXG4gIGRiLnF1ZXJ5KFxuICAgIGBzZWxlY3Qgby5pZCwgby5ubywgby5hbW91bnQsIGEubmFtZSBhcyBhY2NvdW50X25hbWUsIGEucm9sZVxuICAgICBmcm9tIG9yZGVycyBvIGpvaW4gYWNjb3VudCBhIG9uIGEuaWQgPSBvLmFjY291bnRfaWRcbiAgICAgd2hlcmUgYS5yb2xlID0gPyBvcmRlciBieSBvLmlkYCxcbiAgICBbcm9sZV0sXG4gIClcbiAgICAudGhlbigocikgPT4ganNvbi5vayhyKSlcbiAgICAuY2F0Y2goKGUpID0+IGpzb24uZmFpbCg1MDAsIFN0cmluZyhlKSkpO1xufVxuXG5leHBvcnQgZGVmYXVsdCB7IGdldCB9O1xuIl0sIm5hbWVzIjpbXSwibWFwcGluZ3MiOiJBQUFBLFNBQVMsV0FBVyxRQUFRLDhCQUE4QjtBQUUxRCxTQUFTO0VBQ1AsTUFBTSxPQUFPLFlBQVksS0FBSyxLQUFLLENBQUMsUUFBUSxXQUFXLGlCQUFpQjtFQUN4RSxHQUFHLEtBQUssQ0FDTixDQUFDOzttQ0FFOEIsQ0FBQyxFQUNoQztJQUFDO0dBQUssRUFFTCxJQUFJLENBQUMsQ0FBQyxJQUFNLEtBQUssRUFBRSxDQUFDLElBQ3BCLEtBQUssQ0FBQyxDQUFDLElBQU0sS0FBSyxJQUFJLENBQUMsS0FBSyxPQUFPO0FBQ3hDO0FBRUEsZUFBZTtFQUFFO0FBQUksRUFBRSJ9
