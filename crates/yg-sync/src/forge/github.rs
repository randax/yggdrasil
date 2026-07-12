//! The GitHub adapter: github.com and GitHub Enterprise hosts.

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use super::{BoxFuture, Forge, ListedRepo, OrgDiscovery, common_rate_limit_phrasing};

const GITHUB_PAGE_SIZE: usize = 100;

pub(crate) struct GitHubForge;

impl Forge for GitHubForge {
    fn kind(&self) -> &'static str {
        "github"
    }

    fn claims_host(&self, host: &str) -> bool {
        host == "github.com"
    }

    fn default_token_env(&self) -> Option<&'static str> {
        Some("YG_GITHUB_TOKEN")
    }

    /// github.com's REST API lives on its own host; a GitHub Enterprise
    /// instance serves it under `/api/v3` on the clone host.
    fn default_api_root(&self, base_url: &str) -> Option<String> {
        let base_url = base_url.trim_end_matches('/');
        Some(match base_url {
            "https://github.com" => "https://api.github.com".to_string(),
            enterprise => format!("{enterprise}/api/v3"),
        })
    }

    /// GitHub repos live at exactly owner/repo; a longer path is a
    /// pasted browser page (tree/…, issues/…), not a different repo —
    /// rejected rather than guessed at. And GitHub only speaks https:
    /// normalizing keeps a worker from ever sending the Forge token
    /// over plaintext because of a URL spelling, and keeps http/https
    /// variants on one forge row.
    fn canonical_scheme(
        &self,
        _scheme: &str,
        segments: &[&str],
        url: &str,
    ) -> Result<String, String> {
        if segments.len() > 2 {
            return Err(format!(
                "GitHub repositories are owner/repo — drop the trailing path \
                 (got {} extra segment(s)): {url}",
                segments.len() - 2
            ));
        }
        Ok("https".to_string())
    }

    fn is_rate_limit(&self, message: &str) -> bool {
        common_rate_limit_phrasing(message)
    }

    fn discovery(&self) -> Option<&dyn OrgDiscovery> {
        Some(self)
    }
}

#[derive(Deserialize)]
struct GitHubRepo {
    full_name: String,
    private: bool,
    visibility: Option<String>,
}

impl GitHubRepo {
    fn into_listed(self) -> ListedRepo {
        let visibility = match self.visibility.as_deref() {
            Some("internal") => yg_control::RepoVisibility::Internal,
            Some("private") => yg_control::RepoVisibility::Private,
            _ if self.private => yg_control::RepoVisibility::Private,
            _ => yg_control::RepoVisibility::Public,
        };
        ListedRepo {
            slug: self.full_name,
            visibility,
        }
    }
}

impl OrgDiscovery for GitHubForge {
    fn list_org_repos<'a>(
        &'a self,
        client: &'a reqwest::Client,
        api_root: &'a str,
        org: &'a str,
        token: Option<&'a str>,
    ) -> BoxFuture<'a, anyhow::Result<Vec<ListedRepo>>> {
        Box::pin(async move {
            let mut next = Some(format!(
                "{}/orgs/{org}/repos?per_page={GITHUB_PAGE_SIZE}&type=all",
                api_root.trim_end_matches('/')
            ));
            let mut repos = Vec::new();
            while let Some(url) = next.take() {
                let mut request = client
                    .get(&url)
                    .header(reqwest::header::USER_AGENT, "yggdrasil-sync");
                if let Some(token) = token {
                    request = request.bearer_auth(token);
                }
                let response = request
                    .send()
                    .await
                    .with_context(|| format!("listing GitHub repos for org {org}"))?;
                let status = response.status();
                let link = response
                    .headers()
                    .get(reqwest::header::LINK)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                if !status.is_success() {
                    let text = response.text().await.unwrap_or_default();
                    anyhow::bail!("GitHub repo discovery for {org} returned {status}: {text}");
                }
                let page: Vec<GitHubRepo> = response
                    .json()
                    .await
                    .with_context(|| format!("parsing GitHub repo list for org {org}"))?;
                repos.extend(page.into_iter().map(GitHubRepo::into_listed));
                next = link.as_deref().and_then(next_link);
            }
            Ok(repos)
        })
    }
}

/// The HTTP client discovery requests share: bounded, so one wedged
/// forge API can't hold a worker's discovery loop open forever.
pub(crate) fn discovery_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("building the forge discovery client")
}

fn next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (url, attrs) = part.trim().split_once(';')?;
        attrs
            .split(';')
            .any(|attr| attr.trim() == r#"rel="next""#)
            .then(|| {
                url.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string()
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn github_org_listing_maps_public_internal_and_private_visibility() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 1024];
            let _ = socket.readable().await;
            let _ = socket.try_read(&mut buf);
            let body = r#"[
                {"full_name":"acme/public-widgets","private":false,"visibility":"public"},
                {"full_name":"acme/internal-widgets","private":false,"visibility":"internal"},
                {"full_name":"acme/private-widgets","private":true,"visibility":"private"}
            ]"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.writable().await.unwrap();
            socket.try_write(response.as_bytes()).unwrap();
        });

        let client = discovery_client();
        // The API root is explicit — a local test server here; the
        // Forge record's field in production.
        let repos = GitHubForge
            .list_org_repos(&client, &format!("http://{addr}"), "acme", None)
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(repos.len(), 3);
        assert_eq!(repos[0].slug, "acme/public-widgets");
        assert_eq!(repos[0].visibility, yg_control::RepoVisibility::Public);
        assert_eq!(repos[1].visibility, yg_control::RepoVisibility::Internal);
        assert_eq!(repos[2].visibility, yg_control::RepoVisibility::Private);
    }

    #[test]
    fn github_discovery_defaults_to_the_rest_api_root_not_the_clone_root() {
        assert_eq!(
            GitHubForge.default_api_root("https://github.com").unwrap(),
            "https://api.github.com"
        );
        assert_eq!(
            GitHubForge
                .default_api_root("https://github.enterprise.example")
                .unwrap(),
            "https://github.enterprise.example/api/v3"
        );
    }
}
