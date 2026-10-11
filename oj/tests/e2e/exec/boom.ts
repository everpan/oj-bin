// 失败路径：顶层 throw → 退出码 1，stderr 携带 V8 异常消息（不吞不改写）。
throw new Error("EXEC_BOOM_MARKER");
