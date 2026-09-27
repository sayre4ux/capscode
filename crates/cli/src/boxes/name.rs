use anyhow::{bail, Result};

use super::{index, BoxRef, Resolved};

pub(crate) fn sanitise_base(input: &str) -> Result<String> {
    let mut base = String::new();
    let mut dash = false;
    for ch in input.chars().flat_map(char::to_lowercase) {
        let accepted = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        if accepted {
            base.push(ch);
            dash = false;
        } else if !dash && !base.is_empty() {
            base.push('-');
            dash = true;
        } else if base.is_empty() && ch == '-' {
            base.push('-');
        }
    }
    while base.ends_with('-') {
        base.pop();
    }
    if base.is_empty() {
        bail!("box name is empty after sanitising; use letters, numbers, or hyphens");
    }
    if base.starts_with('-') {
        bail!("box name cannot start with '-'");
    }
    if looks_like_identifier(&base) {
        bail!("box name looks like an id; choose a descriptive name");
    }
    Ok(base)
}

pub(crate) fn fresh_id(existing: impl IntoIterator<Item = String>) -> String {
    let taken: std::collections::HashSet<String> = existing.into_iter().collect();
    loop {
        let id = capscode_core::SessionId::new().as_str().replace('-', "")[..6].to_string();
        if !taken.contains(&id) {
            return id;
        }
    }
}

fn looks_like_identifier(base: &str) -> bool {
    let compact = base.replace('-', "");
    let uuid_shaped = base.len() == 36
        && base.chars().enumerate().all(|(index, ch)| {
            if [8, 13, 18, 23].contains(&index) {
                ch == '-'
            } else {
                ch.is_ascii_hexdigit()
            }
        });
    uuid_shaped
        || (compact.len() >= 6
            && compact.len() <= 32
            && compact.chars().all(|c| c.is_ascii_hexdigit()))
}

pub(crate) fn resolve_in(word: &str, entries: &[index::Entry]) -> Resolved {
    if let Some(entry) = entries.iter().find(|entry| entry.name == word) {
        return Resolved::One(BoxRef {
            name: entry.name.clone(),
            id: entry.id.clone(),
            session: entry.session.clone(),
        });
    }
    if let Some(entry) = entries.iter().find(|entry| entry.id == word) {
        return Resolved::One(BoxRef {
            name: entry.name.clone(),
            id: entry.id.clone(),
            session: entry.session.clone(),
        });
    }
    let matches: Vec<_> = entries
        .iter()
        .filter(|entry| entry.name.starts_with(word))
        .map(|entry| BoxRef {
            name: entry.name.clone(),
            id: entry.id.clone(),
            session: entry.session.clone(),
        })
        .collect();
    match matches.len() {
        0 => Resolved::None,
        1 => Resolved::One(matches.into_iter().next().expect("one match")),
        _ => Resolved::Ambiguous(matches),
    }
}

pub fn resolve(word: &str) -> Result<Resolved> {
    Ok(resolve_in(word, &index::read()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, id: &str, session: &str) -> index::Entry {
        index::Entry {
            name: name.into(),
            id: id.into(),
            session: session.into(),
            ..Default::default()
        }
    }

    #[test]
    fn sanitises_base_and_rejects_empty_leading_dash_and_ids() {
        assert_eq!(sanitise_base("My Project___X").unwrap(), "my-project-x");
        assert!(sanitise_base("???").is_err());
        assert!(sanitise_base("-alpha").is_err());
        assert!(sanitise_base("a1b2c3").is_err());
        assert!(sanitise_base("550e8400-e29b-41d4-a716-446655440000").is_err());
    }

    #[test]
    fn resolves_full_name_prefix_and_id_and_reports_ambiguity() {
        let entries = vec![
            entry("alpha-123abc", "123abc", "s1"),
            entry("alpine-223abc", "223abc", "s2"),
        ];
        assert!(matches!(
            resolve_in("alpha-123abc", &entries),
            Resolved::One(_)
        ));
        assert!(matches!(resolve_in("123abc", &entries), Resolved::One(_)));
        assert!(matches!(resolve_in("alpha-1", &entries), Resolved::One(_)));
        assert!(matches!(resolve_in("al", &entries), Resolved::Ambiguous(_)));
        assert_eq!(resolve_in("missing", &entries), Resolved::None);
    }

    #[test]
    fn full_name_wins_over_a_longer_name_prefix() {
        let entries = vec![
            entry("alpha-123abc", "123abc", "s1"),
            entry("alpha-123abc-extra-456def", "456def", "s2"),
        ];
        assert!(
            matches!(resolve_in("alpha-123abc", &entries), Resolved::One(item) if item.session == "s1")
        );
    }
}
