use bviz_sniff::Kind;

/// The token links and manifests name `kind` by. Tokens are stable: links
/// already shared keep them.
pub fn kind_token(kind: Kind) -> &'static str {
    match kind {
        Kind::Profile => "profile",
        Kind::Bep => "bep",
        Kind::ExecLog => "execlog",
        Kind::Query => "query",
        Kind::Cquery => "cquery",
        Kind::Aquery => "aquery",
    }
}

/// The kind [`kind_token`] names `token`, if any.
pub fn kind_from_token(token: &str) -> Option<Kind> {
    Some(match token {
        "profile" => Kind::Profile,
        "bep" => Kind::Bep,
        "execlog" => Kind::ExecLog,
        "query" => Kind::Query,
        "cquery" => Kind::Cquery,
        "aquery" => Kind::Aquery,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [Kind; 6] = [
        Kind::Profile,
        Kind::Bep,
        Kind::ExecLog,
        Kind::Query,
        Kind::Cquery,
        Kind::Aquery,
    ];

    #[test]
    fn every_kind_is_read_back_from_its_token() {
        for kind in KINDS {
            assert_eq!(kind_from_token(kind_token(kind)), Some(kind));
        }
    }

    #[test]
    fn a_token_is_matched_exactly() {
        assert_eq!(kind_from_token("Profile"), None);
        assert_eq!(kind_from_token(" bep"), None);
        assert_eq!(kind_from_token(""), None);
        assert_eq!(kind_from_token("https"), None);
    }
}
