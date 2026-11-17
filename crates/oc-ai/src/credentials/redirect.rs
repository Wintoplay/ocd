//! A redirect URL a user pasted when the browser could not reach this machine's callback server (SSH, a remote
//! desktop, a container): `OauthRedirect` (`packages/core/src/oauth/redirect.ts`).
//!
//! It must carry the sign-in's `state`, the same check the browser's redirect passes. A failure here rejects only
//! this paste, so the sign-in keeps waiting; an `error` the provider put in a valid redirect ends the sign-in as it
//! would in the browser.

use std::sync::Arc;

use super::callback::Once;
use super::integration::{FlowError, FlowResult, SubmitFn};

/// `URLSearchParams.get`/`has` over a callback request or a pasted URL.
pub trait Params {
    fn param(&self, name: &str) -> Option<String>;
    fn has_param(&self, name: &str) -> bool;
}

impl Params for super::callback::Request {
    fn param(&self, name: &str) -> Option<String> {
        super::callback::Request::param(self, name)
    }
    fn has_param(&self, name: &str) -> bool {
        super::callback::Request::has_param(self, name)
    }
}

/// The query of a pasted redirect URL.
#[derive(Debug, Clone)]
pub struct Pasted {
    query: String,
}

impl Params for Pasted {
    fn param(&self, name: &str) -> Option<String> {
        url::form_urlencoded::parse(self.query.as_bytes()).find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
    }
    fn has_param(&self, name: &str) -> bool {
        url::form_urlencoded::parse(self.query.as_bytes()).any(|(k, _)| k == name)
    }
}

/// What a valid pasted redirect carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Code(String),
    /// `error_description ?? error`: ends the sign-in.
    Error(String),
}

/// `pasted(text, state)`: `Err` rejects only this paste.
pub fn pasted(text: &str, state: &str) -> Result<(Pasted, Outcome), String> {
    let Ok(url) = url::Url::parse(text.trim()) else {
        return Err("Paste the full URL from the browser's address bar".into());
    };
    let params = Pasted { query: url.query().unwrap_or("").to_string() };
    if params.param("state").as_deref() != Some(state) {
        return Err("This URL belongs to a different sign-in".into());
    }
    if let Some(error) = params.param("error_description").or_else(|| params.param("error")).filter(|e| !e.is_empty()) {
        return Ok((params, Outcome::Error(error)));
    }
    let Some(code) = params.param("code").filter(|c| !c.is_empty()) else {
        return Err("The URL does not contain an authorization code".into());
    };
    Ok((params, Outcome::Code(code)))
}

/// `submit({state, callback, received})`: an automatic sign-in's `submit`. It reads the pasted redirect, then
/// settles the sign-in's pending callback as the browser's redirect would; `received` turns the redirect into that
/// value. A callback that already settled is left as it is.
pub fn submit<T, F>(state: String, callback: Arc<Once<FlowResult<T>>>, received: F) -> SubmitFn
where
    T: Send + 'static,
    F: Fn(String, &Pasted) -> T + Send + Sync + 'static,
{
    Arc::new(move |text: String| {
        let result = pasted(&text, &state).map(|(params, outcome)| {
            callback.complete(match outcome {
                Outcome::Error(error) => Err(FlowError::new(error)),
                Outcome::Code(code) => Ok(received(code, &params)),
            });
        });
        Box::pin(async move { result })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_redirects() {
        let reject = |text: &str| pasted(text, "st").unwrap_err();
        assert_eq!(reject("abc"), "Paste the full URL from the browser's address bar");
        assert_eq!(reject("http://127.0.0.1:1/cb?code=c&state=other"), "This URL belongs to a different sign-in");
        assert_eq!(reject("http://127.0.0.1:1/cb?code=c"), "This URL belongs to a different sign-in");
        assert_eq!(reject("http://127.0.0.1:1/cb?state=st"), "The URL does not contain an authorization code");
        assert_eq!(reject("http://127.0.0.1:1/cb?state=st&code="), "The URL does not contain an authorization code");
        let (params, outcome) = pasted(" http://127.0.0.1:1/cb?code=c%201&state=st&iss=x\n", "st").unwrap();
        assert_eq!(outcome, Outcome::Code("c 1".into()));
        assert_eq!(params.param("iss").as_deref(), Some("x"));
        let (_, outcome) = pasted("http://h/cb?error=access_denied&error_description=No&state=st", "st").unwrap();
        assert_eq!(outcome, Outcome::Error("No".into()));
        let (_, outcome) = pasted("http://h/cb?error=access_denied&state=st", "st").unwrap();
        assert_eq!(outcome, Outcome::Error("access_denied".into()));
        // An empty description is no error (`??` keeps it, and an empty string is falsy).
        assert_eq!(pasted("http://h/cb?error_description=&error=x&state=st&code=c", "st").unwrap().1, Outcome::Code("c".into()));
    }

    #[tokio::test]
    async fn submit_settles_the_callback_once() {
        let (once, rx) = Once::<FlowResult<(String, Option<String>)>>::new();
        let submit = submit("st".into(), once, |code, params| (code, params.param("client_id")));
        assert_eq!(submit("nope".into()).await.unwrap_err(), "Paste the full URL from the browser's address bar");
        submit("http://h/cb?code=c&client_id=app&state=st".into()).await.unwrap();
        // A later paste finds the callback settled and changes nothing.
        submit("http://h/cb?error=late&state=st".into()).await.unwrap();
        assert_eq!(rx.await.unwrap().unwrap(), ("c".to_string(), Some("app".to_string())));
    }
}
