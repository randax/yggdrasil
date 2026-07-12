//! The Forge seam: what the sync loops need from a code host.

pub(crate) mod github;

/// One repository as a Forge's discovery listing returns it.
#[derive(Debug)]
pub(crate) struct ListedRepo {
    pub(crate) slug: String,
    pub(crate) visibility: yg_control::RepoVisibility,
}

/// Whether a git failure is the forge pushing back on request volume — a
/// 429, a secondary-rate-limit notice, or an abuse-detection trip —
/// rather than an ordinary error (missing repo, auth, DNS). Matched on
/// the message because git surfaces the forge's HTTP status as prose;
/// judged case-insensitively across the phrasings forges use.
///
/// The needles are deliberately multi-word or punctuated phrases, never
/// bare `429`/`abuse`: the message this is fed includes the clone URL
/// (the `polling {clone_url} …` context plus git's own output), so a repo
/// slug like `acme/abuse-tracker` or `org/sloc-429` must not be mistaken
/// for the forge rate-limiting us — that would cool the whole forge down.
/// URL path segments can't contain spaces, so spaced phrases are safe.
pub(crate) fn is_rate_limit_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "too many requests",
        "rate limit",
        "abuse detection",
        "error: 429",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_errors_are_recognized_across_a_forges_phrasings() {
        for message in [
            "fatal: unable to access: The requested URL returned error: 429",
            "You have exceeded a secondary rate limit",
            "remote: Too Many Requests",
            "error: RPC failed; abuse detection mechanism triggered",
        ] {
            assert!(is_rate_limit_error(message), "must flag: {message:?}");
        }
        for message in [
            "fatal: repository not found",
            "fatal: could not read Username",
            "error: unable to resolve host",
            // The message includes the clone URL (the `polling {url}`
            // context + git's output), so an ordinary failure on a repo
            // whose slug merely contains "abuse" or "429" must NOT be
            // mistaken for the forge rate-limiting us.
            "polling https://github.com/acme/abuse-tracker for its head: \
             fatal: unable to access: The requested URL returned error: 404",
            "polling https://github.com/org/sloc-429-counter for its head: \
             fatal: repository not found",
        ] {
            assert!(
                !is_rate_limit_error(message),
                "must not flag an ordinary failure: {message:?}"
            );
        }
    }
}
