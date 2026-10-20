use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// API 错误：`{"error":{"code":"...","message":"..."}}`
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub detail: Option<serde_json::Value>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), detail: None }
    }
    pub fn with_detail(mut self, d: serde_json::Value) -> Self {
        self.detail = Some(d);
        self
    }
    pub fn bad_request(m: impl Into<String>) -> Self { Self::new(StatusCode::BAD_REQUEST, "bad_request", m) }
    pub fn not_found(m: impl Into<String>) -> Self { Self::new(StatusCode::NOT_FOUND, "not_found", m) }
    pub fn conflict(m: impl Into<String>) -> Self { Self::new(StatusCode::CONFLICT, "conflict", m) }
    pub fn unauthorized() -> Self { Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "需要登录") }
    pub fn forbidden(m: impl Into<String>) -> Self { Self::new(StatusCode::FORBIDDEN, "forbidden", m) }
    pub fn internal(m: impl Into<String>) -> Self { Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", m) }
}

/// 8 位十六进制短编号
fn error_id() -> String { uuid::Uuid::new_v4().simple().to_string()[..8].to_string() }

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // 5xx：完整原因只进日志，对客户端只给通用提示和一个短编号（用来对日志）
        let message = if self.status.is_server_error() {
            let eid = error_id();
            tracing::error!(code = self.code, error_id = %eid, "{}", self.message);
            format!("服务器内部错误（编号 {eid}），请把编号告诉管理员查看日志")
        } else {
            self.message
        };
        (self.status, Json(json!({"error": {"code": self.code, "message": message, "detail": self.detail}}))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::RowNotFound = e {
            return Self::not_found("对象不存在");
        }
        if let Some(db) = e.as_database_error() {
            match db.code().as_deref() {
                // unique_violation：把约束名翻译成人话
                Some("23505") => {
                    let m = match db.constraint() {
                        Some("machine_ip_live") => "这个 IP 已经属于一台未退役的机器",
                        Some("machine_number_live") => "这个团队里已经有这个编号",
                        Some("machine_label_live") => "已经有同名线路机",
                        Some("entry_port_live") => "线路机上这个端口已被占用",
                        Some("entry_node_relay_live") => "这个节点在这台线路机上已经有入口",
                        Some("job_one_active_per_target") => "这台机器还有进行中的作业",
                        _ => "数据冲突",
                    };
                    return Self::conflict(m);
                }
                Some("23503") => return Self::conflict("被其他数据引用，不能这样改"),
                Some("23514") => return Self::bad_request(format!("字段不合法（{}）", db.constraint().unwrap_or("check"))),
                _ => {}
            }
        }
        Self::internal(format!("数据库错误：{e}"))
    }
}

/// 请求体 JSON 提取器：解析失败时也按统一的 `{"error":…}` 格式返回 400
#[derive(axum::extract::FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct ApiJson<T>(pub T);

impl From<axum::extract::rejection::JsonRejection> for ApiError {
    fn from(r: axum::extract::rejection::JsonRejection) -> Self { Self::bad_request(format!("请求内容不对：{}", r.body_text())) }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self { Self::internal(format!("{e:#}")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    async fn body_of(e: ApiError) -> (StatusCode, serde_json::Value) {
        let res = e.into_response();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn server_errors_hide_internal_details() {
        for e in [
            ApiError::from(anyhow::anyhow!("连接 10.0.0.5 失败，password=hunter2")),
            ApiError::from(sqlx::Error::Protocol("relation \"secret_table\" does not exist".into())),
        ] {
            let (s, v) = body_of(e).await;
            assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
            let m = v["error"]["message"].as_str().unwrap();
            assert!(m.contains("服务器内部错误") && m.contains("编号"), "{m}");
            assert!(!m.contains("hunter2") && !m.contains("secret_table") && !m.contains("10.0.0.5"), "{m}");
        }
    }

    #[tokio::test]
    async fn client_errors_keep_their_message() {
        let (s, v) = body_of(ApiError::bad_request("端口不对")).await;
        assert_eq!((s, v["error"]["message"].as_str()), (StatusCode::BAD_REQUEST, Some("端口不对")));
    }
}
