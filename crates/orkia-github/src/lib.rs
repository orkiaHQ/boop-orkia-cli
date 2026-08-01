//! GitHub App adapter. It contains transport concerns only, never review logic.

use hmac::{Hmac, Mac};
use orkia_model::{ForgeReview, OrkiaError, Result};
use orkia_ports::Forge;
use reqwest::blocking::Client;
use serde::Serialize;
use sha2::Sha256;
use url::Url;

#[derive(Clone, Debug)]
pub struct GitHubApp {
    pub api_base: Url,
    pub owner: String,
    pub repository: String,
    pub installation_token: String,
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
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("repos/{}/{}/pulls", self.owner, self.repository),
            )
            .json(&Request {
                title: &review.title,
                head: &review.branch,
                base: &review.base,
                body: &review.body,
            })
            .send()
            .map_err(|e| OrkiaError::External(format!("GitHub publish: {e}")))?;
        if !response.status().is_success() {
            return Err(OrkiaError::External(format!(
                "GitHub publish returned {}",
                response.status()
            )));
        }
        let value: serde_json::Value = response
            .json()
            .map_err(|e| OrkiaError::External(e.to_string()))?;
        value
            .get("html_url")
            .and_then(|url| url.as_str())
            .map(str::to_owned)
            .ok_or_else(|| OrkiaError::External("GitHub did not return a PR URL".into()))
    }
    fn set_required_checks(&self, branch: &str, checks: &[String]) -> Result<()> {
        #[derive(Serialize)]
        struct Status<'a> {
            strict: bool,
            contexts: &'a [String],
        }
        #[derive(Serialize)]
        struct Protection<'a> {
            required_status_checks: Status<'a>,
            enforce_admins: bool,
            required_pull_request_reviews: serde_json::Value,
            restrictions: serde_json::Value,
        }
        let response = self.request(reqwest::Method::PUT, &format!("repos/{}/{}/branches/{}/protection", self.owner, self.repository, branch)).json(&Protection { required_status_checks: Status { strict: true, contexts: checks }, enforce_admins: true, required_pull_request_reviews: serde_json::json!({"required_approving_review_count": 1}), restrictions: serde_json::Value::Null }).send().map_err(|e| OrkiaError::External(format!("GitHub protection: {e}")))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(OrkiaError::External(format!(
                "GitHub protection returned {}",
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
    #[test]
    fn accepts_valid_webhook() {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(b"payload");
        let header = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_webhook(b"secret", &header, b"payload"));
    }
}
