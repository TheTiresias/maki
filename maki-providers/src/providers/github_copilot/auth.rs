use std::env;
use std::time::Duration;
use std::{io, thread};

use isahc::ReadResponseExt;
use isahc::config::Configurable;
use maki_storage::DataDir;
use maki_storage::auth::{OAuthTokens, delete_tokens, load_tokens, save_tokens};
use serde::Deserialize;
use tracing::{debug, error, warn};

use crate::AgentError;
use crate::providers::ResolvedAuth;

pub(crate) const PROVIDER: &str = "github-copilot";
const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DEFAULT_DOMAIN: &str = "github.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_SAFETY_MARGIN: Duration = Duration::from_secs(5);
const POLL_TIMEOUT: Duration = Duration::from_secs(300);
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

const COPILOT_HEADERS: [(&str, &str); 4] = [
    ("User-Agent", "GitHubCopilotChat/0.35.0"),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];

fn http_client(timeout: Duration) -> Result<isahc::HttpClient, AgentError> {
    isahc::HttpClient::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        .build()
        .map_err(|e| AgentError::Config {
            message: format!("http client: {e}"),
        })
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
    #[serde(default)]
    #[allow(dead_code)]
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct CopilotTokenResponse {
    token: String,
    expires_at: u64,
}

fn request_device_code(domain: &str) -> Result<DeviceCodeResponse, AgentError> {
    let client = http_client(TOKEN_EXCHANGE_TIMEOUT)?;
    let body = format!(
        "client_id={}&scope=read:user",
        crate::providers::urlenc(CLIENT_ID),
    );

    let request = isahc::Request::builder()
        .method("POST")
        .uri(format!("https://{domain}/login/device/code"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(body.into_bytes())?;

    let mut resp = client.send(request).map_err(|e| AgentError::Config {
        message: format!("device code request: {e}"),
    })?;

    if resp.status().as_u16() != 200 {
        let body_text = resp.text().unwrap_or_else(|_| "unknown error".into());
        return Err(AgentError::Config {
            message: format!(
                "device code request failed ({}): {body_text}",
                resp.status()
            ),
        });
    }

    let body_text = resp.text()?;
    serde_json::from_str(&body_text).map_err(Into::into)
}

fn poll_for_access_token(
    domain: &str,
    device_code: &DeviceCodeResponse,
) -> Result<String, AgentError> {
    let client = http_client(POLL_TIMEOUT)?;
    let interval = Duration::from_secs(device_code.interval.max(5)) + POLL_SAFETY_MARGIN;
    let deadline = std::time::Instant::now() + POLL_TIMEOUT;

    let body = format!(
        "client_id={}&device_code={}&grant_type=urn:ietf:params:oauth:grant-type:device_code",
        crate::providers::urlenc(CLIENT_ID),
        crate::providers::urlenc(&device_code.device_code),
    );

    loop {
        if std::time::Instant::now() > deadline {
            return Err(AgentError::Config {
                message: "device authorization timed out".into(),
            });
        }

        thread::sleep(interval);

        let request = isahc::Request::builder()
            .method("POST")
            .uri(format!("https://{domain}/login/oauth/access_token"))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(body.as_bytes().to_vec())?;

        let mut resp = client.send(request).map_err(|e| AgentError::Config {
            message: format!("access token poll: {e}"),
        })?;

        let status = resp.status().as_u16();
        let body_text = resp.text().unwrap_or_else(|_| String::new());

        let parsed: serde_json::Value = match serde_json::from_str(&body_text) {
            Ok(v) => v,
            Err(e) => {
                if status != 200 {
                    return Err(AgentError::Config {
                        message: format!("access token poll failed ({status}): {body_text}"),
                    });
                }
                return Err(AgentError::Config {
                    message: format!("failed to parse access token response: {e}"),
                });
            }
        };

        if let Some(error_type) = parsed.get("error").and_then(|v| v.as_str()) {
            match error_type {
                "authorization_pending" => continue,
                "slow_down" => {
                    thread::sleep(Duration::from_secs(5));
                    continue;
                }
                _ => {
                    return Err(AgentError::Config {
                        message: format!("access token poll failed: {body_text}"),
                    });
                }
            }
        }

        let access_token = parsed["access_token"]
            .as_str()
            .ok_or_else(|| AgentError::Config {
                message: format!("access token response missing access_token: {body_text}"),
            })?;
        return Ok(access_token.to_string());
    }
}

fn exchange_copilot_token(
    github_access_token: &str,
    domain: &str,
) -> Result<CopilotTokenResponse, AgentError> {
    let client = http_client(TOKEN_EXCHANGE_TIMEOUT)?;
    let url = format!("https://api.{domain}/copilot_internal/v2/token");

    let mut builder = isahc::Request::builder()
        .method("GET")
        .uri(&url)
        .header("authorization", format!("token {github_access_token}"));
    for (key, value) in &COPILOT_HEADERS {
        builder = builder.header(*key, *value);
    }
    let request = builder.body(())?;

    let mut resp = client.send(request).map_err(|e| AgentError::Config {
        message: format!("copilot token exchange: {e}"),
    })?;

    if resp.status().as_u16() != 200 {
        let body_text = resp.text().unwrap_or_else(|_| "unknown error".into());
        return Err(AgentError::Config {
            message: format!(
                "copilot token exchange failed ({}): {body_text}",
                resp.status()
            ),
        });
    }

    let body_text = resp.text()?;
    serde_json::from_str(&body_text).map_err(|e| AgentError::Config {
        message: format!("failed to parse copilot token: {e}"),
    })
}

pub(crate) fn parse_base_url_from_token(token: &str) -> Option<String> {
    for part in token.split(';') {
        if let Some(proxy_ep) = part.strip_prefix("proxy-ep=") {
            let host = proxy_ep.trim();
            let api_host = if host.starts_with("proxy.") {
                host.replacen("proxy.", "api.", 1)
            } else {
                format!("api.{host}")
            };
            return Some(format!("https://{api_host}"));
        }
    }
    None
}

fn enable_model(copilot_token: &str, model_id: &str, base_url: &str) -> Result<(), AgentError> {
    let client = http_client(TOKEN_EXCHANGE_TIMEOUT)?;
    let url = format!("{base_url}/models/{model_id}/policy");
    let body = serde_json::json!({"state": "enabled"});
    let json_body = serde_json::to_vec(&body)?;

    let mut builder = isahc::Request::builder()
        .method("POST")
        .uri(&url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {copilot_token}"));
    for (key, value) in &COPILOT_HEADERS {
        builder = builder.header(*key, *value);
    }
    let request = builder.body(json_body)?;

    let mut resp = client.send(request).map_err(|e| {
        warn!(model = model_id, error = %e, "failed to enable model");
        e
    })?;
    let status = resp.status().as_u16();
    if status != 200 {
        let body_text = resp.text().unwrap_or_default();
        warn!(model = model_id, status, body = %body_text, "model policy enable failed");
    }
    Ok(())
}

pub(crate) fn build_resolved_auth(copilot_token: &str, base_url: Option<String>) -> ResolvedAuth {
    let mut headers = vec![("authorization".into(), format!("Bearer {copilot_token}"))];
    for (key, value) in &COPILOT_HEADERS {
        headers.push(((*key).into(), (*value).into()));
    }
    ResolvedAuth { base_url, headers }
}

pub fn resolve(dir: &DataDir) -> Result<ResolvedAuth, AgentError> {
    if let Some(tokens) = load_tokens(dir, PROVIDER) {
        if !tokens.is_expired() {
            debug!("using Copilot OAuth authentication");
            let base_url = parse_base_url_from_token(&tokens.access).or_else(|| {
                tokens
                    .account_id
                    .as_ref()
                    .map(|d| format!("https://api.{d}"))
            });
            return Ok(build_resolved_auth(&tokens.access, base_url));
        }
        match refresh_tokens(&tokens) {
            Ok(fresh) => {
                save_tokens(dir, PROVIDER, &fresh)?;
                debug!("using Copilot OAuth authentication (refreshed)");
                let base_url = parse_base_url_from_token(&fresh.access).or_else(|| {
                    fresh
                        .account_id
                        .as_ref()
                        .map(|d| format!("https://api.{d}"))
                });
                return Ok(build_resolved_auth(&fresh.access, base_url));
            }
            Err(e) => {
                warn!(error = %e, "Copilot token refresh failed, clearing stale tokens");
                delete_tokens(dir, PROVIDER).ok();
            }
        }
    }

    for env_var in ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(token) = env::var(env_var) {
            debug!(
                env = env_var,
                "using GitHub token for Copilot authentication"
            );
            let exchange = exchange_copilot_token(&token, DEFAULT_DOMAIN)?;
            let base_url = parse_base_url_from_token(&exchange.token);
            return Ok(build_resolved_auth(&exchange.token, base_url));
        }
    }

    Err(AgentError::Config {
        message:
            "not authenticated, run `maki auth login github-copilot` or set COPILOT_GITHUB_TOKEN"
                .into(),
    })
}

pub fn login(dir: &DataDir) -> Result<(), AgentError> {
    println!("Authenticate with GitHub for Copilot access.\n");
    println!("For enterprise GitHub, enter your domain (e.g. github.example.com).");
    println!("Press Enter for github.com:");
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let domain = input.trim();
    let domain = if domain.is_empty() {
        DEFAULT_DOMAIN
    } else {
        domain
    };

    let device = request_device_code(domain)?;

    println!(
        "\nOpen this URL in your browser:\n\n  {}\n",
        device.verification_uri
    );
    println!("Enter code: {}\n", device.user_code);
    println!("Waiting for authorization...");

    let github_token = poll_for_access_token(domain, &device).map_err(|e| {
        error!(error = %e, "GitHub device authorization failed");
        e
    })?;

    let copilot = exchange_copilot_token(&github_token, domain).map_err(|e| {
        error!(error = %e, "Copilot token exchange failed");
        e
    })?;

    let base_url = parse_base_url_from_token(&copilot.token)
        .unwrap_or_else(|| format!("https://api.{domain}"));

    let all_models = crate::providers::github_copilot::models();
    for entry in all_models {
        for &model_id in entry.prefixes {
            enable_model(&copilot.token, model_id, &base_url).ok();
        }
    }

    let expires = copilot.expires_at.saturating_sub(300) * 1000;
    let tokens = OAuthTokens {
        access: copilot.token,
        refresh: github_token,
        expires,
        account_id: Some(domain.to_string()),
    };
    save_tokens(dir, PROVIDER, &tokens)?;
    println!("Authenticated successfully as GitHub Copilot.");
    Ok(())
}

pub fn logout(dir: &DataDir) -> Result<(), AgentError> {
    if delete_tokens(dir, PROVIDER)? {
        println!("Logged out of GitHub Copilot.");
    } else {
        println!("Not currently logged in to GitHub Copilot.");
    }
    Ok(())
}

pub(crate) fn refresh_tokens(tokens: &OAuthTokens) -> Result<OAuthTokens, AgentError> {
    let domain = tokens.account_id.as_deref().unwrap_or(DEFAULT_DOMAIN);
    let copilot = exchange_copilot_token(&tokens.refresh, domain)?;
    let expires = copilot.expires_at.saturating_sub(300) * 1000;
    Ok(OAuthTokens {
        access: copilot.token,
        refresh: tokens.refresh.clone(),
        expires,
        account_id: tokens.account_id.clone(),
    })
}

pub(crate) fn is_oauth(dir: &DataDir) -> bool {
    load_tokens(dir, PROVIDER).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_base_url_from_proxy_ep() {
        let token = "tid=abc;exp=123;proxy-ep=proxy.individual.githubcopilot.com;sku=monthly";
        assert_eq!(
            parse_base_url_from_token(token),
            Some("https://api.individual.githubcopilot.com".into())
        );
    }

    #[test]
    fn parse_base_url_no_proxy_ep() {
        let token = "tid=abc;exp=123;sku=monthly";
        assert_eq!(parse_base_url_from_token(token), None);
    }

    #[test]
    fn parse_base_url_enterprise_proxy() {
        let token = "tid=abc;exp=123;proxy-ep=proxy.github.myenterprise.com;sku=enterprise";
        assert_eq!(
            parse_base_url_from_token(token),
            Some("https://api.github.myenterprise.com".into())
        );
    }
}
