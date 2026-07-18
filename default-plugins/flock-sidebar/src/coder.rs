//! Exact recognition of Coder-bound sessions and panes. The selector writes
//! `coder ssh owner/name` as the session default command; the sidebar treats
//! only that exact shape as remote.

pub fn parse_coder_ssh(argv: &[String]) -> Option<&str> {
    match argv {
        [coder, ssh, identifier]
            if coder == "coder" && ssh == "ssh" && valid_identifier(identifier) =>
        {
            Some(identifier)
        },
        _ => None,
    }
}

fn valid_identifier(identifier: &str) -> bool {
    let mut parts = identifier.split('/');
    parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    #[test]
    fn recognizes_exact_owner_workspace_binding() {
        assert_eq!(
            parse_coder_ssh(&argv(&["coder", "ssh", "alice/api"])),
            Some("alice/api")
        );
    }

    #[test]
    fn rejects_ambiguous_or_non_binding_commands() {
        assert_eq!(parse_coder_ssh(&argv(&["coder", "ssh", "api"])), None);
        assert_eq!(
            parse_coder_ssh(&argv(&["coder", "ssh", "alice/api", "ls"])),
            None
        );
        assert_eq!(parse_coder_ssh(&argv(&["coder", "list"])), None);
    }
}
