// 输出通道分离校验（spec §3.3/§4）：console.log → stdout（无级别前缀）；
// console.error 与 log.* → stderr（带级别标签）。
console.log("stdout-line");
console.error("stderr-line");
log.info("log-info-msg", "k", 1);
