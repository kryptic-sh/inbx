pub fn lang_for_content_type(ct: &str) -> Option<&'static str> {
    let base = ct.split(';').next().unwrap_or(ct).trim();
    match base {
        "text/x-patch" | "text/x-diff" => Some("diff"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_maps_to_diff() {
        assert_eq!(lang_for_content_type("text/x-patch"), Some("diff"));
    }

    #[test]
    fn diff_maps_to_diff() {
        assert_eq!(lang_for_content_type("text/x-diff"), Some("diff"));
    }

    #[test]
    fn with_params_still_matches() {
        assert_eq!(
            lang_for_content_type("text/x-patch; charset=utf-8"),
            Some("diff")
        );
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(lang_for_content_type("text/plain"), None);
        assert_eq!(lang_for_content_type("application/json"), None);
    }
}
