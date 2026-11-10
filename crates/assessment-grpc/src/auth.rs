//! Bearer-token authentication for the server.
//!
//! [`BearerAuth`] is a tonic [`Interceptor`]: wrap the service with it
//! (`InterceptedService::new(server, auth)`) and every call must carry
//! `authorization: Bearer <token>`, compared in constant time. Calls without
//! it, or with another token, fail with `UNAUTHENTICATED`. A disabled
//! `BearerAuth` lets every call through, so the service type stays the same
//! whether auth is on or off.

use std::fmt;
use std::sync::Arc;

use subtle::ConstantTimeEq;
use tonic::service::Interceptor;
use tonic::{Request, Status};

/// Requires `authorization: Bearer <token>` on every call (when enabled).
#[derive(Clone)]
pub struct BearerAuth {
    expected: Option<Arc<[u8]>>,
}

impl BearerAuth {
    /// Requires `token` on every call.
    pub fn new(token: impl AsRef<str>) -> Self {
        Self {
            expected: Some(Arc::from(token.as_ref().as_bytes())),
        }
    }

    /// Lets every call through.
    pub fn disabled() -> Self {
        Self { expected: None }
    }

    /// Requires `token` when there is one (an empty token counts as none).
    pub fn from_option(token: Option<&str>) -> Self {
        match token {
            Some(t) if !t.is_empty() => Self::new(t),
            _ => Self::disabled(),
        }
    }

    /// Whether calls need a token.
    pub fn is_enabled(&self) -> bool {
        self.expected.is_some()
    }

    /// Checks the `authorization` metadata of a call.
    pub fn check<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let Some(expected) = &self.expected else {
            return Ok(());
        };
        let header = request
            .metadata()
            .get("authorization")
            .ok_or_else(|| Status::unauthenticated("missing bearer token"))?;
        let value = header
            .to_str()
            .map_err(|_| Status::unauthenticated("malformed authorization header"))?;
        let token = value
            .split_once(' ')
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| token.trim())
            .ok_or_else(|| {
                Status::unauthenticated("the authorization header is not a bearer token")
            })?;
        if bool::from(token.as_bytes().ct_eq(expected)) {
            Ok(())
        } else {
            Err(Status::unauthenticated("invalid bearer token"))
        }
    }
}

impl Default for BearerAuth {
    fn default() -> Self {
        Self::disabled()
    }
}

impl fmt::Debug for BearerAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the token.
        f.debug_struct("BearerAuth")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

impl Interceptor for BearerAuth {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        self.check(&request)?;
        // The handler counts its deadline from here.
        let mut request = request;
        request
            .extensions_mut()
            .insert(crate::service::RequestStart(tokio::time::Instant::now()));
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_header(value: &str) -> Request<()> {
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert("authorization", value.parse().expect("ascii"));
        request
    }

    #[test]
    fn checks_the_bearer_token() {
        let auth = BearerAuth::new("s3cret");
        assert!(auth.check(&with_header("Bearer s3cret")).is_ok());
        assert!(auth.check(&with_header("bearer s3cret")).is_ok());
        assert!(auth.check(&with_header("Bearer s3cre")).is_err());
        assert!(auth.check(&with_header("Basic s3cret")).is_err());
        assert!(auth.check(&with_header("s3cret")).is_err());
        assert!(auth.check(&Request::new(())).is_err());
        assert!(BearerAuth::disabled().check(&Request::new(())).is_ok());
        assert!(!format!("{auth:?}").contains("s3cret"));
    }
}
