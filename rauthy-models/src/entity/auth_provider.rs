use crate::app_state::AppState;
use crate::entity::clients::Client;
use crate::entity::users::User;
use crate::entity::users_values::UserValues;
use crate::entity::well_known::WellKnown;
use crate::language::Language;
use crate::request::{
    ProviderCallbackRequest, ProviderLoginRequest, ProviderRequest, UserValuesRequest,
};
use crate::response::ProviderLookupResponse;
use crate::{AddressClaim, JktClaim, JwtTokenType};
use actix_web::cookie::Cookie;
use actix_web::http::header::HeaderValue;
use actix_web::{cookie, web};
use cryptr::utils::secure_random_alnum;
use cryptr::EncValue;
use rauthy_common::constants::{
    CACHE_NAME_12HR, CACHE_NAME_AUTH_PROVIDER_CALLBACK, COOKIE_UPSTREAM_CALLBACK,
    IDX_AUTH_PROVIDER, IDX_AUTH_PROVIDER_TEMPLATE, PROVIDER_CALLBACK_URI, RAUTHY_VERSION,
    UPSTREAM_AUTH_CALLBACK_TIMEOUT_SECS,
};
use rauthy_common::error_response::{ErrorResponse, ErrorResponseType};
use rauthy_common::utils::{base64_decode, base64_encode, base64_url_encode, new_store_id};
use redhac::{cache_del, cache_get, cache_get_from, cache_get_value, cache_insert, AckLevel};
use reqwest::tls;
use ring::digest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json_path::{ExactlyOneError, JsonPath};
use sqlx::{query, query_as};
use std::collections::HashMap;
use std::fmt::Write;
use std::net::ToSocketAddrs;
use std::str::FromStr;
use std::time::Duration;
use time::OffsetDateTime;
use tracing::{debug, error, warn};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthProviderType {
    OIDC,
    // this can be extended in the future tu support special providers that do not
    // work with the oidc standards
}

impl AuthProviderType {
    pub fn as_str(&self) -> &str {
        match self {
            Self::OIDC => "oidc",
        }
    }
}

impl TryFrom<&str> for AuthProviderType {
    type Error = ErrorResponse;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let slf = match value {
            "oidc" => Self::OIDC,
            _ => {
                return Err(ErrorResponse::new(
                    ErrorResponseType::BadRequest,
                    "Invalid AuthProviderType".to_string(),
                ))
            }
        };
        Ok(slf)
    }
}

impl From<String> for AuthProviderType {
    /// Defaults to Self::OIDC in case of an error
    fn from(value: String) -> Self {
        Self::try_from(value.as_str()).unwrap_or(Self::OIDC)
    }
}

/// Upstream Auth Provider for upstream logins without a local Rauthy account
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthProvider {
    pub id: String,
    pub name: String,
    pub typ: AuthProviderType,

    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub userinfo_endpoint: String,

    pub client_id: String,
    pub secret: Option<Vec<u8>>,
    pub scope: String,

    pub allow_insecure_requests: bool,
    pub use_pkce: bool,

    pub root_pem: Option<String>,
    pub logo: Option<Vec<u8>>,
    pub logo_type: Option<String>,
}

impl AuthProvider {
    pub async fn create(
        data: &web::Data<AppState>,
        payload: ProviderRequest,
    ) -> Result<Self, ErrorResponse> {
        let id = new_store_id();
        // let scope = Self::cleanup_scope(&payload.scope);
        // let secret = Self::secret_encrypted(&payload.client_secret)?;
        //
        // let slf = Self {
        //     id,
        //     name: payload.name,
        //     issuer: payload.issuer,
        //     authorization_endpoint: payload.authorization_endpoint,
        //     token_endpoint: payload.token_endpoint,
        //     userinfo_endpoint: payload.userinfo_endpoint,
        //     client_id: payload.client_id,
        //     secret,
        //     scope,
        //     token_auth_method_basic: payload.token_auth_method_basic,
        //     use_pkce: payload.use_pkce,
        //     root_pem: payload.root_pem,
        //
        //     // TODO when implemented
        //     logo: None,
        //     logo_type: None,
        // };
        let slf = Self::try_from_id_req(id, payload)?;

        query!(
            r#"
            INSERT INTO
            auth_providers (id, name, typ, issuer, authorization_endpoint, token_endpoint,
            userinfo_endpoint, client_id, secret, scope, allow_insecure_requests,
            use_pkce, root_pem, logo, logo_type)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)"#,
            slf.id,
            slf.name,
            slf.typ.as_str(),
            slf.issuer,
            slf.authorization_endpoint,
            slf.token_endpoint,
            slf.userinfo_endpoint,
            slf.client_id,
            slf.secret,
            slf.scope,
            slf.allow_insecure_requests,
            slf.use_pkce,
            slf.root_pem,
            slf.logo,
            slf.logo_type,
        )
        .execute(&data.db)
        .await?;

        cache_insert(
            CACHE_NAME_12HR.to_string(),
            Self::cache_idx(&slf.id),
            &data.caches.ha_cache_config,
            &slf,
            AckLevel::Quorum,
        )
        .await?;

        Self::invalidate_cache_all(data).await?;

        Ok(slf)
    }

    pub async fn find(data: &web::Data<AppState>, id: &str) -> Result<Self, ErrorResponse> {
        if let Some(res) = cache_get!(
            Self,
            CACHE_NAME_12HR.to_string(),
            Self::cache_idx(&id),
            &data.caches.ha_cache_config,
            false
        )
        .await?
        {
            return Ok(res);
        }

        let slf = query_as!(Self, "SELECT * FROM auth_providers WHERE id = $1", id)
            .fetch_one(&data.db)
            .await?;

        cache_insert(
            CACHE_NAME_12HR.to_string(),
            Self::cache_idx(id),
            &data.caches.ha_cache_config,
            &slf,
            AckLevel::Quorum,
        )
        .await?;

        Ok(slf)
    }

    pub async fn find_all(data: &web::Data<AppState>) -> Result<Vec<Self>, ErrorResponse> {
        if let Some(res) = cache_get!(
            Vec<Self>,
            CACHE_NAME_12HR.to_string(),
            Self::cache_idx("all"),
            &data.caches.ha_cache_config,
            false
        )
        .await?
        {
            return Ok(res);
        }

        let res = query_as!(Self, "SELECT * FROM auth_providers")
            .fetch_all(&data.db)
            .await?;

        // needed for rendering each single login page -> always cache this
        cache_insert(
            CACHE_NAME_12HR.to_string(),
            Self::cache_idx("all"),
            &data.caches.ha_cache_config,
            &res,
            AckLevel::Quorum,
        )
        .await?;

        Ok(res)
    }

    pub async fn delete(data: &web::Data<AppState>, id: &str) -> Result<(), ErrorResponse> {
        query!("DELETE FROM auth_providers WHERE id = $1", id)
            .execute(&data.db)
            .await?;

        Self::invalidate_cache_all(data).await?;
        cache_del(
            CACHE_NAME_12HR.to_string(),
            Self::cache_idx(id),
            &data.caches.ha_cache_config,
        )
        .await?;

        Ok(())
    }

    pub async fn update(
        data: &web::Data<AppState>,
        id: String,
        payload: ProviderRequest,
    ) -> Result<(), ErrorResponse> {
        Self::try_from_id_req(id, payload)?.save(data).await
    }

    pub async fn save(&self, data: &web::Data<AppState>) -> Result<(), ErrorResponse> {
        // TODO when implemented: logo = $12, logo_type = $13
        query!(
            r#"UPDATE auth_providers
            SET name = $1, issuer = $2, typ = $3, authorization_endpoint = $4, token_endpoint = $5,
            userinfo_endpoint = $6, client_id = $7, secret = $8, scope = $9,
            allow_insecure_requests = $10, use_pkce = $11, root_pem = $12
            WHERE id = $13"#,
            self.name,
            self.issuer,
            self.typ.as_str(),
            self.authorization_endpoint,
            self.token_endpoint,
            self.userinfo_endpoint,
            self.client_id,
            self.secret,
            self.scope,
            self.allow_insecure_requests,
            self.use_pkce,
            self.root_pem,
            // self.logo,
            // self.logo_type,
            self.id,
        )
        .execute(&data.db)
        .await?;

        Self::invalidate_cache_all(data).await?;

        cache_insert(
            CACHE_NAME_12HR.to_string(),
            Self::cache_idx(&self.id),
            &data.caches.ha_cache_config,
            &self,
            AckLevel::Quorum,
        )
        .await?;

        Ok(())
    }
}

impl AuthProvider {
    fn cache_idx(id: &str) -> String {
        format!("{}_{}", IDX_AUTH_PROVIDER, id)
    }

    fn cleanup_scope(scope: &str) -> String {
        scope
            .split(' ')
            // filter this way to catch multiple spaces in a row
            .filter_map(|s| if !s.is_empty() { Some(s.trim()) } else { None })
            .collect::<Vec<&str>>()
            .join("+")
    }

    fn build_client(
        danger_allow_insecure: bool,
        root_pem: Option<&str>,
    ) -> Result<reqwest::Client, ErrorResponse> {
        let client = if danger_allow_insecure {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .connect_timeout(Duration::from_secs(10))
                .user_agent(format!("Rauthy Auth Provider Client v{}", RAUTHY_VERSION))
                .https_only(false)
                .danger_accept_invalid_certs(true)
                .build()?
        } else {
            let mut builder = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .connect_timeout(Duration::from_secs(10))
                .min_tls_version(tls::Version::TLS_1_3)
                .tls_built_in_root_certs(true)
                .user_agent(format!("Rauthy Auth Provider Client v{}", RAUTHY_VERSION))
                .https_only(true);
            if let Some(pem) = root_pem {
                let root_cert = reqwest::tls::Certificate::from_pem(pem.as_bytes())?;
                builder = builder.add_root_certificate(root_cert);
            }
            builder.build()?
        };

        Ok(client)
    }

    fn try_from_id_req(id: String, req: ProviderRequest) -> Result<Self, ErrorResponse> {
        let scope = Self::cleanup_scope(&req.scope);
        let secret = Self::secret_encrypted(&req.client_secret)?;

        Ok(Self {
            id,
            name: req.name,
            // for now, this will always be OIDC
            // preparation for future special providers
            typ: AuthProviderType::OIDC,
            issuer: req.issuer,
            authorization_endpoint: req.authorization_endpoint,
            token_endpoint: req.token_endpoint,
            userinfo_endpoint: req.userinfo_endpoint,
            client_id: req.client_id,
            secret,
            scope,
            allow_insecure_requests: req.danger_allow_insecure.unwrap_or(false),
            use_pkce: req.use_pkce,
            root_pem: req.root_pem,

            logo: None,
            logo_type: None,
            // TODO when implemented in the UI
            // logo: payload.,
            // logo_type: payload.use_pkce,
        })
    }

    async fn invalidate_cache_all(data: &web::Data<AppState>) -> Result<(), ErrorResponse> {
        cache_del(
            CACHE_NAME_12HR.to_string(),
            Self::cache_idx("all"),
            &data.caches.ha_cache_config,
        )
        .await?;
        AuthProviderTemplate::invalidate_cache(data).await?;
        Ok(())
    }

    pub async fn lookup_config(
        issuer: &str,
        danger_allow_insecure: bool,
        root_pem: Option<&str>,
    ) -> Result<ProviderLookupResponse, ErrorResponse> {
        let client = Self::build_client(danger_allow_insecure, root_pem)?;

        let issuer_url = if issuer.starts_with("http://") || issuer.starts_with("https://") {
            issuer.to_string()
        } else {
            // we always assume https connections, if the scheme is not given
            format!("https://{}", issuer)
        };
        // for now, we only support the default openid-configuration location
        let config_url = if issuer_url.ends_with('/') {
            format!("{}.well-known/openid-configuration", issuer_url)
        } else {
            format!("{}/.well-known/openid-configuration", issuer_url)
        };

        debug!("AuthProvider lookup to {}", config_url);
        let res = client.get(&config_url).send().await?;
        let status = res.status();
        if !status.is_success() {
            let body = res.text().await?;
            return Err(ErrorResponse::new(
                ErrorResponseType::Connection,
                format!(
                    "HTTP {} when trying provider config lookup to {}: {}",
                    config_url, status, body
                ),
            ));
        }

        let well_known = res.json::<WellKnown>().await.map_err(|err| {
            ErrorResponse::new(
                // TODO we could make this more UX friendly in the future and return a link to the
                // docs, when they exist
                ErrorResponseType::BadRequest,
                format!(
                    "The provider does not support the mandatory openid-configuration: {}",
                    err
                ),
            )
        })?;

        let scopes_supported = well_known
            .scopes_supported
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<&str>>();
        let mut scope = String::with_capacity(24);
        if scopes_supported.contains(&"openid") {
            scope.push_str("openid ");
        }
        if scopes_supported.contains(&"profile") {
            scope.push_str("profile ");
        }
        if scopes_supported.contains(&"email") {
            scope.push_str("email ");
        }

        Ok(ProviderLookupResponse {
            issuer: well_known.issuer,
            // TODO optimization (and possibly security enhancement): strip issuer url from all of these?
            // what does the RFC mention about it? MUST they always be on the same sub path?
            authorization_endpoint: well_known.authorization_endpoint,
            token_endpoint: well_known.token_endpoint,
            userinfo_endpoint: well_known.userinfo_endpoint,
            token_auth_method_basic: !well_known
                .token_endpoint_auth_methods_supported
                .iter()
                .any(|m| m == "client_secret_post"),
            use_pkce: well_known
                .code_challenge_methods_supported
                .iter()
                .any(|c| c == "S256"),
            danger_allow_insecure,
            scope,
            // TODO add `scopes_supported` Vec and make them selectable with checkboxes in the UI
            // instead of typing them in?
        })
    }

    fn secret_encrypted(secret: &Option<String>) -> Result<Option<Vec<u8>>, ErrorResponse> {
        if let Some(secret) = &secret {
            Ok(Some(
                EncValue::encrypt(secret.as_bytes())?.into_bytes().to_vec(),
            ))
        } else {
            Ok(None)
        }
    }

    pub fn get_secret_cleartext(secret: &Option<Vec<u8>>) -> Result<Option<String>, ErrorResponse> {
        if let Some(secret) = &secret {
            let bytes = EncValue::try_from(secret.clone())?.decrypt()?;
            let cleartext = String::from_utf8_lossy(bytes.as_ref()).to_string();
            Ok(Some(cleartext))
        } else {
            Ok(None)
        }
    }
}

/// Will be created to start a new upstream login and afterward validate a callback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthProviderCallback {
    pub callback_id: String,
    pub xsrf_token: String,
    pub typ: AuthProviderType,

    pub client_id: String,
    pub client_scope: Vec<String>,
    pub client_force_mfa: bool,
    pub client_redirect_uri: String,
    pub client_state: Option<String>,
    pub client_nonce: Option<String>,
    pub client_code_challenge: Option<String>,
    pub client_code_challenge_method: Option<String>,

    pub provider_id: String,
    pub provider_issuer: String,
    pub provider_token_endpoint: String,
    // pub userinfo_endpoint: String,
    pub provider_client_id: String,
    pub provider_secret: Option<Vec<u8>>,
    pub allow_insecure_requests: bool,
    pub use_pkce: bool,
    pub root_pem: Option<String>,
    // TODO add a nonce upstream as well? -> improvement?
    pub pkce_challenge: String,
}

// CRUD
impl AuthProviderCallback {
    pub async fn delete(
        data: &web::Data<AppState>,
        callback_id: String,
    ) -> Result<(), ErrorResponse> {
        cache_del(
            CACHE_NAME_AUTH_PROVIDER_CALLBACK.to_string(),
            callback_id,
            &data.caches.ha_cache_config,
        )
        .await?;
        Ok(())
    }

    async fn find(data: &web::Data<AppState>, callback_id: String) -> Result<Self, ErrorResponse> {
        match cache_get!(
            Self,
            CACHE_NAME_AUTH_PROVIDER_CALLBACK.to_string(),
            callback_id,
            &data.caches.ha_cache_config,
            true
        )
        .await?
        {
            None => Err(ErrorResponse::new(
                ErrorResponseType::NotFound,
                "Callback Code not found - timeout reached?".to_string(),
            )),
            Some(slf) => Ok(slf),
        }
    }

    async fn save(&self, data: &web::Data<AppState>) -> Result<(), ErrorResponse> {
        cache_insert(
            CACHE_NAME_AUTH_PROVIDER_CALLBACK.to_string(),
            self.callback_id.clone(),
            &data.caches.ha_cache_config,
            &self,
            // Once is good enough and faster -> random ID each time cannot produce conflicts
            AckLevel::Once,
        )
        .await?;
        Ok(())
    }
}

impl AuthProviderCallback {
    /// returns (encrypted cookie, xsrf token, location header, optional allowed origins)
    pub async fn login_start(
        data: &web::Data<AppState>,
        payload: ProviderLoginRequest,
    ) -> Result<(Cookie, String, HeaderValue, Option<Vec<String>>), ErrorResponse> {
        let provider = AuthProvider::find(data, &payload.provider_id).await?;

        let client = Client::find(data, payload.client_id).await?;
        let client_scope = client.sanitize_login_scopes(&payload.scopes)?;
        let allowed_origins = client.get_allowed_origins();

        let slf = Self {
            callback_id: secure_random_alnum(32),
            xsrf_token: secure_random_alnum(32),
            typ: provider.typ,

            client_id: client.id,
            client_scope,
            client_force_mfa: client.force_mfa,
            client_redirect_uri: payload.redirect_uri,
            client_state: payload.state,
            client_nonce: payload.nonce,
            client_code_challenge: payload.code_challenge,
            client_code_challenge_method: payload.code_challenge_method,

            provider_id: provider.id,
            provider_issuer: provider.issuer,
            provider_token_endpoint: provider.token_endpoint,
            provider_client_id: provider.client_id,
            provider_secret: provider.secret,
            allow_insecure_requests: provider.allow_insecure_requests,
            use_pkce: provider.use_pkce,
            root_pem: provider.root_pem,

            pkce_challenge: payload.pkce_challenge,
        };

        let mut location = format!(
            "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}",
            provider.authorization_endpoint,
            slf.provider_client_id,
            *PROVIDER_CALLBACK_URI,
            provider.scope,
            slf.callback_id
        );
        if provider.use_pkce {
            write!(
                location,
                "&code_challenge={}&code_challenge_method=S256",
                slf.pkce_challenge
            )
            .expect("write to always succeed");
        }

        let id_enc = EncValue::encrypt(slf.callback_id.as_bytes())?;
        let id_b64 = base64_encode(id_enc.into_bytes().as_ref());
        let cookie = cookie::Cookie::build(COOKIE_UPSTREAM_CALLBACK, id_b64)
            .secure(true)
            .http_only(true)
            .same_site(cookie::SameSite::Lax)
            .max_age(cookie::time::Duration::seconds(
                UPSTREAM_AUTH_CALLBACK_TIMEOUT_SECS as i64,
            ))
            .path("/auth")
            .finish();

        slf.save(data).await?;

        Ok((
            cookie,
            slf.xsrf_token,
            HeaderValue::from_str(&location).expect("Location HeaderValue to be correct"),
            allowed_origins,
        ))
    }

    /// In case of any error, the callback code will be fully deleted for security reasons.
    pub async fn login_finish(
        data: &web::Data<AppState>,
        cookie: Cookie<'_>,
        payload: &ProviderCallbackRequest,
    ) -> Result<Self, ErrorResponse> {
        // the callback id for the cache should be inside the encrypted cookie
        let bytes = base64_decode(cookie.value())?;
        let plain = EncValue::try_from(bytes)?.decrypt()?;
        let callback_id = String::from_utf8_lossy(plain.as_ref()).to_string();

        // validate state
        if callback_id != payload.state {
            Self::delete(data, callback_id).await?;

            error!("`state` does not match");
            return Err(ErrorResponse::new(
                ErrorResponseType::BadRequest,
                "`state` does not match".to_string(),
            ));
        }

        // validate csrf token
        let slf = Self::find(data, callback_id).await?;
        if slf.xsrf_token != payload.xsrf_token {
            Self::delete(data, slf.callback_id).await?;

            error!("invalid CSRF token");
            return Err(ErrorResponse::new(
                ErrorResponseType::Unauthorized,
                "invalid CSRF token".to_string(),
            ));
        }

        // validate PKCE verifier
        let hash = digest::digest(&digest::SHA256, payload.pkce_verifier.as_bytes());
        let hash_base64 = base64_url_encode(hash.as_ref());
        if slf.pkce_challenge != hash_base64 {
            Self::delete(data, slf.callback_id).await?;

            error!("invalid PKCE verifier");
            return Err(ErrorResponse::new(
                ErrorResponseType::Unauthorized,
                "invalid PKCE verifier".to_string(),
            ));
        }

        // request is valid -> fetch token for the user
        let client =
            AuthProvider::build_client(slf.allow_insecure_requests, slf.root_pem.as_deref())?;
        let res = client
            .post(&slf.provider_token_endpoint)
            .basic_auth(
                &slf.provider_client_id,
                AuthProvider::get_secret_cleartext(&slf.provider_secret)?,
            )
            .form(&OidcCodeRequestParams {
                client_id: &slf.provider_client_id,
                client_secret: AuthProvider::get_secret_cleartext(&slf.provider_secret)?,
                code: &payload.code,
                code_verifier: slf.use_pkce.then_some(&payload.pkce_verifier),
                grant_type: "authorization_code",
                redirect_uri: &*PROVIDER_CALLBACK_URI,
            })
            .send()
            .await?;

        if !res.status().is_success() {
            let status = res.status().as_u16();
            let err = match res.text().await {
                Ok(body) => format!(
                    "HTTP {} during POST {} for upstream auth provider '{}'\n{}",
                    status, slf.provider_token_endpoint, slf.provider_client_id, body
                ),
                Err(_) => format!(
                    "HTTP {} during POST {} for upstream auth provider '{}' without any body",
                    status, slf.provider_token_endpoint, slf.provider_client_id
                ),
            };
            error!("{}", err);
            return Err(ErrorResponse::new(ErrorResponseType::Internal, err));
        }

        let user = match res.json::<AuthProviderTokenSet>().await {
            Ok(ts) => {
                // in case of a standard OIDC provider, we only care about the ID token
                if ts.id_token.is_none() {
                    let err = format!(
                        "Did not receive an ID token from {} when one was expected",
                        slf.provider_issuer,
                    );
                    error!("{}", err);
                    return Err(ErrorResponse::new(ErrorResponseType::Internal, err));
                }
                AuthProviderIdClaims::validate_update_user(
                    data,
                    ts.id_token.as_deref().unwrap(),
                    &slf.provider_id,
                )
                .await?
            }
            Err(err) => {
                let err = format!(
                    "Deserializing /token response from auth provider {}: {}",
                    slf.provider_client_id, err
                );
                error!("{}", err);
                return Err(ErrorResponse::new(ErrorResponseType::Internal, err));
            }
        };

        // if we get here, everything was valid and the user has possibly been
        // updated on our side as well

        // update + upgrade the session

        if slf.client_force_mfa {
            // If the client needs forced mfa and our user has no passkey, return
            // a proper message, that this needs to be added in the account overview.
            //
            // If the user has a passkey, request it here
            todo!();
        }

        // create delete cookie
        // let cookie = cookie::Cookie::build(COOKIE_UPSTREAM_CALLBACK, "")
        //     .secure(true)
        //     .http_only(true)
        //     .same_site(cookie::SameSite::Lax)
        //     .max_age(cookie::time::Duration::ZERO)
        //     .path("/auth")
        //     .finish();

        todo!()
    }
}

/// Auth Provider as template value for SSR of the Login page
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthProviderTemplate {
    pub id: String,
    pub name: String,
    pub use_pkce: bool,
    // pub logo: Option<Vec<u8>>,
    // pub logo_type: Option<String>,
}

impl AuthProviderTemplate {
    async fn invalidate_cache(data: &web::Data<AppState>) -> Result<(), ErrorResponse> {
        cache_del(
            CACHE_NAME_12HR.to_string(),
            IDX_AUTH_PROVIDER_TEMPLATE.to_string(),
            &data.caches.ha_cache_config,
        )
        .await?;
        Ok(())
    }

    pub async fn get_all_json_template(
        data: &web::Data<AppState>,
    ) -> Result<Option<String>, ErrorResponse> {
        if let Some(res) = cache_get!(
            Option<String>,
            CACHE_NAME_12HR.to_string(),
            IDX_AUTH_PROVIDER_TEMPLATE.to_string(),
            &data.caches.ha_cache_config,
            false
        )
        .await?
        {
            return Ok(res);
        }

        let providers = AuthProvider::find_all(data)
            .await?
            .into_iter()
            .map(|p| Self {
                id: p.id,
                name: p.name,
                use_pkce: p.use_pkce,
            })
            .collect::<Vec<Self>>();

        let json = if providers.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&providers)?)
        };
        cache_insert(
            CACHE_NAME_12HR.to_string(),
            IDX_AUTH_PROVIDER_TEMPLATE.to_string(),
            &data.caches.ha_cache_config,
            &json,
            AckLevel::Quorum,
        )
        .await?;

        Ok(json)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AuthProviderAddressClaims<'a> {
    pub formatted: Option<&'a str>,
    pub street_address: Option<&'a str>,
    pub locality: Option<&'a str>,
    pub postal_code: Option<i32>,
    pub country: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
pub struct AuthProviderIdClaims<'a> {
    pub iss: &'a str,
    pub sub: &'a str,
    pub aud: Option<&'a str>,
    pub azp: Option<&'a str>,
    pub amr: Option<Vec<&'a str>>,
    // even though `email` is mandatory, we set it to optional for the deserialization
    // to have more control over the error message being returned
    pub email: Option<&'a str>,
    pub email_verified: Option<bool>,
    pub given_name: Option<&'a str>,
    pub family_name: Option<&'a str>,
    pub address: Option<AuthProviderAddressClaims<'a>>,
    pub birthdate: Option<&'a str>,
    pub locale: Option<&'a str>,
    pub phone: Option<&'a str>,
}

impl AuthProviderIdClaims<'_> {
    pub async fn validate_update_user(
        data: &web::Data<AppState>,
        token: &str,
        auth_provider_id: &str,
    ) -> Result<User, ErrorResponse> {
        let mut parts = token.split(".");
        let _header = parts.next().ok_or_else(|| {
            ErrorResponse::new(
                ErrorResponseType::BadRequest,
                "incorrect ID did not contain claims".to_string(),
            )
        })?;
        let claims = parts.next().ok_or_else(|| {
            ErrorResponse::new(
                ErrorResponseType::BadRequest,
                "ID token was unsigned".to_string(),
            )
        })?;
        let bytes = base64_decode(claims)?;
        let slf = serde_json::from_slice::<AuthProviderIdClaims>(&bytes)?;

        if slf.email.is_none() {
            let err = "No `email` in ID token claims. This is a mandatory claim";
            error!("{}", err);
            return Err(ErrorResponse::new(
                ErrorResponseType::BadRequest,
                err.to_string(),
            ));
        }

        // We need to search for the user on our side in 2 ways:
        // 1. by email to make sure there are no conflicts
        // 2. by provider uid in case the email has been changed remotely, if the first
        // lookup produced any error
        let user_opt = match User::find_by_email(data, slf.email.unwrap().to_string()).await {
            Ok(user) => Some(user),
            Err(_) => {
                // if we were unable to find the user, we need to make another lookup
                // and search by federation uid on the remote system to not end up with
                // duplicate users in case of a remote email update
                User::find_by_federation_uid(data, &slf.sub).await?
            }
        };

        let now = OffsetDateTime::now_utc().unix_timestamp();
        let user = if let Some(mut user) = user_opt {
            let mut old_email = None;
            let mut forbidden_error = None;

            // validate federation_uid
            // we must reject any upstream login, if a non-federated local user with the same email
            // exists, as it could lead to an account takeover
            if user.federation_uid.is_none() || user.federation_uid.as_deref() != Some(slf.sub) {
                forbidden_error = Some("non-federated user or ID mismatch");
            }

            // validate auth_provider_id
            if user.auth_provider_id.as_deref() != Some(auth_provider_id) {
                forbidden_error = Some("invalid login from wrong auth provider");
            }

            if let Some(err) = forbidden_error {
                user.last_failed_login = Some(now);
                user.failed_login_attempts =
                    Some(user.failed_login_attempts.unwrap_or_default() + 1);
                user.save(data, old_email, None).await?;

                return Err(ErrorResponse::new(
                    ErrorResponseType::Forbidden,
                    err.to_string(),
                ));
            }

            // check / update email
            if Some(user.email.as_str()) != slf.email {
                old_email = Some(user.email);
                user.email = slf.email.unwrap().to_string();
            }

            // check other existing values and possibly update them
            if let Some(given_name) = slf.given_name {
                if user.given_name.as_str() != given_name {
                    user.given_name = given_name.to_string();
                }
            }
            if let Some(family_name) = slf.family_name {
                if user.family_name.as_str() != family_name {
                    user.family_name = family_name.to_string();
                }
            }

            // TODO role check update depending on some upstream claim?

            // update the user on our side
            user.last_login = Some(now);
            user.last_failed_login = None;
            user.failed_login_attempts = None;

            user.save(data, old_email, None).await?;
            user
        } else {
            // Create a new federated user
            let new_user = User {
                email: slf.email.unwrap().to_string(),
                given_name: slf.given_name.unwrap_or("N/A").to_string(),
                family_name: slf.family_name.unwrap_or("N/A").to_string(),
                // TODO `rauthy_admin` role mapping by config? json path lookup?
                // roles: "".to_string(),
                enabled: true,
                email_verified: slf.email_verified.unwrap_or(false),
                last_login: Some(now),
                language: slf
                    .locale
                    .map(Language::from)
                    .unwrap_or(Language::default()),
                auth_provider_id: Some(auth_provider_id.to_string()),
                federation_uid: Some(slf.sub.to_string()),
                ..Default::default()
            };
            User::create_federated(data, new_user).await?
        };

        // check if we got additional values from the token
        let mut found_values = false;
        let mut user_values = match UserValues::find(data, &user.id).await? {
            Some(values) => UserValuesRequest {
                birthdate: values.birthdate,
                phone: values.phone,
                street: values.street,
                zip: values.zip,
                city: values.city,
                country: values.country,
            },
            None => UserValuesRequest {
                birthdate: None,
                phone: None,
                street: None,
                zip: None,
                city: None,
                country: None,
            },
        };
        if let Some(bday) = slf.birthdate {
            user_values.birthdate = Some(bday.to_string());
            found_values = true;
        }
        if let Some(phone) = slf.phone {
            user_values.phone = Some(phone.to_string());
            found_values = true;
        }
        if let Some(addr) = slf.address {
            if let Some(street) = addr.street_address {
                user_values.street = Some(street.to_string());
            }
            if let Some(country) = addr.country {
                user_values.country = Some(country.to_string());
            }
            if let Some(zip) = addr.postal_code {
                user_values.zip = Some(zip);
            }
            found_values = true;
        }
        if found_values {
            UserValues::upsert(data, user.id.clone(), user_values).await?;
        }

        Ok(user)
    }
}

#[derive(Debug, Deserialize)]
pub struct AuthProviderTokenSet {
    pub access_token: String,
    pub token_type: Option<String>,
    pub id_token: Option<String>,
    pub expires_in: i32,
    pub refresh_token: Option<String>,
}

#[derive(Debug, Serialize)]
struct OidcCodeRequestParams<'a> {
    client_id: &'a str,
    client_secret: Option<String>,
    code: &'a str,
    code_verifier: Option<&'a str>,
    grant_type: &'static str,
    redirect_uri: &'a str,
}
