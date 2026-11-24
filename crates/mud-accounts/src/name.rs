//! The account key of a login name.

use mud_world::EntityId;

/// A login name, trimmed and lowercased: the key of its account. The
/// account is the character, so its entity is `pc_<key>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NameKey(String);

impl NameKey {
    /// The key of `name`, or `None` unless it is 2 to 16 letters, digits
    /// or underscores once trimmed.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let n = name.trim();
        let ok = (2..=16).contains(&n.chars().count())
            && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        ok.then(|| Self(n.to_ascii_lowercase()))
    }

    /// The key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The character's entity id.
    #[must_use]
    pub fn entity(&self) -> EntityId {
        EntityId::new(format!("{}{}", mud_world::state::PLAYER_ID_PREFIX, self.0))
    }

    /// The key of an entity id `pc_<key>`, if it is one.
    #[must_use]
    pub fn of_entity(id: &EntityId) -> Option<Self> {
        id.as_str()
            .strip_prefix(mud_world::state::PLAYER_ID_PREFIX)
            .and_then(Self::parse)
    }
}

impl std::fmt::Display for NameKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_trimmed_lowercased_and_checked() {
        let k = NameKey::parse("  Ada_9 ").unwrap();
        assert_eq!(k.as_str(), "ada_9");
        assert_eq!(k.to_string(), "ada_9");
        assert_eq!(k.entity(), EntityId::from("pc_ada_9"));
        assert_eq!(NameKey::of_entity(&k.entity()), Some(k));
        assert_eq!(NameKey::of_entity(&EntityId::from("torin")), None);
        for bad in ["a", "bad name", "x".repeat(17).as_str(), "é1"] {
            assert_eq!(NameKey::parse(bad), None, "{bad}");
        }
    }
}
