//! NDJSON 帧编解码（design spec §5.3）：增量解帧 + 请求编码 + 响应解析。
//! 行上限 64 MiB——超限/非 UTF8 都是协议损坏，显式报 FrameError（T9 据此刻 sidecar 判死）。

use serde_json::Value;

/// 单行上限（spec §6.1）。JDBC 大结果行经 stream 分批，单帧不应逼近此值。
pub const MAX_LINE: usize = 64 * 1024 * 1024;

/// 一帧 = 一行（不含行尾 `\n`/`\r\n`）。
pub type Line = String;

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// 行超上限：sidecar 行为异常（不回换行/喷垃圾），判死依据。
    TooLong,
    /// 行不是合法 UTF-8：不 panic、不 lossy 替换——协议损坏要显式可见。
    NotUtf8,
    /// 行是 UTF-8 但不是合法协议 JSON / 缺必需键。
    Json(String),
}

/// 增量解帧器：push 任意字节分片，按 `\n` 切出完整行。
/// 出错后 reader 不可复用（缓冲可能残留半帧）——调用方（T9）直接判死 sidecar。
pub struct FrameReader {
    buf: Vec<u8>,
    max_line: usize,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    pub fn new() -> Self {
        Self::with_max(MAX_LINE)
    }

    pub fn with_max(max_line: usize) -> Self {
        Self {
            buf: Vec::new(),
            max_line,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Line>, FrameError> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        // 每轮从缓冲头找 `\n`；完整行切出后做长度与 UTF-8 校验。
        while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line = self.buf.drain(..=nl).collect::<Vec<_>>();
            line.pop(); // '\n'
            if line.len() > self.max_line {
                return Err(FrameError::TooLong);
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            out.push(String::from_utf8(line).map_err(|_| FrameError::NotUtf8)?);
        }
        // 无换行尾的半行超上限同样判死——不能等一个永不到来的 `\n`。
        if self.buf.len() > self.max_line {
            return Err(FrameError::TooLong);
        }
        Ok(out)
    }
}

/// 编码请求帧：`{"id":..,"op":..,...params}` 单行 + `\n`（NDJSON 终止符由编码器保证）。
pub fn encode_request(id: u64, op: &str, params: &serde_json::Map<String, Value>) -> Line {
    let mut obj = serde_json::Map::with_capacity(params.len() + 2);
    obj.insert("id".into(), Value::from(id));
    obj.insert("op".into(), Value::from(op));
    for (k, v) in params {
        obj.insert(k.clone(), v.clone());
    }
    let mut s = Value::Object(obj).to_string();
    s.push('\n');
    s
}

/// 响应帧（spec §5.3）：`{"id":1,"ok":{...}}` 或 `{"id":1,"err":...}`。
/// err 载荷支持字符串形态与 §5.8 结构化形态（`{message,sql_state,error_code}`）；
/// 未知键一律忽略（协议演进契约）。
#[derive(Debug, PartialEq)]
pub enum Response {
    Ok {
        id: u64,
        value: Value,
    },
    Err {
        id: u64,
        message: String,
        sql_state: Option<String>,
        error_code: Option<i32>,
    },
}

fn bad(line: &str, why: &str) -> FrameError {
    // 按 char 截断：sidecar 是不可信输入源，按字节切多字节码点会 panic（不 panic 红线）。
    let snippet: String = line.chars().take(64).collect();
    FrameError::Json(format!("{why}: {snippet}"))
}

pub fn parse_response(line: &str) -> Result<Response, FrameError> {
    let v: Value = serde_json::from_str(line).map_err(|e| bad(line, &e.to_string()))?;
    let obj = v.as_object().ok_or_else(|| bad(line, "not an object"))?;
    let id = obj
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad(line, "missing id"))?;
    // err 优先于 ok（同帧双键视为协议异常，按出错处理）。
    if let Some(err) = obj.get("err") {
        let (message, sql_state, error_code) = match err {
            Value::String(m) => (m.clone(), None, None),
            Value::Object(m) => (
                m.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                m.get("sql_state")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                m.get("error_code")
                    .and_then(Value::as_i64)
                    .and_then(|c| i32::try_from(c).ok()),
            ),
            _ => return Err(bad(line, "err neither string nor object")),
        };
        return Ok(Response::Err {
            id,
            message,
            sql_state,
            error_code,
        });
    }
    if let Some(ok) = obj.get("ok") {
        return Ok(Response::Ok {
            id,
            value: ok.clone(),
        });
    }
    Err(bad(line, "neither ok nor err"))
}
