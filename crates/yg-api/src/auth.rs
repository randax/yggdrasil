//! Authentication and the admin scope gate. Authorization itself lives
//! in the route table (issue #38): these middlewares decide *who* is
//! calling, the router's shape decides what they may reach.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::error::{ApiError, error_json};

/// What a bearer token is scoped to, decided once by [`authenticate`] and
/// carried in request extensions. Authorization itself lives in the route
/// table: admin routes check for [`TokenScope::Admin`] via [`require_admin`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TokenScope {
    Admin,
    Member,
}

/// Every route behind the auth layer requires a bearer token. The
/// bootstrap token is Admin-scoped; a stored member token is
/// Member-scoped. This middleware only authenticates and records the
/// scope — which routes a scope may reach is the router's shape.
pub(crate) async fn authenticate(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    use subtle::ConstantTimeEq;

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        // RFC 9110: the scheme is case-insensitive.
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        // RFC 9110 allows 1*SP between scheme and credentials.
        .map(|(_, presented)| presented.trim_start_matches(' ').to_string());

    let Some(presented) = presented else {
        return error_json(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
    };

    let admin: bool = presented
        .as_bytes()
        .ct_eq(state.bootstrap_token.as_bytes())
        .into();
    let scope = if admin {
        TokenScope::Admin
    } else {
        match state.control.authenticate_member_token(&presented).await {
            Ok(true) => TokenScope::Member,
            Ok(false) => {
                return error_json(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
            }
            Err(e) => return ApiError::internal(e.context("auth lookup failed")).into_response(),
        }
    };
    req.extensions_mut().insert(scope);
    next.run(req).await
}

/// The admin router's gate: any authenticated caller reaches it, only an
/// Admin token passes.
pub(crate) async fn require_admin(req: Request, next: Next) -> Response {
    match req.extensions().get::<TokenScope>() {
        Some(TokenScope::Admin) => next.run(req).await,
        _ => error_json(
            StatusCode::FORBIDDEN,
            "this operation requires an Admin token; member tokens may call \
             Verbs and the read-only status route",
        ),
    }
}
