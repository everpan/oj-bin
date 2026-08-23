import{requireRole}from"../../user-0.1.0/_shared/validate.js";function get(){const role=requireRole(http.param("role","admin"));db.query(`select o.id, o.no, o.amount, a.name as account_name, a.role
     from orders o join account a on a.id = o.account_id
     where a.role = ? order by o.id`,[role]).then(r=>json.ok(r)).catch(e=>json.fail(500,String(e)));}export default{get};