//! GitHub App adapter. It contains transport concerns only, never review logic.

use hmac::{Hmac, Mac};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use orkia_model::{ForgeReview, OrkiaError, Result};
use orkia_ports::Forge;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use time::OffsetDateTime;
use url::Url;

#[derive(Clone, Debug)]
pub struct GitHubApp {
    pub api_base: Url,
    pub owner: String,
    pub repository: String,
    pub installation_token: String,
}

/// Transport-level webhook envelope. The adapter validates authenticity and
/// JSON shape but deliberately leaves review/stack decisions to the server
/// composition root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookEvent {
    pub event_name: String,
    pub delivery_id: String,
    pub payload: serde_json::Value,
}

pub fn parse_webhook(event_name: &str, delivery_id: &str, payload: &[u8]) -> Result<WebhookEvent> {
    if event_name.trim().is_empty() || delivery_id.trim().is_empty() {
        return Err(OrkiaError::Invalid(
            "GitHub webhook event and delivery headers cannot be empty".into(),
        ));
    }
    let payload: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|error| OrkiaError::Invalid(format!("invalid GitHub webhook JSON: {error}")))?;
    if !payload.is_object() {
        return Err(OrkiaError::Invalid(
            "GitHub webhook payload must be a JSON object".into(),
        ));
    }
    Ok(WebhookEvent {
        event_name: event_name.into(),
        delivery_id: delivery_id.into(),
        payload,
    })
}

/// Credentials used only to exchange a short-lived GitHub App JWT for the
/// installation token that GitHub expects on repository API calls.  The PEM is
/// intentionally borrowed: callers retain ownership and should obtain it from
/// their platform secret store rather than from a repository file.
#[derive(Debug, Clone, Copy)]
pub struct GitHubAppCredentials<'a> {
    pub app_id: u64,
    pub installation_id: u64,
    pub private_key_pem: &'a [u8],
}

impl GitHubApp {
    pub fn new(
        owner: impl Into<String>,
        repository: impl Into<String>,
        installation_token: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self {
            api_base: Url::parse("https://api.github.com/")
                .map_err(|e| OrkiaError::Invalid(e.to_string()))?,
            owner: owner.into(),
            repository: repository.into(),
            installation_token: installation_token.into(),
        })
    }

    /// Authenticates as a GitHub App and exchanges its RS256 JWT for a scoped,
    /// short-lived installation token.  It is deliberately separate from
    /// [`Self::new`], which accepts an already-issued installation token for
    /// deployments that inject one through a dedicated credential broker.
    pub fn from_app_credentials(
        owner: impl Into<String>,
        repository: impl Into<String>,
        credentials: GitHubAppCredentials<'_>,
    ) -> Result<Self> {
        let api_base = Url::parse("https://api.github.com/")
            .map_err(|error| OrkiaError::Invalid(error.to_string()))?;
        Self::from_app_credentials_at(api_base, owner, repository, credentials)
    }

    /// Same authentication flow as [`Self::from_app_credentials`], with an
    /// explicit API endpoint for GitHub Enterprise and transport tests.
    pub fn from_app_credentials_at(
        api_base: Url,
        owner: impl Into<String>,
        repository: impl Into<String>,
        credentials: GitHubAppCredentials<'_>,
    ) -> Result<Self> {
        #[derive(Serialize)]
        struct Claims {
            iat: i64,
            exp: i64,
            iss: String,
        }
        #[derive(Deserialize)]
        struct InstallationToken {
            token: String,
        }

        if credentials.app_id == 0 || credentials.installation_id == 0 {
            return Err(OrkiaError::Invalid(
                "GitHub App and installation IDs must be non-zero".into(),
            ));
        }
        let now = OffsetDateTime::now_utc().unix_timestamp();
        // GitHub accepts JWTs lasting at most ten minutes.  Starting one
        // minute in the past tolerates minor clock skew without making the
        // token long-lived.
        let claims = Claims {
            iat: now - 60,
            exp: now + 9 * 60,
            iss: credentials.app_id.to_string(),
        };
        let key = EncodingKey::from_rsa_pem(credentials.private_key_pem).map_err(|error| {
            OrkiaError::Invalid(format!("invalid GitHub App private key: {error}"))
        })?;
        let jwt = encode(&Header::new(Algorithm::RS256), &claims, &key)
            .map_err(|error| OrkiaError::External(format!("sign GitHub App JWT: {error}")))?;
        let endpoint = api_base
            .join(&format!(
                "app/installations/{}/access_tokens",
                credentials.installation_id
            ))
            .map_err(|error| OrkiaError::Invalid(format!("GitHub API path: {error}")))?;
        let response = Client::new()
            .post(endpoint)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "orkia-github")
            .bearer_auth(jwt)
            .send()
            .map_err(|error| {
                OrkiaError::External(format!("GitHub App installation token: {error}"))
            })?;
        if !response.status().is_success() {
            return Err(OrkiaError::External(format!(
                "GitHub App installation token returned {}",
                response.status()
            )));
        }
        let token: InstallationToken = response.json().map_err(|error| {
            OrkiaError::External(format!("GitHub App installation token body: {error}"))
        })?;
        if token.token.is_empty() {
            return Err(OrkiaError::External(
                "GitHub App installation token response contained an empty token".into(),
            ));
        }
        Ok(Self {
            api_base,
            owner: owner.into(),
            repository: repository.into(),
            installation_token: token.token,
        })
    }
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        Client::new()
            .request(
                method,
                self.api_base.join(path).expect("valid GitHub API path"),
            )
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(&self.installation_token)
            .header("User-Agent", "orkia-github")
    }
    pub fn verify_webhook(&self, secret: &[u8], signature_header: &str, payload: &[u8]) -> bool {
        verify_webhook(secret, signature_header, payload)
    }

    fn pull_request_url(value: serde_json::Value, operation: &str) -> Result<String> {
        value
            .get("html_url")
            .and_then(|url| url.as_str())
            .map(str::to_owned)
            .ok_or_else(|| {
                OrkiaError::External(format!("GitHub {operation} did not return a PR URL"))
            })
    }

    fn open_pull_number(&self, branch: &str) -> Result<Option<u64>> {
        let head = format!("{}:{branch}", self.owner);
        let encoded = url::form_urlencoded::byte_serialize(head.as_bytes()).collect::<String>();
        let response = self
            .request(
                reqwest::Method::GET,
                &format!(
                    "repos/{}/{}/pulls?state=open&head={encoded}",
                    self.owner, self.repository
                ),
            )
            .send()
            .map_err(|error| OrkiaError::External(format!("GitHub find pull: {error}")))?;
        if !response.status().is_success() {
            return Err(OrkiaError::External(format!(
                "GitHub find pull returned {}",
                response.status()
            )));
        }
        let pulls: Vec<serde_json::Value> = response
            .json()
            .map_err(|error| OrkiaError::External(error.to_string()))?;
        pulls
            .into_iter()
            .next()
            .map(|pull| {
                pull.get("number")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| OrkiaError::External("GitHub pull has no number".into()))
            })
            .transpose()
    }
}
impl Forge for GitHubApp {
    fn publish(&self, review: &ForgeReview) -> Result<String> {
        #[derive(Serialize)]
        struct Request<'a> {
            title: &'a str,
            head: &'a str,
            base: &'a str,
            body: &'a str,
        }
        let existing = self.open_pull_number(&review.branch)?;
        let request = Request {
            title: &review.title,
            head: &review.branch,
            base: &review.base,
            body: &review.body,
        };
        let response = match existing {
            Some(number) => self
                .request(
                    reqwest::Method::PATCH,
                    &format!("repos/{}/{}/pulls/{number}", self.owner, self.repository),
                )
                .json(&request)
                .send()
                .map_err(|error| OrkiaError::External(format!("GitHub update pull: {error}")))?,
            None => self
                .request(
                    reqwest::Method::POST,
                    &format!("repos/{}/{}/pulls", self.owner, self.repository),
                )
                .json(&request)
                .send()
                .map_err(|error| OrkiaError::External(format!("GitHub publish: {error}")))?,
        };
        if !response.status().is_success() {
            return Err(OrkiaError::External(format!(
                "GitHub pull publication returned {}",
                response.status()
            )));
        }
        let value: serde_json::Value = response
            .json()
            .map_err(|e| OrkiaError::External(e.to_string()))?;
        Self::pull_request_url(value, "pull publication")
    }
    fn set_required_checks(
        &self,
        branch: &str,
        checks: &[String],
        required_approvals: u8,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct Status<'a> {
            strict: bool,
            contexts: &'a [String],
        }
        #[derive(Serialize)]
        struct Protection<'a> {
            required_status_checks: Status<'a>,
            enforce_admins: bool,
            required_pull_request_reviews: RequiredReviews,
            restrictions: serde_json::Value,
        }
        #[derive(Serialize)]
        struct RequiredReviews {
            required_approving_review_count: u8,
        }
        let response = self
            .request(
                reqwest::Method::PUT,
                &format!(
                    "repos/{}/{}/branches/{}/protection",
                    self.owner, self.repository, branch
                ),
            )
            .json(&Protection {
                required_status_checks: Status {
                    strict: true,
                    contexts: checks,
                },
                enforce_admins: true,
                required_pull_request_reviews: RequiredReviews {
                    required_approving_review_count: required_approvals,
                },
                restrictions: serde_json::Value::Null,
            })
            .send()
            .map_err(|e| OrkiaError::External(format!("GitHub protection: {e}")))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(OrkiaError::External(format!(
                "GitHub protection returned {}",
                response.status()
            )))
        }
    }
    fn publish_check(&self, commit: &str, name: &str, passed: bool, summary: &str) -> Result<()> {
        #[derive(Serialize)]
        struct Output<'a> {
            title: &'a str,
            summary: &'a str,
        }
        #[derive(Serialize)]
        struct Check<'a> {
            name: &'a str,
            head_sha: &'a str,
            status: &'static str,
            conclusion: &'static str,
            output: Output<'a>,
        }
        if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(OrkiaError::Invalid(
                "a GitHub check requires a full 160-bit or 256-bit hexadecimal Git commit ID"
                    .into(),
            ));
        }
        let conclusion = if passed { "success" } else { "failure" };
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("repos/{}/{}/check-runs", self.owner, self.repository),
            )
            .json(&Check {
                name,
                head_sha: commit,
                status: "completed",
                conclusion,
                output: Output {
                    title: name,
                    summary,
                },
            })
            .send()
            .map_err(|error| OrkiaError::External(format!("GitHub check publication: {error}")))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(OrkiaError::External(format!(
                "GitHub check publication returned {}",
                response.status()
            )))
        }
    }
}

pub fn verify_webhook(secret: &[u8], signature_header: &str, payload: &[u8]) -> bool {
    let Some(hex_signature) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_signature) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(payload);
    mac.verify_slice(&expected).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_model::ForgeReview;
    use orkia_ports::Forge;
    use rand_core::OsRng;
    use rsa::{
        RsaPrivateKey,
        pkcs8::{EncodePrivateKey, LineEnding},
    };
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn test_rsa_private_key() -> Vec<u8> {
        RsaPrivateKey::new(&mut OsRng, 2048)
            .unwrap()
            .to_pkcs8_pem(LineEnding::LF)
            .unwrap()
            .as_bytes()
            .to_vec()
    }
    #[test]
    fn accepts_valid_webhook() {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(b"payload");
        let header = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_webhook(b"secret", &header, b"payload"));
    }

    #[test]
    fn parses_only_object_webhook_payloads_with_identity_headers() {
        let event = parse_webhook("pull_request", "delivery-1", br#"{"action":"closed"}"#).unwrap();
        assert_eq!(event.event_name, "pull_request");
        assert_eq!(event.delivery_id, "delivery-1");
        assert!(parse_webhook("pull_request", "delivery-1", b"[]").is_err());
        assert!(parse_webhook("", "delivery-1", b"{}").is_err());
    }

    #[test]
    fn publish_creates_a_pr_only_after_checking_for_an_existing_one() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            for (expected, response) in [
                (
                    "GET /repos/acme/repo/pulls?state=open&head=acme%3Aorkia%2Fstack-pr%2Fone",
                    "[]",
                ),
                (
                    "POST /repos/acme/repo/pulls",
                    r#"{"html_url":"https://github.test/pr/1"}"#,
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                assert!(request.starts_with(expected), "{request}");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(), response
                )
                .unwrap();
            }
        });
        let mut github = GitHubApp::new("acme", "repo", "token").unwrap();
        github.api_base = Url::parse(&format!("http://{address}/")).unwrap();
        let url = github
            .publish(&ForgeReview {
                unit: None,
                pull_request: None,
                branch: "orkia/stack-pr/one".into(),
                base: "main".into(),
                title: "One".into(),
                body: "body".into(),
            })
            .unwrap();
        worker.join().unwrap();
        assert_eq!(url, "https://github.test/pr/1");
    }

    #[test]
    fn publishes_a_completed_check_for_the_exact_projected_commit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(
                request.starts_with("POST /repos/acme/repo/check-runs"),
                "{request}"
            );
            assert!(
                request.contains("\"name\":\"orkia/integrate\""),
                "{request}"
            );
            assert!(request.contains("\"conclusion\":\"success\""), "{request}");
            write!(
                stream,
                "HTTP/1.1 201 Created\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            )
            .unwrap();
        });
        let mut github = GitHubApp::new("acme", "repo", "token").unwrap();
        github.api_base = Url::parse(&format!("http://{address}/")).unwrap();
        github
            .publish_check(
                "aabbccddeeff00112233445566778899aabbccdd",
                "orkia/integrate",
                true,
                "policy passed",
            )
            .unwrap();
        worker.join().unwrap();
        assert!(
            github
                .publish_check("short", "orkia/integrate", true, "policy passed")
                .is_err()
        );
    }

    #[test]
    fn protected_branch_uses_the_policy_approval_quorum() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(
                request.starts_with("PUT /repos/acme/repo/branches/main/protection"),
                "{request}"
            );
            assert!(request.contains("\"orkia/integrate\""), "{request}");
            assert!(
                request.contains("\"required_approving_review_count\":2"),
                "{request}"
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            )
            .unwrap();
        });
        let mut github = GitHubApp::new("acme", "repo", "token").unwrap();
        github.api_base = Url::parse(&format!("http://{address}/")).unwrap();
        github
            .set_required_checks("main", &["orkia/integrate".into()], 2)
            .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn exchanges_a_signed_app_jwt_for_an_installation_token() {
        let private_key = test_rsa_private_key();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(
                request.starts_with("POST /app/installations/42/access_tokens"),
                "{request}"
            );
            assert!(request.contains("authorization: Bearer ey"), "{request}");
            write!(
                stream,
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 24\r\nConnection: close\r\n\r\n{{\"token\":\"issued-token\"}}"
            )
            .unwrap();
        });
        let github = GitHubApp::from_app_credentials_at(
            Url::parse(&format!("http://{address}/")).unwrap(),
            "acme",
            "repo",
            GitHubAppCredentials {
                app_id: 7,
                installation_id: 42,
                private_key_pem: &private_key,
            },
        )
        .unwrap();
        worker.join().unwrap();
        assert_eq!(github.installation_token, "issued-token");
    }
}
