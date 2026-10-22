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
        // 等不到空闲连接（池子吃紧，见 db::connect）、和数据库的连接断了：是中控暂时不可用，不是请求的问题，回 503（同 agent 上报接口）
        if matches!(e, sqlx::Error::PoolTimedOut | sqlx::Error::Io(_)) {
            return Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", format!("数据库暂时不可用：{e}"));
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
                        Some("plan_provider_id_name_key") => "这个供应商下已有同名套餐",
                        Some("team_name_key") => "已有同名团队",
                        Some("provider_name_key") => "已有同名供应商",
                        Some("provider_code_key") => "已有同缩写的供应商",
                        Some("router_name_key") => "已经有同名的软路由",
                        _ => "数据冲突",
                    };
                    return Self::conflict(m);
                }
                // foreign_key_violation：按引用方插入/修改处理（400）。两种违反的错误字段相同、报错文字随 lc_messages 变，分不出来；
                // 会硬删/改动被引用父对象的接口要自己把 23503 映射成 409（目前没有：父对象都是软删、级联，或走带 purge_blocker 的作业）
                Some("23503") => return Self::bad_request("引用的对象不存在"),
                // invalid_text_representation：枚举等类型解析失败（如 cycle="weekly"）
                Some("22P02") => return Self::bad_request("填写的值格式不对"),
                // query_canceled：语句超时（访问监控的查询池设了 statement_timeout，见 state::Inner::access_db）
                Some("57014") => {
                    return Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "查询太慢，已中止：缩小时间范围或换更具体的搜索");
                }
                // check_violation：约束名只进日志，不给用户（该校验的字段应在接口里先拦下）
                Some("23514") => {
                    tracing::warn!(constraint = db.constraint().unwrap_or("?"), "违反检查约束");
                    return Self::bad_request("填写的值超出允许范围");
                }
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
    /// 不把 serde 的英文原文给界面：写成中文，能定位时带出字段（serde 的错误路径，如 items[0].ssh_port）
    fn from(r: axum::extract::rejection::JsonRejection) -> Self {
        use axum::extract::rejection::JsonRejection as R;
        Self::bad_request(match &r {
            R::JsonDataError(e) => data_error(e),
            R::JsonSyntaxError(_) => "请求内容不是合法的 JSON".into(),
            R::MissingJsonContentType(_) => "请求要带 Content-Type: application/json".into(),
            _ => "请求内容读取失败".into(),
        })
    }
}

/// JSON 合法但对不上接口的字段：从错误链里找 serde 的错误路径。缺字段时路径指向外层对象，字段名在错误原文的反引号里
fn data_error(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cur = Some(e);
    while let Some(x) = cur {
        if let Some(p) = x.downcast_ref::<serde_path_to_error::Error<serde_json::Error>>() {
            let path = p.path().to_string();
            let inner = p.inner().to_string();
            if let Some(f) = inner.strip_prefix("missing field `").and_then(|s| s.split('`').next()) {
                return if path == "." { format!("请求内容格式不对：缺少字段 {f}") } else { format!("请求内容格式不对：缺少字段 {path}.{f}") };
            }
            if path != "." {
                return format!("请求内容格式不对：字段 {path} 的值类型或范围不对");
            }
            break;
        }
        cur = x.source();
    }
    "请求内容格式不对".into()
}

/// 路径参数提取器：解析失败（如编号不是合法 uuid）时同样返回 JSON 400
#[derive(axum::extract::FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
pub struct ApiPath<T>(pub T);

impl From<axum::extract::rejection::PathRejection> for ApiError {
    fn from(r: axum::extract::rejection::PathRejection) -> Self { Self::bad_request(format!("路径参数不对：{}", r.body_text())) }
}

/// 查询参数提取器：解析失败时同样返回 JSON 400
#[derive(axum::extract::FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
pub struct ApiQuery<T>(pub T);

impl From<axum::extract::rejection::QueryRejection> for ApiError {
    fn from(r: axum::extract::rejection::QueryRejection) -> Self { Self::bad_request(format!("查询参数不对：{}", r.body_text())) }
}

impl From<anyhow::Error> for ApiError {
    /// 包着的是数据库错误（如 agent 接口的 entries::desired_state / on_applied）就按数据库错误映射：连接池超时同样回 503。
    /// 映射成 5xx 时（原因只进日志）留整条上下文链，日志里看得出是哪一步出的错
    fn from(e: anyhow::Error) -> Self {
        let chain = format!("{e:#}");
        match e.downcast::<sqlx::Error>() {
            Ok(db) => {
                let mut a = Self::from(db);
                if a.status.is_server_error() {
                    a.message = chain;
                }
                a
            }
            Err(_) => Self::internal(chain),
        }
    }
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
    async fn db_errors_inside_anyhow_keep_their_status() {
        let (s, v) = body_of(ApiError::from(anyhow::Error::from(sqlx::Error::PoolTimedOut))).await;
        assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("unavailable")));
    }

    /// 请求体对不上接口：中文、带出字段路径，不给 serde 的英文原文
    #[test]
    fn json_rejections_in_chinese_with_field() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct In {
            items: Vec<It>,
        }
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct It {
            kind: String,
            ssh_port: Option<u16>,
        }
        let msg = |b: &str| ApiError::from(axum::Json::<In>::from_bytes(b.as_bytes()).err().unwrap()).message;
        assert_eq!(msg(r#"{"items":[{"kind":"vps","ssh_port":99999}]}"#), "请求内容格式不对：字段 items[0].ssh_port 的值类型或范围不对");
        assert_eq!(msg(r#"{"items":[{"ssh_port":22}]}"#), "请求内容格式不对：缺少字段 items[0].kind");
        assert_eq!(msg(r#"{}"#), "请求内容格式不对：缺少字段 items");
        assert_eq!(msg("{"), "请求内容不是合法的 JSON");
    }

    #[tokio::test]
    async fn client_errors_keep_their_message() {
        let (s, v) = body_of(ApiError::bad_request("端口不对")).await;
        assert_eq!((s, v["error"]["message"].as_str()), (StatusCode::BAD_REQUEST, Some("端口不对")));
    }
}
