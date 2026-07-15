use super::SearchHit;

pub(super) fn clamped_page_limit(offset: usize, limit: usize) -> usize {
    limit.min(super::MAX_SEARCH_WINDOW.saturating_sub(offset))
}

/// Merge per-repo hits into a deterministic ranking and select one page.
pub(super) fn merge_paginate(
    all: Vec<SearchHit>,
    offset: usize,
    limit: usize,
) -> (Vec<SearchHit>, bool) {
    let mut keyed: Vec<_> = all
        .into_iter()
        .map(|hit| (hit.id.external(), hit))
        .collect();
    keyed.sort_by(|(a_id, a), (b_id, b)| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.repo.as_str().cmp(b.repo.as_str()))
            .then_with(|| a_id.cmp(b_id))
    });
    let has_more = keyed.len() > offset + limit;
    let page = keyed
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(_, hit)| hit)
        .collect();
    (page, has_more)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VerbId;
    use crate::search::RepoQualifier;

    fn test_hit(repo: &str, id: &str, score: f32) -> SearchHit {
        SearchHit {
            id: VerbId::parse(id).expect("test id parses"),
            kind: yg_shard::NodeKind::Symbol,
            name: None,
            path: None,
            repo: RepoQualifier::new(repo.to_string()),
            score,
            snippet: None,
        }
    }

    fn page_ids(page: Vec<SearchHit>) -> Vec<String> {
        page.into_iter().map(|hit| hit.id.external()).collect()
    }

    #[test]
    fn orders_across_repos_and_pages_by_offset() {
        let corpus = || {
            vec![
                test_hit("a", "sym:a:x#1", 1.0),
                test_hit("b", "sym:b:y#1", 3.0),
                test_hit("a", "sym:a:x#2", 2.0),
                test_hit("b", "sym:b:y#2", 2.0),
            ]
        };
        let (first, more) = merge_paginate(corpus(), 0, 2);
        assert_eq!(page_ids(first), ["sym:b:y#1", "sym:a:x#2"]);
        assert!(more);
        let (second, more) = merge_paginate(corpus(), 2, 2);
        assert_eq!(page_ids(second), ["sym:b:y#2", "sym:a:x#1"]);
        assert!(!more);
    }

    #[test]
    fn breaks_score_and_repo_ties_by_id_and_bounds_pages() {
        let corpus = || {
            vec![
                test_hit("a", "sym:a:c", 2.0),
                test_hit("a", "sym:a:a", 2.0),
                test_hit("a", "sym:a:b", 2.0),
            ]
        };
        let (page, more) = merge_paginate(corpus(), 0, 3);
        assert_eq!(page_ids(page), ["sym:a:a", "sym:a:b", "sym:a:c"]);
        assert!(!more);
        let (page, more) = merge_paginate(corpus(), 0, 2);
        assert_eq!(page_ids(page), ["sym:a:a", "sym:a:b"]);
        assert!(more);
        let (page, more) = merge_paginate(corpus(), 5, 2);
        assert!(page.is_empty());
        assert!(!more);
    }

    #[test]
    fn preserves_external_id_order_across_node_kinds() {
        let corpus = vec![
            test_hit("a", "sym:a:z", 2.0),
            test_hit("a", "repo:a", 2.0),
            test_hit("a", "pkg:a:z", 2.0),
        ];
        let (page, more) = merge_paginate(corpus, 0, 3);
        assert_eq!(page_ids(page), ["pkg:a:z", "repo:a", "sym:a:z"]);
        assert!(!more);
    }

    #[test]
    fn page_limit_is_clamped_to_the_window() {
        assert_eq!(clamped_page_limit(0, 20), 20);
        assert_eq!(
            clamped_page_limit(super::super::MAX_SEARCH_WINDOW - 40, 30),
            30
        );
        let near = super::super::MAX_SEARCH_WINDOW - 10;
        let clamped = clamped_page_limit(near, 30);
        assert_eq!(clamped, 10);
        assert_eq!(near + clamped, super::super::MAX_SEARCH_WINDOW);
        assert_eq!(clamped_page_limit(super::super::MAX_SEARCH_WINDOW, 30), 0);
    }
}
