// 登录/登出/会话校验：cookie 解析、登录限速。
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::state::{App, SESSION_SECS};

pub(crate) const COOKIE: &str = "panel_session";

pub(crate) fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v)
}

#[derive(serde::Deserialize)]
pub struct LoginReq {
    password: String,
}

pub async fn login(State(app): State<Arc<App>>, Json(body): Json<LoginReq>) -> Response {
    if let Some(wait) = app.lockout() {
        let msg = format!("失败次数过多，请 {wait} 秒后再试");
        return (StatusCode::TOO_MANY_REQUESTS, msg).into_response();
    }
    if body.password != app.cfg.password {
        app.on_fail();
        return (StatusCode::UNAUTHORIZED, "密码错误").into_response();
    }
    app.reset_fails();

    let Some(tok) = app.new_session() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "无法生成会话").into_response();
    };
    let c = format!("{COOKIE}={tok}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_SECS}");
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, c)]).into_response()
}

pub async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if let Some(t) = cookie_token(&headers) {
        app.sessions.lock().unwrap().remove(t);
    }
    let c = format!("{COOKIE}=; Path=/; Max-Age=0");
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, c)]).into_response()
}

/// 前端拿它判断「cookie 还有效吗」，能进来就说明有效
pub async fn me() -> StatusCode {
    StatusCode::NO_CONTENT
}

pub async fn require_auth(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let ok = cookie_token(req.headers()).is_some_and(|t| app.valid_session(t));
    if ok {
        next.run(req).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}
