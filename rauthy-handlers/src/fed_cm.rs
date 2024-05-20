use crate::ReqPrincipal;
use actix_web::{get, post, web, HttpRequest, HttpResponse};
use rauthy_common::constants::{EXPERIMENTAL_FED_CM_ENABLE, HEADER_ALLOW_ALL_ORIGINS, HEADER_JSON};
use rauthy_common::error_response::{ErrorResponse, ErrorResponseType};
use rauthy_models::app_state::AppState;
use rauthy_models::entity::clients::Client;
use rauthy_models::entity::fed_cm::{
    FedCMAccount, FedCMAccounts, FedCMClientMetadata, FedCMIdPConfig, WebIdentity,
};
use rauthy_models::entity::users::User;
use rauthy_models::request::{FedCMAssertionRequest, FedCMClientMetadataRequest};
use rauthy_service::token_set::{AuthCodeFlow, DeviceCodeFlow, TokenNonce, TokenSet};

/// GET accounts linked to the users
///
/// https://fedidcg.github.io/FedCM/#idp-api
#[utoipa::path(
    get,
    path = "/fed_cm/accounts",
    tag = "fed_cm",
    responses(
        (status = 200, description = "Ok", body = FedCMAccounts),
        (status = 400, description = "BadRequest"),
    ),
)]
#[get("/fed_cm/accounts")]
pub async fn get_fed_cm_accounts(
    req: HttpRequest,
    data: web::Data<AppState>,
) -> Result<HttpResponse, ErrorResponse> {
    is_fed_cm_enabled()?;
    is_web_identity_fetch(&req)?;

    let user = User::find_for_fed_cm_validated(&data, &req).await?;

    let account = FedCMAccount::from(user);
    let accounts = FedCMAccounts {
        accounts: vec![account],
    };
    Ok(HttpResponse::Ok().json(accounts))
}

/// GET metadata for the FedCM client
///
/// https://fedidcg.github.io/FedCM/#idp-api
#[utoipa::path(
    get,
    path = "/fed_cm/client_meta",
    tag = "fed_cm",
    params(FedCMClientMetadataRequest),
    responses(
        (status = 200, description = "Ok"),
    ),
)]
#[get("/fed_cm/client_meta")]
pub async fn get_fed_cm_client_meta(
    data: web::Data<AppState>,
    req: HttpRequest,
    params: actix_web_validator::Query<FedCMClientMetadataRequest>,
) -> Result<HttpResponse, ErrorResponse> {
    is_fed_cm_enabled()?;
    is_web_identity_fetch(&req)?;

    let params = params.into_inner();

    let client = Client::find(&data, params.client_id).await?;
    if !client.enabled {
        return Err(ErrorResponse::new(
            ErrorResponseType::WWWAuthenticate("client-disabled".to_string()),
            "This client has been disabled".to_string(),
        ));
    }

    let origin_header = client
        .validate_origin(&req, &data.listen_scheme, &data.public_url)?
        .ok_or_else(|| {
            ErrorResponse::new(
                ErrorResponseType::WWWAuthenticate("origin-header-missing".to_string()),
                "The `Origin` header is missing".to_string(),
            )
        })?;

    let meta = FedCMClientMetadata::new();
    Ok(HttpResponse::Ok().insert_header(origin_header).json(meta))
}

/// The FedCM IdP configuration
///
/// https://fedidcg.github.io/FedCM/#idp-api
#[utoipa::path(
    get,
    path = "/fed_cm/config",
    tag = "fed_cm",
    responses(
        (status = 200, description = "Ok", body = FedCMIdPConfig),
        (status = 400, description = "BadRequest"),
    ),
)]
#[get("/fed_cm/config")]
pub async fn get_fed_cm_config(
    req: HttpRequest,
    data: web::Data<AppState>,
) -> Result<HttpResponse, ErrorResponse> {
    is_fed_cm_enabled()?;
    is_web_identity_fetch(&req)?;

    let config = FedCMIdPConfig::get(&data).await?;
    Ok(HttpResponse::Ok()
        .insert_header(HEADER_JSON)
        .insert_header(HEADER_ALLOW_ALL_ORIGINS)
        .json(config))
}

/// Disconnect an account
///
/// https://fedidcg.github.io/FedCM/#idp-api
#[utoipa::path(
    get,
    path = "/fed_cm/disconnect",
    tag = "fed_cm",
    responses(
        (status = 200, description = "Ok"),
    ),
)]
#[get("/fed_cm/disconnect")]
pub async fn post_fed_cm_disconnect(
    data: web::Data<AppState>,
) -> Result<HttpResponse, ErrorResponse> {
    is_fed_cm_enabled()?;

    todo!()
}

/// POST ID assertion
///
/// https://fedidcg.github.io/FedCM/#idp-api
#[utoipa::path(
    post,
    path = "/fed_cm/token",
    tag = "fed_cm",
    request_body(
        content = FedCMAssertionRequest,
        content_type = "application/x-www-form-urlencoded"
    ),
    responses(
        (status = 200, description = "Ok"),
    ),
)]
#[post("/fed_cm/token")]
pub async fn post_fed_cm_token(
    req: HttpRequest,
    data: web::Data<AppState>,
    payload: actix_web_validator::Form<FedCMAssertionRequest>,
    principal: Option<ReqPrincipal>,
) -> Result<HttpResponse, ErrorResponse> {
    is_fed_cm_enabled()?;
    is_web_identity_fetch(&req)?;

    // check the active session
    // TODO should we return the not logged in here or at /accounts? how to handle clock skew?
    let principal = principal.ok_or_else(|| {
        ErrorResponse::new(
            ErrorResponseType::WWWAuthenticate("not-logged-in".to_string()),
            "No active session found".to_string(),
        )
    })?;
    let session = principal.validate_session_auth().map_err(|err| {
        ErrorResponse::new(
            ErrorResponseType::WWWAuthenticate("not-logged-in".to_string()),
            err.message,
        )
    })?;

    let payload = payload.into_inner();

    // find and check the client
    let client = Client::find(&data, payload.client_id).await?;
    if !client.enabled {
        return Err(ErrorResponse::new(
            ErrorResponseType::WWWAuthenticate("client-disabled".to_string()),
            "This client has been disabled".to_string(),
        ));
    }

    // TODO what about confidential clients?
    // TODO impl a new `FedCM` flow for client's and reject if not true?

    let origin_header = client
        .validate_origin(&req, &data.listen_scheme, &data.public_url)?
        .ok_or_else(|| {
            ErrorResponse::new(
                ErrorResponseType::WWWAuthenticate("origin-header-missing".to_string()),
                "The `Origin` header is missing".to_string(),
            )
        })?;

    // find and check the user
    let user = User::find_for_fed_cm_validated(&data, &req).await?;
    if payload.account_id != user.id || session.user_id.as_deref() != Some(&user.id) {
        return Err(ErrorResponse::new(
            ErrorResponseType::WWWAuthenticate("invalid-user".to_string()),
            "The `account_id` does not match the `user_id` from the active session".to_string(),
        ));
    }

    // We are good - issue a TokenSet
    let nonce = if let Some(nonce) = payload.nonce {
        Some(TokenNonce(nonce))
    } else {
        None
    };
    let ts = TokenSet::from_user(
        &user,
        &data,
        &client,
        None,
        nonce,
        // TODO add something like `fedcm` to the scopes? Maybe depending on new allowed flow?
        None,
        AuthCodeFlow::No,
        DeviceCodeFlow::No,
    )
    .await?;

    Ok(HttpResponse::Ok().insert_header(origin_header).json(ts))
}

/// The `.well-known` endpoint for FedCM clients
///
/// https://fedidcg.github.io/FedCM/#idp-api
#[utoipa::path(
    get,
    path = "/.well-known/web-identity",
    tag = "fed_cm",
    responses(
        (status = 200, description = "Ok", body = WebIdentity),
    ),
)]
#[get("/.well-known/web-identity")]
pub async fn get_fed_cm_well_known(
    data: web::Data<AppState>,
    req: HttpRequest,
) -> Result<HttpResponse, ErrorResponse> {
    is_fed_cm_enabled()?;
    is_web_identity_fetch(&req)?;

    Ok(HttpResponse::Ok()
        .insert_header(HEADER_JSON)
        .insert_header(HEADER_ALLOW_ALL_ORIGINS)
        .json(WebIdentity::new(&data.issuer)))
}

#[inline(always)]
fn is_fed_cm_enabled() -> Result<(), ErrorResponse> {
    if *EXPERIMENTAL_FED_CM_ENABLE {
        Ok(())
    } else {
        Err(ErrorResponse::new(
            ErrorResponseType::Internal,
            "The FedCM API is disabled on this instance".to_string(),
        ))
    }
}

/// Checks for `Sec-Fetch-Dest: webidentity`
/// Ignores validation of empty origin and referrer headers - not our job
#[inline(always)]
fn is_web_identity_fetch(req: &HttpRequest) -> Result<(), ErrorResponse> {
    if req
        .headers()
        .get("sec-fetch-dest")
        .map(|v| v.to_str().unwrap_or_default())
        == Some("webidentity")
    {
        Ok(())
    } else {
        Err(ErrorResponse::new(
            ErrorResponseType::BadRequest,
            "Expected header `Sec-Fetch-Dest: webidentity`".to_string(),
        ))
    }
}
