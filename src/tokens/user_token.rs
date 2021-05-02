use crate::ClientSecret;
use crate::{
    id::TwitchTokenErrorResponse,
    tokens::{
        errors::{RefreshTokenError, UserTokenExchangeError, ValidationError},
        Scope, TwitchToken,
    },
};

use oauth2::{AccessToken, ClientId, RedirectUrl, RefreshToken};
use oauth2::{HttpRequest, HttpResponse};
use std::future::Future;

use super::errors::ImplicitUserTokenExchangeError;

/// An User Token from the [OAuth implicit code flow](https://dev.twitch.tv/docs/authentication/getting-tokens-oauth#oauth-implicit-code-flow) or [OAuth authorization code flow](https://dev.twitch.tv/docs/authentication/getting-tokens-oauth#oauth-authorization-code-flow)
///
/// Used for requests that need an authenticated user. See also [`AppAccessToken`](super::AppAccessToken)
///
/// See [`UserToken::builder`](UserTokenBuilder::new) for authenticating the user using the `OAuth authorization code flow`.
#[derive(Clone)]
pub struct UserToken {
    /// The access token used to authenticate requests with
    pub access_token: AccessToken,
    client_id: ClientId,
    client_secret: Option<ClientSecret>,
    /// Username of user associated with this token
    pub login: String,
    /// User ID of the user associated with this token
    pub user_id: String,
    /// The refresh token used to extend the life of this user token
    pub refresh_token: Option<RefreshToken>,
    /// Expiration from when the response was generated.
    expires_in: std::time::Duration,
    /// When this struct was created, not when token was created.
    struct_created: std::time::Instant,
    scopes: Vec<Scope>,
    /// Token will never expire
    ///
    /// This is only true for old client IDs, like <https://twitchapps.com/tmi> and others
    pub never_expiring: bool,
}

impl std::fmt::Debug for UserToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserToken")
            .field("access_token", &self.access_token)
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret)
            .field("login", &self.login)
            .field("user_id", &self.user_id)
            .field("refresh_token", &self.refresh_token)
            .field("expires_in", &self.expires_in())
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl UserToken {
    /// Assemble token without checks.
    ///
    /// If `expires_in` is `None`, we'll assume `token.is_elapsed` is always false
    #[allow(clippy::too_many_arguments)]
    pub fn from_existing_unchecked(
        access_token: impl Into<AccessToken>,
        refresh_token: impl Into<Option<RefreshToken>>,
        client_id: impl Into<ClientId>,
        client_secret: impl Into<Option<ClientSecret>>,
        login: String,
        user_id: String,
        scopes: Option<Vec<Scope>>,
        expires_in: Option<std::time::Duration>,
    ) -> UserToken {
        UserToken {
            access_token: access_token.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            login,
            user_id,
            refresh_token: refresh_token.into(),
            expires_in: expires_in.unwrap_or_else(|| {
                // TODO: Use Duration::MAX
                std::time::Duration::new(u64::MAX, 1_000_000_000 - 1)
            }),
            struct_created: std::time::Instant::now(),
            scopes: scopes.unwrap_or_default(),
            never_expiring: expires_in.is_none(),
        }
    }

    /// Assemble token and validate it. Retrieves [`login`](TwitchToken::login), [`client_id`](TwitchToken::client_id) and [`scopes`](TwitchToken::scopes)
    ///
    /// If the token is already expired, this function will fail to produce a [`UserToken`] and return [`ValidationError::NotAuthorized`]
    pub async fn from_existing<RE, C, F>(
        http_client: C,
        access_token: AccessToken,
        refresh_token: impl Into<Option<RefreshToken>>,
        client_secret: impl Into<Option<ClientSecret>>,
    ) -> Result<UserToken, ValidationError<RE>>
    where
        RE: std::error::Error + Send + Sync + 'static,
        C: FnOnce(HttpRequest) -> F,
        F: Future<Output = Result<HttpResponse, RE>>,
    {
        let validated = crate::validate_token(http_client, &access_token).await?;
        Ok(Self::from_existing_unchecked(
            access_token,
            refresh_token.into(),
            validated.client_id,
            client_secret,
            validated.login.ok_or(ValidationError::NoLogin)?,
            validated.user_id.ok_or(ValidationError::NoLogin)?,
            validated.scopes,
            Some(validated.expires_in).filter(|d| {
                // FIXME: https://github.com/rust-lang/rust/pull/84084
                // FIXME: nanos are not returned
                // if duration is zero, the token will never expire. if the token was expired, twitch would return NotAuthorized
                // TODO: There could be a situation where this fails, if the token is just about to expire, say 500ms, does twitch round up to 1 or down to 0?
                !(d.as_secs() == 0 && d.as_nanos() == 0)
            }),
        ))
    }

    #[doc(hidden)]
    /// Returns true if this token is never expiring. Needs to be assembled manually, we never assume a token is never expiring. See [`UserToken::from_existing_unexpiring`]
    ///
    /// Hidden because it's not expected to be used.
    pub fn never_expires(&self) -> bool { self.never_expiring }

    /// Create a [`UserTokenBuilder`] to get a token with the [OAuth Authorization Code](https://dev.twitch.tv/docs/authentication/getting-tokens-oauth/#oauth-authorization-code-flow)
    pub fn builder(
        client_id: ClientId,
        client_secret: ClientSecret,
        redirect_url: RedirectUrl,
    ) -> Result<UserTokenBuilder, oauth2::url::ParseError> {
        UserTokenBuilder::new(client_id, client_secret, redirect_url)
    }

    /// Set the client secret
    pub fn set_secret(&mut self, secret: Option<ClientSecret>) { self.client_secret = secret }
}

#[async_trait::async_trait]
impl TwitchToken for UserToken {
    fn token_type() -> super::BearerTokenType { super::BearerTokenType::UserToken }

    fn client_id(&self) -> &ClientId { &self.client_id }

    fn token(&self) -> &AccessToken { &self.access_token }

    fn login(&self) -> Option<&str> { Some(&self.login) }

    fn user_id(&self) -> Option<&str> { Some(&self.user_id) }

    async fn refresh_token<RE, C, F>(
        &mut self,
        http_client: C,
    ) -> Result<(), RefreshTokenError<RE>>
    where
        Self: Sized,
        RE: std::error::Error + Send + Sync + 'static,
        C: FnOnce(HttpRequest) -> F + Send,
        F: Future<Output = Result<HttpResponse, RE>> + Send,
    {
        if let Some(client_secret) = self.client_secret.clone() {
            let (access_token, expires, refresh_token) = if let Some(token) =
                self.refresh_token.take()
            {
                crate::refresh_token(http_client, token, &self.client_id, &client_secret).await?
            } else {
                return Err(RefreshTokenError::NoRefreshToken);
            };
            self.access_token = access_token;
            self.expires_in = expires;
            self.refresh_token = refresh_token;
            Ok(())
        } else {
            return Err(RefreshTokenError::NoClientSecretFound);
        }
    }

    fn expires_in(&self) -> std::time::Duration {
        if !self.never_expiring {
            self.expires_in
                .checked_sub(self.struct_created.elapsed())
                .unwrap_or_default()
        } else {
            // We don't return an option here because it's not expected to use this if the token is known to be unexpiring.
            // TODO: Use Duration::MAX
            std::time::Duration::new(u64::MAX, 1_000_000_000 - 1)
        }
    }

    fn scopes(&self) -> &[Scope] { self.scopes.as_slice() }
}

/// Builder for [OAuth authorization code flow](https://dev.twitch.tv/docs/authentication/getting-tokens-oauth/#oauth-authorization-code-flow)
///
/// See [`ImplicitUserTokenBuilder`] for the [OAuth implicit code flow](https://dev.twitch.tv/docs/authentication/getting-tokens-oauth/#oauth-implicit-code-flow) (does not require Client Secret)
pub struct UserTokenBuilder {
    pub(crate) scopes: Vec<Scope>,
    pub(crate) client: crate::TwitchClient,
    pub(crate) csrf: Option<oauth2::CsrfToken>,
    pub(crate) force_verify: bool,
    pub(crate) redirect_url: RedirectUrl,
    client_id: ClientId,
    client_secret: ClientSecret,
}

impl UserTokenBuilder {
    /// Create a [`UserTokenBuilder`]
    ///
    /// # Notes
    ///
    /// The `url` crate converts empty paths into "/" (such as `https://example.com` into `https://example.com/`),
    /// which means that you'll need to add "https://example.com/" to your redirect URIs (note the "trailing" slash) if you want to use an empty path.
    ///
    /// To avoid this, use a path such as `https://example.com/twitch/register` or similar instead, where the `url` crate would not add a trailing `/`.
    pub fn new(
        client_id: ClientId,
        client_secret: ClientSecret,
        redirect_url: RedirectUrl,
    ) -> Result<UserTokenBuilder, oauth2::url::ParseError> {
        Ok(UserTokenBuilder {
            scopes: vec![],
            client: crate::TwitchClient::new(
                client_id.clone(),
                Some(client_secret.clone()),
                oauth2::AuthUrl::new(crate::AUTH_URL.to_string())?,
                Some(oauth2::TokenUrl::new(crate::TOKEN_URL.to_string())?),
            )
            .set_auth_type(oauth2::AuthType::BasicAuth)
            .set_redirect_uri(redirect_url.clone()),
            csrf: None,
            force_verify: false,
            redirect_url,
            client_id,
            client_secret,
        })
    }

    /// Add scopes to the request
    pub fn set_scopes(mut self, scopes: Vec<Scope>) -> Self {
        self.scopes = scopes;
        self
    }

    /// Add a single scope to request
    pub fn add_scope(&mut self, scope: Scope) { self.scopes.push(scope); }

    /// Enable or disable function to make the user able to switch accounts if needed.
    pub fn force_verify(mut self, b: bool) -> Self {
        self.force_verify = b;
        self
    }

    /// Generate the URL to request a code.
    ///
    /// Step 1. in the [guide](https://dev.twitch.tv/docs/authentication/getting-tokens-oauth/#oauth-authorization-code-flow)
    pub fn generate_url(&mut self) -> (oauth2::url::Url, oauth2::CsrfToken) {
        let mut auth = self.client.authorize_url(oauth2::CsrfToken::new_random);

        for scope in self.scopes.iter() {
            auth = auth.add_scope(scope.as_oauth_scope())
        }

        auth = auth.add_extra_param(
            "force_verify",
            if self.force_verify { "true" } else { "false" },
        );

        let (url, csrf) = auth.url();
        self.csrf = Some(csrf.clone());
        (url, csrf)
    }

    /// Set the CSRF token.
    ///
    /// Hidden because you should preferably not use this.
    #[doc(hidden)]
    pub fn set_csrf(&mut self, csrf: oauth2::CsrfToken) { self.csrf = Some(csrf); }

    /// Generate the code with the help of the authorization code
    ///
    /// Step. 3 and 4 in the [guide](https://dev.twitch.tv/docs/authentication/getting-tokens-oauth/#oauth-authorization-code-flow)
    pub async fn get_user_token<RE, C, F>(
        self,
        http_client: C,
        state: &str,
        // TODO: Should be either str or AuthorizationCode
        code: &str,
    ) -> Result<UserToken, UserTokenExchangeError<RE>>
    where
        RE: std::error::Error + Send + Sync + 'static,
        C: Copy + FnOnce(HttpRequest) -> F,
        F: Future<Output = Result<HttpResponse, RE>>,
    {
        if let Some(csrf) = self.csrf {
            if csrf.secret() != state {
                return Err(UserTokenExchangeError::StateMismatch);
            }
        } else {
            return Err(UserTokenExchangeError::StateMismatch);
        }

        // FIXME: self.client.exchange_code(code) does not work as oauth2 currently only sends it in body as per spec, but twitch uses query params.
        use oauth2::http::{HeaderMap, Method, StatusCode};
        use std::collections::HashMap;
        let mut params = HashMap::new();
        params.insert("client_id", self.client_id.as_str());
        params.insert("client_secret", self.client_secret.secret().as_str());
        params.insert("code", code);
        params.insert("grant_type", "authorization_code");
        params.insert("redirect_uri", self.redirect_url.as_str());
        let req = HttpRequest {
            url: oauth2::url::Url::parse_with_params(crate::TOKEN_URL, &params)
                .expect("unexpectedly failed to parse revoke url"),
            method: Method::POST,
            headers: HeaderMap::new(),
            body: vec![],
        };

        let resp = http_client(req)
            .await
            .map_err(UserTokenExchangeError::RequestError)?;
        match resp.status_code {
            StatusCode::OK => (),
            c if c == StatusCode::BAD_REQUEST || c == StatusCode::FORBIDDEN => {
                return Err(UserTokenExchangeError::TwitchError(serde_json::from_slice(
                    resp.body.as_slice(),
                )?));
            }
            c => {
                return Err(UserTokenExchangeError::TwitchError(
                    TwitchTokenErrorResponse {
                        status: c,
                        // This is not returned as I'm unsure what could be contained
                        message: "<censored>".to_string(),
                    },
                ));
            }
        };
        let response: crate::id::TwitchTokenResponse<
            oauth2::EmptyExtraTokenFields,
            oauth2::basic::BasicTokenType,
        > = serde_json::from_slice(resp.body.as_slice())?;
        UserToken::from_existing(
            http_client,
            response.access_token,
            response.refresh_token,
            self.client_secret,
        )
        .await
        .map_err(Into::into)
    }
}

/// Builder for [OAuth implicit code flow](https://dev.twitch.tv/docs/authentication/getting-tokens-oauth/#oauth-implicit-code-flow)
///
/// See [`UserTokenBuilder`] for the [OAuth authorization code flow](https://dev.twitch.tv/docs/authentication/getting-tokens-oauth/#oauth-authorization-code-flow) (requires Client Secret, generally more secure)
pub struct ImplicitUserTokenBuilder {
    pub(crate) scopes: Vec<Scope>,
    pub(crate) client: crate::TwitchClient,
    pub(crate) csrf: Option<oauth2::CsrfToken>,
    pub(crate) force_verify: bool,
}

impl ImplicitUserTokenBuilder {
    /// Create a [`ImplicitUserTokenBuilder`]
    ///
    /// # Notes
    ///
    /// The `url` crate converts empty paths into "/" (such as `https://example.com` into `https://example.com/`),
    /// which means that you'll need to add "https://example.com/" to your redirect URIs (note the "trailing" slash) if you want to use an empty path.
    ///
    /// To avoid this, use a path such as `https://example.com/twitch/register` or similar instead, where the `url` crate would not add a trailing `/`.
    pub fn new(
        client_id: ClientId,
        redirect_url: RedirectUrl,
    ) -> Result<ImplicitUserTokenBuilder, oauth2::url::ParseError> {
        Ok(ImplicitUserTokenBuilder {
            scopes: vec![],
            client: crate::TwitchClient::new(
                client_id,
                None,
                oauth2::AuthUrl::new(crate::AUTH_URL.to_string())?,
                Some(oauth2::TokenUrl::new(crate::TOKEN_URL.to_string())?),
            )
            .set_auth_type(oauth2::AuthType::BasicAuth)
            .set_redirect_uri(redirect_url),
            csrf: None,
            force_verify: false,
        })
    }

    /// Add scopes to the request
    pub fn set_scopes(mut self, scopes: Vec<Scope>) -> Self {
        self.scopes = scopes;
        self
    }

    /// Add a single scope to request
    pub fn add_scope(&mut self, scope: Scope) { self.scopes.push(scope); }

    /// Enable or disable function to make the user able to switch accounts if needed.
    pub fn force_verify(mut self, b: bool) -> Self {
        self.force_verify = b;
        self
    }

    /// Generate the URL to request a token.
    ///
    /// Step 1. in the [guide](https://dev.twitch.tv/docs/authentication/getting-tokens-oauth/#auth-implicit-code-flow)
    pub fn generate_url(&mut self) -> (oauth2::url::Url, oauth2::CsrfToken) {
        let mut auth = self
            .client
            .authorize_url(oauth2::CsrfToken::new_random)
            .use_implicit_flow();

        for scope in self.scopes.iter() {
            auth = auth.add_scope(scope.as_oauth_scope())
        }

        auth = auth.add_extra_param(
            "force_verify",
            if self.force_verify { "true" } else { "false" },
        );

        let (url, csrf) = auth.url();
        self.csrf = Some(csrf.clone());
        (url, csrf)
    }

    /// Generate the code with the help of the hash.
    ///
    /// You can skip this method and instead use the token in the hash directly with [`UserToken::from_existing()`], but it's provided here for convenience.
    ///
    /// Step. 3 and 4 in the [guide](https://dev.twitch.tv/docs/authentication/getting-tokens-oauth/#oauth-implicit-code-flow)
    ///
    /// # Example
    ///
    /// When the user authenticates, they are sent to `<redirecturl>#access_token=<access_token>&scope=<scopes, space (%20) separated>&state=<csrf state>&token_type=bearer`
    ///
    /// On failure, they are sent to
    ///
    /// `<redirect_url or first defined url in dev console>?error=<error type>&error_description=<error description>&state=<csrf state>`
    /// Get the hash of the url with javascript.
    ///
    /// ```js
    /// document.location.hash.substr(1);
    /// ```
    ///
    /// and send it to your client in what ever way convenient.
    ///
    /// Provided below is an example of how to do it, no guarantees on the safety of this method.
    ///
    /// ```html
    /// <!DOCTYPE html>
    /// <html>
    /// <head>
    /// <title>Authorization</title>
    /// <meta name="ROBOTS" content="NOFOLLOW">
    /// <meta http-equiv="Content-Type" content="text/html; charset=UTF-8">
    /// <script type="text/javascript">
    /// <!--
    /// function initiate() {
    ///     var hash = document.location.hash.substr(1);
    ///     document.getElementById("javascript").className = "";
    ///     if (hash != null) {
    ///             document.location.replace("/token?"+hash);
    ///     }
    ///     else {
    ///         document.getElementById("javascript").innerHTML = "Error: Access Token not found";
    ///     }
    /// }
    /// -->
    /// </script>
    /// <style type="text/css">
    ///     body { text-align: center; background-color: #FFF; max-width: 500px; margin: auto; }
    ///     noscript { color: red;  }
    ///     .hide { display: none; }
    /// </style>
    /// </head>
    /// <body onload="initiate()">
    /// <h1>Authorization</h1>
    /// <noscript>
    ///     <p>This page requires <strong>JavaScript</strong> to get your token.
    /// </noscript>
    /// <p id="javascript" class="hide">
    /// You should be redirected..
    /// </p>
    /// </body>
    /// </html>
    /// ```
    ///
    /// where `/token?` gives this function it's corresponding arguments in query params
    ///
    /// Make sure that `/token` removes the query from the history.
    ///
    /// ```html
    /// <!DOCTYPE html>
    /// <html>
    /// <head>
    /// <title>Authorization Successful</title>
    /// <meta name="ROBOTS" content="NOFOLLOW">
    /// <meta http-equiv="Content-Type" content="text/html; charset=UTF-8">
    /// <script type="text/javascript">
    /// <!--
    /// function initiate() {
    ///     //
    ///     document.location.replace("/token_retrieved);
    /// }
    /// -->
    /// </script>
    /// <style type="text/css">
    ///     body { text-align: center; background-color: #FFF; max-width: 500px; margin: auto; }
    /// </style>
    /// </head>
    /// <body onload="initiate()">
    /// <h1>Authorization Successful</h1>
    /// </body>
    /// </html>
    /// ```
    ///
    ///
    pub async fn get_user_token<RE, C, F>(
        self,
        http_client: C,
        state: Option<&str>,
        access_token: Option<&str>,
        error: Option<&str>,
        error_description: Option<&str>,
    ) -> Result<UserToken, ImplicitUserTokenExchangeError<RE>>
    where
        RE: std::error::Error + Send + Sync + 'static,
        C: Copy + FnOnce(HttpRequest) -> F,
        F: Future<Output = Result<HttpResponse, RE>>,
    {
        if let Some(csrf) = self.csrf {
            if csrf.secret() != state.unwrap_or("") {
                return Err(ImplicitUserTokenExchangeError::StateMismatch);
            }
        } else {
            return Err(ImplicitUserTokenExchangeError::StateMismatch);
        }
        match (access_token, error, error_description) {
            (Some(access_token), None, None) => UserToken::from_existing(
                http_client,
                crate::oauth2::AccessToken::new(access_token.to_string()),
                None,
                None,
            )
            .await
            .map_err(Into::into),
            (_, error, description) => {
                let (error, description) = (
                    error.map(|s| s.to_string()),
                    description.map(|s| s.to_string()),
                );
                Err(ImplicitUserTokenExchangeError::TwitchError { error, description })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    pub use super::*;
    #[test]
    fn generate_url() {
        dbg!(UserTokenBuilder::new(
            ClientId::new("clientid".to_string()),
            ClientSecret::new("secret".to_string()),
            oauth2::RedirectUrl::new("https://localhost".to_string()).unwrap(),
        )
        .unwrap()
        .force_verify(true)
        .generate_url()
        .0
        .to_string());
    }

    #[tokio::test]
    #[ignore]
    async fn get_token() {
        let mut t = UserTokenBuilder::new(
            ClientId::new("clientid".to_string()),
            ClientSecret::new("secret".to_string()),
            crate::RedirectUrl::new(r#"https://localhost"#.to_string()).unwrap(),
        )
        .unwrap()
        .force_verify(true);
        t.csrf = Some(oauth2::CsrfToken::new("random".to_string()));
        let token = t
            .get_user_token(crate::client::surf_http_client, "random", "authcode")
            .await
            .unwrap();
        println!("token: {:?} - {}", token, token.access_token.secret());
    }

    #[tokio::test]
    #[ignore]
    async fn get_implicit_token() {
        let mut t = ImplicitUserTokenBuilder::new(
            ClientId::new("clientid".to_string()),
            crate::RedirectUrl::new(r#"http://localhost/twitch/register"#.to_string()).unwrap(),
        )
        .unwrap()
        .force_verify(true);
        println!("{}", t.generate_url().0);
        t.csrf = Some(oauth2::CsrfToken::new("random".to_string()));
        let token = t
            .get_user_token(
                crate::client::surf_http_client,
                Some("random"),
                Some("authcode"),
                None,
                None,
            )
            .await
            .unwrap();
        println!("token: {:?} - {}", token, token.access_token.secret());
    }
}
