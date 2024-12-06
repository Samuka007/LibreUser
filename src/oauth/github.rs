//!
//! This example showcases the Github OAuth2 process for requesting access to the user's public repos and
//! email address.
//!
//! Before running it, you'll need to generate your own Github OAuth2 credentials.
//!
//! In order to run the example call:
//!
//! ```sh
//! GITHUB_CLIENT_ID=xxx GITHUB_CLIENT_SECRET=yyy cargo run --example github
//! ```
//!
//! ...and follow the instructions.
//!

use actix_web::{web, HttpMessage, HttpRequest, HttpResponse, Responder, ResponseError};

use oauth2::basic::{BasicClient, BasicErrorResponseType, BasicTokenType};
use oauth2::{
    Client, EmptyExtraTokenFields, PkceCodeChallenge, PkceCodeVerifier, RevocationErrorResponseType, StandardErrorResponse, StandardRevocableToken, StandardTokenIntrospectionResponse, StandardTokenResponse
};
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, RedirectUrl, Scope,
    TokenResponse, TokenUrl,
};
use redis::{AsyncCommands as _, RedisError};
use serde::{Deserialize, Serialize};
use url::Url;

use std::env;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

use crate::env::HOST_URL;
use super::Error;

type GitHubClient = Client<
    StandardErrorResponse<BasicErrorResponseType>,
    StandardTokenResponse<EmptyExtraTokenFields, BasicTokenType>,
    BasicTokenType,
    StandardTokenIntrospectionResponse<EmptyExtraTokenFields, BasicTokenType>,
    StandardRevocableToken,
    StandardErrorResponse<RevocationErrorResponseType>,
>;

const GITHUB_CALLBACK_PATH: &str = "/auth/github/callback";
const GITHUB_AUTH_URL: &str = "https://github.com/login/oauth/authorize";
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_USER_API_URL: &str = "https://api.github.com/user";

/// Reference: https://github.com/XAMPPRocky/octocrab/blob/fae5b089161f6e97a7cd1eb7b4c7c6aa2589ee61/src/models.rs#L488-L512
/// The simple profile for a GitHub user
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GitHubUser {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub login: String,
    pub id: u64,
    pub node_id: String,
    pub avatar_url: Url,
    pub gravatar_id: String,
    pub url: Url,
    pub html_url: Url,
    pub followers_url: Url,
    pub following_url: Url,
    pub gists_url: Url,
    pub starred_url: Url,
    pub subscriptions_url: Url,
    pub organizations_url: Url,
    pub repos_url: Url,
    pub events_url: Url,
    pub received_events_url: Url,
    pub r#type: String,
    pub site_admin: bool,
    pub starred_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub fn github_config(cfg: &mut web::ServiceConfig) {
    let client_id = env::var("GITHUB_CLIENT_ID");
    let client_secret = env::var("GITHUB_CLIENT_SECRET");
    if client_id.is_err() || client_secret.is_err() {
        log::info!("GITHUB environments are not set. Start without github auth");
        return;
    }
    let redirect_url = RedirectUrl::new(HOST_URL.to_string() + GITHUB_CALLBACK_PATH).unwrap();
    let client = BasicClient::new(
        ClientId::new(client_id.unwrap()),
        Some(ClientSecret::new(client_secret.unwrap())),
        AuthUrl::new(GITHUB_AUTH_URL.to_string()).unwrap(),
        Some(TokenUrl::new(GITHUB_TOKEN_URL.to_string()).unwrap()),
    )
    .set_redirect_uri(redirect_url);
    cfg.service(
        web::scope("/github")
            .app_data(client)
            .route("/auth", web::get().to(auth))
            .route("/callback", web::get().to(callback))
    );
}

async fn auth(github: web::Data<GitHubClient>, redis: web::Data<redis::aio::MultiplexedConnection>) -> impl Responder {
    // Generate a PKCE challenge.
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    // Create an authorization URL to which we'll redirect the user.
    let (authorize_url, csrf_state) = github
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("public_repo".to_string()))
        .add_scope(Scope::new("user:email".to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();
    // Save the CSRF state to the Redis database.
    let csrf_state = csrf_state.secret();
    let pkce_verifier = pkce_verifier.secret();
    let mut redis = (**redis).clone();
    let _ = redis.set::<_ ,_ ,()>(csrf_state, pkce_verifier).await;
    // Return the CSRF token to the client
    HttpResponse::SeeOther()
        .append_header(("Location", authorize_url.as_str()))
        .append_header(("X-CSRF-Token", csrf_state.as_str()))
        .finish()
}

async fn callback(
    query: web::Query<super::CallbackQuery>,
    github: web::Data<GitHubClient>, 
    redis: web::Data<redis::aio::MultiplexedConnection>
) -> Result<HttpResponse, Error> {
    let query = query.into_inner();
    let mut redis = (**redis).clone();
    let pkce_verifier: String = redis.get(query.state.secret()).await?;
    let pkce_verifier = PkceCodeVerifier::new(pkce_verifier);
    let token = github.exchange_code(query.code)
        .set_pkce_verifier(pkce_verifier)
        .request_async(oauth2::reqwest::async_http_client)
        .await?;

    let user = get_user_info_from_github(&token).await?;

    return Ok(HttpResponse::Ok().finish());
}

async fn get_user_info_from_github(
    token: &StandardTokenResponse<EmptyExtraTokenFields, BasicTokenType>
) -> Result<GitHubUser, Error> {
    // NB: Github returns a single comma-separated "scope" parameter instead of multiple
    // space-separated scopes. Github-specific clients can parse this scope into
    // multiple scopes by splitting at the commas. Note that it's not safe for the
    // library to do this by default because RFC 6749 allows scopes to contain commas.
    let scopes = if let Some(scopes_vec) = token.scopes() {
        scopes_vec
            .iter()
            .flat_map(|comma_separated| comma_separated.split(','))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    log::debug!("Github returned the following scopes:\n{scopes:?}\n");
    log::debug!("Token type: {:?}\n", token.token_type());

    let response = match token.token_type() {
        BasicTokenType::Bearer => {
            reqwest::Client::new()
                .get(GITHUB_USER_API_URL)
                .header("Authorization", format!("Bearer {}", token.access_token().secret()))
                .send()
                .await
                .map_err(|_| Error::Other("Failed to get user info from Github"))?
        }
        _ => {
            return Err(Error::Other("Unsupported token type"));
        }
    };

    match response.status() {
        reqwest::StatusCode::OK => {}
        reqwest::StatusCode::UNAUTHORIZED => {
            return Err(Error::Authentication);
        }
        _ => {
            return Err(Error::Other("Failed to get user info from Github"));
        }
    }

    let user_info = response
        .json::<GitHubUser>()
        .await
        .map_err(|_| Error::Other("Failed to parse user info from Github"))?;


    log::debug!("Github return info: {:?}\n", user_info);

    Ok(user_info)
}