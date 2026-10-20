//! Streaming-provider vocabulary for Flux: descriptors and capabilities.
//!
//! This milestone defines only small data types that *describe* a
//! provider. There are no provider traits, no networking, no
//! authentication, and no integrations here.
//!
//! A descriptor is descriptive metadata, not a legal claim: listing the
//! [`Capability::Streaming`] capability for some future provider would say
//! what an integration intends to do, and must never be read as asserting
//! a legal right to stream any particular catalog. Each real integration
//! remains subject to its own licensing and terms review in its own
//! milestone.

/// Something a provider integration may be able to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Search the provider's catalog.
    Search,
    /// Stream audio from the provider.
    Streaming,
    /// Provide song lyrics.
    Lyrics,
    /// Provide track/artist/album metadata.
    Metadata,
}

/// Static description of a (future) provider integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDescriptor {
    id: String,
    display_name: String,
    capabilities: Vec<Capability>,
}

impl ProviderDescriptor {
    /// Creates a descriptor from a stable machine-readable `id` (for
    /// example `"example-provider"`), a human-readable `display_name`,
    /// and the provider's capabilities.
    ///
    /// Duplicate capabilities are stored once, in first-seen order.
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        capabilities: impl IntoIterator<Item = Capability>,
    ) -> Self {
        let mut deduped: Vec<Capability> = Vec::new();
        for capability in capabilities {
            if !deduped.contains(&capability) {
                deduped.push(capability);
            }
        }
        Self {
            id: id.into(),
            display_name: display_name.into(),
            capabilities: deduped,
        }
    }

    /// The provider's stable machine-readable identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The provider's human-readable name.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// The capabilities declared for this provider, in declaration order.
    #[must_use]
    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    /// Whether the given capability is declared for this provider.
    #[must_use]
    pub fn has_capability(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{Capability, ProviderDescriptor};

    #[test]
    fn descriptor_exposes_id_name_and_capabilities() {
        let descriptor = ProviderDescriptor::new(
            "example",
            "Example Provider",
            [Capability::Search, Capability::Metadata],
        );
        assert_eq!(descriptor.id(), "example");
        assert_eq!(descriptor.display_name(), "Example Provider");
        assert_eq!(
            descriptor.capabilities(),
            &[Capability::Search, Capability::Metadata]
        );
        assert!(descriptor.has_capability(Capability::Search));
        assert!(!descriptor.has_capability(Capability::Streaming));
        assert!(!descriptor.has_capability(Capability::Lyrics));
    }

    #[test]
    fn duplicate_capabilities_are_deduped_in_first_seen_order() {
        let descriptor = ProviderDescriptor::new(
            "example",
            "Example",
            [
                Capability::Streaming,
                Capability::Search,
                Capability::Streaming,
                Capability::Lyrics,
            ],
        );
        assert_eq!(
            descriptor.capabilities(),
            &[
                Capability::Streaming,
                Capability::Search,
                Capability::Lyrics
            ]
        );
    }

    #[test]
    fn descriptor_may_declare_no_capabilities() {
        let descriptor = ProviderDescriptor::new("empty", "Empty", []);
        assert!(descriptor.capabilities().is_empty());
        assert!(!descriptor.has_capability(Capability::Metadata));
    }

    #[test]
    fn capability_values_are_copyable_hashable_and_distinct() {
        let all = [
            Capability::Search,
            Capability::Streaming,
            Capability::Lyrics,
            Capability::Metadata,
        ];
        let set: HashSet<Capability> = all.into_iter().collect();
        assert_eq!(set.len(), 4);
        let copied = Capability::Search;
        let also_copied = copied;
        assert_eq!(copied, also_copied);
    }
}
