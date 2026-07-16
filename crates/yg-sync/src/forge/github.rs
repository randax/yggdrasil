//! The GitHub adapter: github.com and GitHub Enterprise hosts.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use serde::Deserialize;

use super::{
    BoxFuture, Forge, ForgeRateLimit, ForgeRequestBudget, ListedRepo, OrgDiscovery,
    acquire_forge_request, common_rate_limit_phrasing,
};
use crate::rate::RATE_LIMIT_COOLDOWN;

const GITHUB_PAGE_SIZE: usize = 100;
/// Maximum pages one GitHub org listing may request. At GitHub's maximum page
/// size this permits 100,000 repositories while bounding malformed pagination.
const GITHUB_ORG_PAGE_LIMIT: usize = 1_000;
/// Smallest server-requested cooldown honored for a GitHub API rate limit.
const MIN_RETRY_AFTER: Duration = Duration::from_secs(1);
/// Largest server-requested cooldown honored for a GitHub API rate limit.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);
const SECONDARY_RATE_LIMIT_MESSAGE: &str = "you have exceeded a secondary rate limit";

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
    ) -> Result<String, crate::locator::LocatorError> {
        if segments.len() > 2 {
            return Err(crate::locator::LocatorError::GitHubSubpageUrl {
                extra: segments.len() - 2,
                url: url.into(),
            });
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
        list_org_repos(client, api_root, org, token, None)
    }

    fn list_org_repos_budgeted<'a>(
        &'a self,
        client: &'a reqwest::Client,
        api_root: &'a str,
        org: &'a str,
        token: Option<&'a str>,
        budget: &'a dyn ForgeRequestBudget,
    ) -> BoxFuture<'a, anyhow::Result<Vec<ListedRepo>>> {
        list_org_repos(client, api_root, org, token, Some(budget))
    }
}

fn list_org_repos<'a>(
    client: &'a reqwest::Client,
    api_root: &'a str,
    org: &'a str,
    token: Option<&'a str>,
    budget: Option<&'a dyn ForgeRequestBudget>,
) -> BoxFuture<'a, anyhow::Result<Vec<ListedRepo>>> {
    Box::pin(async move {
        let mut next = Some(format!(
            "{}/orgs/{org}/repos?per_page={GITHUB_PAGE_SIZE}&type=all",
            api_root.trim_end_matches('/')
        ));
        let mut repos = Vec::new();
        let mut pagination = Pagination::default();
        while let Some(url) = next.take() {
            pagination.begin_page(&url)?;
            if let Some(budget) = budget {
                acquire_forge_request(budget).await;
            }
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
            let headers = response.headers();
            let has_retry_after = headers.contains_key(reqwest::header::RETRY_AFTER);
            let retry_after_header = headers
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let remaining_exhausted = headers
                .get("x-ratelimit-remaining")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.trim() == "0");
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || (status == reqwest::StatusCode::FORBIDDEN
                    && (has_retry_after || remaining_exhausted))
            {
                let retry_after =
                    retry_after_or_default(retry_after_header.as_deref(), SystemTime::now());
                return Err(ForgeRateLimit::new(status, retry_after).into());
            }
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                if status == reqwest::StatusCode::FORBIDDEN
                    && is_secondary_rate_limit_message(&text)
                {
                    let retry_after =
                        retry_after_or_default(retry_after_header.as_deref(), SystemTime::now());
                    return Err(ForgeRateLimit::new(status, retry_after).into());
                }
                return Err(GitHubListingError::HttpStatus {
                    status,
                    org: org.to_string(),
                    response_body: text,
                }
                .into());
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

#[derive(Default)]
struct Pagination {
    pages: usize,
    visited: HashSet<String>,
}

#[derive(Debug, thiserror::Error)]
enum GitHubPaginationError {
    #[error("GitHub org listing exceeded its {limit}-page limit")]
    PageLimit { limit: usize },
    #[error("GitHub org listing repeated next-link {url}")]
    Cycle { url: String },
}

#[derive(Debug, thiserror::Error)]
pub enum GitHubListingError {
    #[error("GitHub repo discovery for {org} returned {status}: {response_body}")]
    HttpStatus {
        status: reqwest::StatusCode,
        org: String,
        response_body: String,
    },
}

impl Pagination {
    fn begin_page(&mut self, url: &str) -> Result<(), GitHubPaginationError> {
        if self.pages >= GITHUB_ORG_PAGE_LIMIT {
            tracing::warn!(
                pages = self.pages,
                url,
                "org listing truncated at the pagination cap; later repositories \
                 were not discovered"
            );
            return Err(GitHubPaginationError::PageLimit {
                limit: GITHUB_ORG_PAGE_LIMIT,
            });
        }
        if !self.visited.insert(url.to_string()) {
            tracing::warn!(
                pages = self.pages,
                url,
                "org listing terminated on a repeated next-link; the listing may \
                 be incomplete"
            );
            return Err(GitHubPaginationError::Cycle {
                url: url.to_string(),
            });
        }
        self.pages += 1;
        Ok(())
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

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let duration = value
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            let deadline = parse_imf_fixdate(value.trim())?;
            Some(deadline.duration_since(now).unwrap_or(Duration::ZERO))
        })?;
    Some(duration.clamp(MIN_RETRY_AFTER, MAX_RETRY_AFTER))
}

fn retry_after_or_default(value: Option<&str>, now: SystemTime) -> Duration {
    value
        .and_then(|value| parse_retry_after(value, now))
        .unwrap_or(RATE_LIMIT_COOLDOWN)
}

fn is_secondary_rate_limit_message(body: &str) -> bool {
    body.to_ascii_lowercase()
        .contains(SECONDARY_RATE_LIMIT_MESSAGE)
}

/// Parse the IMF-fixdate form emitted by modern HTTP servers, for example
/// `Wed, 21 Oct 2015 07:28:00 GMT`.
fn parse_imf_fixdate(value: &str) -> Option<SystemTime> {
    let mut fields = value.split_ascii_whitespace();
    let weekday = fields.next()?;
    let day = fields.next()?.parse::<u32>().ok()?;
    let month = parse_month(fields.next()?)?;
    let year = fields.next()?.parse::<i64>().ok()?;
    let (hour, minute, second) = parse_time(fields.next()?)?;
    let timezone = fields.next()?;
    if fields.next().is_some()
        || !weekday.ends_with(',')
        || timezone != "GMT"
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days = days_since_unix_epoch(year, month, day);
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second))?;
    let seconds = u64::try_from(seconds).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

fn parse_month(value: &str) -> Option<u32> {
    [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|month| *month == value)
    .map(|index| index as u32 + 1)
}

fn parse_time(value: &str) -> Option<(u32, u32, u32)> {
    let mut fields = value.split(':');
    let hour = fields.next()?.parse().ok()?;
    let minute = fields.next()?.parse().ok()?;
    let second = fields.next()?.parse().ok()?;
    fields.next().is_none().then_some((hour, minute, second))
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

fn days_since_unix_epoch(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct CountingBudget(AtomicUsize);

    impl ForgeRequestBudget for CountingBudget {
        fn take(
            &self,
        ) -> super::super::BoxFuture<'_, Result<(), super::super::ForgeBudgetExhausted>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        }
    }

    struct RefillingBudget(AtomicUsize);

    impl ForgeRequestBudget for RefillingBudget {
        fn take(
            &self,
        ) -> super::super::BoxFuture<'_, Result<(), super::super::ForgeBudgetExhausted>> {
            Box::pin(async move {
                let attempt = self.0.fetch_add(1, Ordering::Relaxed);
                if attempt == 0 {
                    Err(super::super::ForgeBudgetExhausted {
                        retry_after: Duration::from_millis(1),
                    })
                } else {
                    Ok(())
                }
            })
        }
    }

    #[test]
    fn github_pagination_stops_at_the_named_page_limit() {
        let mut pagination = Pagination::default();
        let first_page = "https://example.test/page/0";
        pagination.begin_page(first_page).unwrap();
        assert!(matches!(
            pagination.begin_page(first_page),
            Err(GitHubPaginationError::Cycle { url }) if url == first_page
        ));

        let mut pagination = Pagination::default();
        for page in 0..GITHUB_ORG_PAGE_LIMIT {
            pagination
                .begin_page(&format!("https://example.test/page/{page}"))
                .unwrap();
        }
        assert!(matches!(
            pagination.begin_page("https://example.test/page/overflow"),
            Err(GitHubPaginationError::PageLimit {
                limit: GITHUB_ORG_PAGE_LIMIT
            })
        ));
    }

    #[test]
    fn retry_after_seconds_produce_a_bounded_forge_cooldown() {
        let cooldown = retry_after_or_default(Some("120"), SystemTime::UNIX_EPOCH);
        let signal = ForgeRateLimit::new(reqwest::StatusCode::TOO_MANY_REQUESTS, cooldown);
        assert_eq!(signal.retry_after(), Duration::from_secs(120));
    }

    #[test]
    fn retry_after_http_date_produces_a_bounded_forge_cooldown() {
        let deadline = parse_imf_fixdate("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        let now = deadline - Duration::from_secs(90);
        let cooldown = retry_after_or_default(Some("Wed, 21 Oct 2015 07:28:00 GMT"), now);
        let signal = ForgeRateLimit::new(reqwest::StatusCode::TOO_MANY_REQUESTS, cooldown);
        assert_eq!(signal.retry_after(), Duration::from_secs(90));

        let expired = retry_after_or_default(
            Some("Wed, 21 Oct 2015 07:28:00 GMT"),
            deadline + Duration::from_secs(1),
        );
        assert_eq!(expired, MIN_RETRY_AFTER);
    }

    #[tokio::test]
    async fn github_429_honors_retry_after_seconds_and_http_date_headers() {
        let Some(seconds) = retry_after_from_429_fixture("120").await else {
            return;
        };
        assert_eq!(seconds, Duration::from_secs(120));

        let date = retry_after_from_429_fixture("Wed, 21 Oct 2099 07:28:00 GMT")
            .await
            .expect("the second fixture binds when the first one did");
        assert_eq!(date, MAX_RETRY_AFTER);
    }

    #[test]
    fn unusable_retry_after_falls_back_and_server_values_are_clamped() {
        assert_eq!(
            retry_after_or_default(Some("not-a-date"), SystemTime::UNIX_EPOCH),
            RATE_LIMIT_COOLDOWN
        );
        assert_eq!(
            retry_after_or_default(Some("0"), SystemTime::UNIX_EPOCH),
            MIN_RETRY_AFTER
        );
        assert_eq!(
            retry_after_or_default(Some("86400"), SystemTime::UNIX_EPOCH),
            MAX_RETRY_AFTER
        );
    }

    #[tokio::test]
    async fn github_org_listing_rejects_a_self_referencing_next_link() {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("binding pagination fixture server: {error}"),
        };
        let addr = listener.local_addr().unwrap();
        let api_root = format!("http://{addr}");
        let first_page = format!("{api_root}/orgs/acme/repos?per_page={GITHUB_PAGE_SIZE}&type=all");
        let fixture_next_link = first_page.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 1024];
            let _ = socket.readable().await;
            let _ = socket.try_read(&mut buf);
            let body = "[]";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nlink: <{fixture_next_link}>; rel=\"next\"\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.writable().await.unwrap();
            socket.try_write(response.as_bytes()).unwrap();
        });

        let error = GitHubForge
            .list_org_repos(&discovery_client(), &api_root, "acme", None)
            .await
            .expect_err("a repeated next-link must fail the incomplete listing");
        server.await.unwrap();

        assert!(matches!(
            error.downcast_ref::<GitHubPaginationError>(),
            Some(GitHubPaginationError::Cycle { url }) if url == &first_page
        ));
    }

    #[tokio::test]
    async fn github_403_with_retry_after_requests_a_forge_cooldown() {
        let Some(signal) =
            rate_limit_from_fixture("HTTP/1.1 403 Forbidden", "retry-after: 120\r\n", "").await
        else {
            return;
        };

        assert_eq!(signal.retry_after(), Duration::from_secs(120));
    }

    #[tokio::test]
    async fn github_403_with_exhausted_primary_limit_requests_a_forge_cooldown() {
        let Some(signal) =
            rate_limit_from_fixture("HTTP/1.1 403 Forbidden", "x-ratelimit-remaining: 0\r\n", "")
                .await
        else {
            return;
        };

        assert_eq!(signal.retry_after(), RATE_LIMIT_COOLDOWN);
    }

    #[tokio::test]
    async fn github_403_with_secondary_limit_message_requests_a_forge_cooldown() {
        let body = r#"{"message":"You have exceeded a secondary rate limit. Please wait before retrying."}"#;
        let Some(signal) = rate_limit_from_fixture(
            "HTTP/1.1 403 Forbidden",
            "content-type: application/json\r\n",
            body,
        )
        .await
        else {
            return;
        };

        assert_eq!(signal.retry_after(), RATE_LIMIT_COOLDOWN);
    }

    #[tokio::test]
    async fn github_non_success_status_surfaces_a_typed_listing_error() {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("binding status fixture server: {error}"),
        };
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 1024];
            let _ = socket.readable().await;
            let _ = socket.try_read(&mut buf);
            let body = "not found";
            let response = format!(
                "HTTP/1.1 404 Not Found\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.writable().await.unwrap();
            socket.try_write(response.as_bytes()).unwrap();
        });

        let error = GitHubForge
            .list_org_repos(&discovery_client(), &format!("http://{addr}"), "acme", None)
            .await
            .expect_err("the fixture returns a non-success status");
        server.await.unwrap();

        assert!(matches!(
            error.downcast_ref::<GitHubListingError>(),
            Some(GitHubListingError::HttpStatus {
                status: reqwest::StatusCode::NOT_FOUND,
                org,
                response_body,
            }) if org == "acme" && response_body == "not found"
        ));
        assert_eq!(
            error.to_string(),
            "GitHub repo discovery for acme returned 404 Not Found: not found"
        );
    }

    async fn rate_limit_from_fixture(
        status_line: &str,
        headers: &str,
        body: &str,
    ) -> Option<ForgeRateLimit> {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("binding rate-limit fixture server: {error}"),
        };
        let addr = listener.local_addr().unwrap();
        let response = format!(
            "{status_line}\r\n{headers}content-length: {}\r\n\r\n{body}",
            body.len()
        );
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 1024];
            let _ = socket.readable().await;
            let _ = socket.try_read(&mut buf);
            socket.writable().await.unwrap();
            socket.try_write(response.as_bytes()).unwrap();
        });

        let error = GitHubForge
            .list_org_repos(&discovery_client(), &format!("http://{addr}"), "acme", None)
            .await
            .expect_err("the fixture returns a rate limit");
        server.await.unwrap();
        Some(
            error
                .downcast::<ForgeRateLimit>()
                .expect("rate-limited responses surface the public cooldown signal"),
        )
    }

    async fn retry_after_from_429_fixture(value: &str) -> Option<Duration> {
        rate_limit_from_fixture(
            "HTTP/1.1 429 Too Many Requests",
            &format!("retry-after: {value}\r\n"),
            "",
        )
        .await
        .map(|signal| signal.retry_after())
    }

    #[tokio::test]
    async fn github_discovery_charges_the_forge_budget_before_requesting() {
        let budget = CountingBudget(AtomicUsize::new(0));

        let result = GitHubForge
            .list_org_repos_budgeted(&discovery_client(), "not-an-api-url", "acme", None, &budget)
            .await;

        assert!(result.is_err(), "the deliberately invalid URL must fail");
        assert_eq!(budget.0.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn github_discovery_waits_for_budget_refill_without_losing_its_page() {
        let budget = RefillingBudget(AtomicUsize::new(0));

        let result = GitHubForge
            .list_org_repos_budgeted(&discovery_client(), "not-an-api-url", "acme", None, &budget)
            .await;

        assert!(result.is_err(), "the deliberately invalid URL must fail");
        assert_eq!(budget.0.load(Ordering::Relaxed), 2);
    }

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
