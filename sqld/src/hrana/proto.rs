//! Messages in the Hrana protocol.
//!
//! Please consult the Hrana specification in the `docs/` directory for more information.
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello { jwt: Option<String> },
    Request { request_id: i32, request: Request },
}

#[derive(Serialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    HelloOk {},
    HelloError { error: Error },
    ResponseOk { request_id: i32, response: Response },
    ResponseError { request_id: i32, error: Error },
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    OpenStream(OpenStreamReq),
    CloseStream(OpenStreamReq),
    Compute(ComputeReq),
    Execute(ExecuteReq),
}

#[derive(Serialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    OpenStream(OpenStreamResp),
    CloseStream(CloseStreamResp),
    Compute(ComputeResp),
    Execute(ExecuteResp),
}

#[derive(Deserialize, Debug)]
pub struct OpenStreamReq {
    pub stream_id: i32,
}

#[derive(Serialize, Debug)]
pub struct OpenStreamResp {}

#[derive(Deserialize, Debug)]
pub struct CloseStreamReq {
    pub stream_id: i32,
}

#[derive(Serialize, Debug)]
pub struct CloseStreamResp {}

#[derive(Deserialize, Debug)]
pub struct ComputeReq {
    pub ops: Vec<ComputeOp>,
}

#[derive(Serialize, Debug)]
pub struct ComputeResp {
    pub results: Vec<Arc<Value>>,
}

#[derive(Deserialize, Debug)]
pub struct ExecuteReq {
    pub stream_id: i32,
    pub stmt: Stmt,
    #[serde(default)]
    pub condition: Option<ComputeExpr>,
    #[serde(default)]
    pub on_ok: Vec<ComputeOp>,
    #[serde(default)]
    pub on_error: Vec<ComputeOp>,
}

#[derive(Serialize, Debug)]
pub struct ExecuteResp {
    pub result: Option<StmtResult>,
}

#[derive(Deserialize, Debug)]
pub struct Stmt {
    pub sql: String,
    #[serde(default)]
    pub args: Vec<Value>,
    #[serde(default)]
    pub named_args: Vec<NamedArg>,
    pub want_rows: bool,
}

#[derive(Deserialize, Debug)]
pub struct NamedArg {
    pub name: String,
    pub value: Value,
}

#[derive(Serialize, Debug)]
pub struct StmtResult {
    pub cols: Vec<Col>,
    pub rows: Vec<Vec<Value>>,
    pub affected_row_count: u64,
    #[serde(with = "option_i64_as_str")]
    pub last_insert_rowid: Option<i64>,
}

#[derive(Serialize, Debug)]
pub struct Col {
    pub name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Value {
    Null,
    Integer {
        #[serde(with = "i64_as_str")]
        value: i64,
    },
    Float {
        value: f64,
    },
    Text {
        value: String,
    },
    Blob {
        #[serde(with = "bytes_as_base64", rename = "base64")]
        value: Vec<u8>,
    },
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ComputeOp {
    Set { var: i32, expr: ComputeExpr },
    Unset { var: i32 },
    Eval { expr: ComputeExpr },
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum ComputeExpr {
    Value(Arc<Value>),
    Expr(ComputeExpr_),
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ComputeExpr_ {
    Var { var: i32 },
    Not { expr: Box<ComputeExpr> },
}

#[derive(Serialize, Debug)]
pub struct Error {
    pub message: String,
}

mod i64_as_str {
    use serde::{de, ser};
    use serde::{de::Error as _, Serialize as _};

    pub fn serialize<S: ser::Serializer>(value: &i64, ser: S) -> Result<S::Ok, S::Error> {
        value.to_string().serialize(ser)
    }

    pub fn deserialize<'de, D: de::Deserializer<'de>>(de: D) -> Result<i64, D::Error> {
        let str_value = <&'de str as de::Deserialize>::deserialize(de)?;
        str_value.parse().map_err(|_| {
            D::Error::invalid_value(
                de::Unexpected::Str(str_value),
                &"decimal integer as a string",
            )
        })
    }
}

mod option_i64_as_str {
    use serde::{ser, Serialize as _};

    pub fn serialize<S: ser::Serializer>(value: &Option<i64>, ser: S) -> Result<S::Ok, S::Error> {
        value.map(|v| v.to_string()).serialize(ser)
    }
}

mod bytes_as_base64 {
    use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
    use serde::{de, ser};
    use serde::{de::Error as _, Serialize as _};

    pub fn serialize<S: ser::Serializer>(value: &Vec<u8>, ser: S) -> Result<S::Ok, S::Error> {
        STANDARD_NO_PAD.encode(value).serialize(ser)
    }

    pub fn deserialize<'de, D: de::Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let str_value = <&'de str as de::Deserialize>::deserialize(de)?;
        STANDARD_NO_PAD
            .decode(str_value.trim_end_matches('='))
            .map_err(|_| {
                D::Error::invalid_value(
                    de::Unexpected::Str(str_value),
                    &"binary data encoded as base64",
                )
            })
    }
}
