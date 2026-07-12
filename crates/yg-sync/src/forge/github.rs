//! The GitHub adapter: org discovery over the REST API.

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use super::ListedRepo;

const GITHUB_PAGE_SIZE: usize = 100;

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

pub(crate) async fn list_github_org_repos(
    client: &reqwest::Client,
    base_url: &str,
    org: &str,
    token: Option<&str>,
) -> anyhow::Result<Vec<ListedRepo>> {
    let api_root = github_api_root(base_url);
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
}

pub(crate) fn github_discovery_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("building the GitHub discovery client")
}

pub(crate) fn github_api_root(base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    match base_url {
        "https://github.com" => "https://api.github.com".to_string(),
        // Test fixtures use a tiny local HTTP server, not a GitHub
        // Enterprise URL shape.
        local
            if local.starts_with("http://127.0.0.1:") || local.starts_with("http://localhost:") =>
        {
            local.to_string()
        }
        enterprise => format!("{enterprise}/api/v3"),
    }
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

        let client = github_discovery_client();
        let repos = list_github_org_repos(&client, &format!("http://{addr}"), "acme", None)
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
    fn github_discovery_uses_the_rest_api_root_not_the_clone_root() {
        assert_eq!(
            github_api_root("https://github.com"),
            "https://api.github.com"
        );
        assert_eq!(
            github_api_root("https://github.enterprise.example"),
            "https://github.enterprise.example/api/v3"
        );
    }
}
