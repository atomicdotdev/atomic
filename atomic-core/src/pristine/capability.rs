//! Repository capability requirements stored in pristine metadata.

use std::fmt;

/// Prefix for repository capability requirements in `PRISTINE_META`.
pub const REQUIRED_CAPABILITY_PREFIX: &str = "required-capability/";

/// Capability required by repositories containing vNext change objects.
pub const CHANGE_FORMAT_VNEXT_CAPABILITY: RepositoryCapability =
    RepositoryCapability::new("change-format-vnext", 1);

/// CB-13B: capability a repository requires once the transactional shadow
/// migration has cut over to the colocated bridge.
///
/// The requirement lives in `PRISTINE_META` under `required-capability/`,
/// so builds that predate this capability fail closed on open instead of
/// writing through a legacy shadow pipeline the repository no longer owns.
pub const BRIDGE_CUTOVER_CAPABILITY: RepositoryCapability =
    RepositoryCapability::new("git-bridge-cutover", 1);

/// Capabilities supported by this Atomic build.
pub const SUPPORTED_REPOSITORY_CAPABILITIES: &[RepositoryCapability] =
    &[CHANGE_FORMAT_VNEXT_CAPABILITY, BRIDGE_CUTOVER_CAPABILITY];

/// A repository capability and the minimum version required to use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepositoryCapability {
    id: &'static str,
    minimum_version: u32,
}

impl RepositoryCapability {
    /// Define a repository capability supported by this build.
    pub const fn new(id: &'static str, minimum_version: u32) -> Self {
        Self {
            id,
            minimum_version,
        }
    }

    /// Stable capability identifier stored in pristine metadata.
    pub const fn id(self) -> &'static str {
        self.id
    }

    /// Minimum supported capability version.
    pub const fn minimum_version(self) -> u32 {
        self.minimum_version
    }

    pub(crate) fn metadata_key(self) -> String {
        format!("{REQUIRED_CAPABILITY_PREFIX}{}", self.id)
    }
}

/// One required capability read from repository metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredRepositoryCapability {
    /// Stable capability identifier.
    pub id: String,
    /// Minimum version required by the repository.
    pub minimum_version: u32,
}

impl RequiredRepositoryCapability {
    pub(crate) fn from_metadata(key: &str, minimum_version: u32) -> Option<Self> {
        key.strip_prefix(REQUIRED_CAPABILITY_PREFIX).map(|id| Self {
            id: id.to_string(),
            minimum_version,
        })
    }
}

/// A requirement unsupported by the current Atomic build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedRepositoryCapability {
    /// Stable capability identifier.
    pub id: String,
    /// Minimum version required by the repository.
    pub required_version: u32,
    /// Highest version supported by this build, if the capability is known.
    pub supported_version: Option<u32>,
}

impl fmt::Display for UnsupportedRepositoryCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.supported_version {
            Some(supported) => write!(
                formatter,
                "'{}' version {} (this build supports through version {})",
                self.id, self.required_version, supported
            ),
            None => write!(
                formatter,
                "'{}' version {} (not supported by this build)",
                self.id, self.required_version
            ),
        }
    }
}

pub(crate) fn unsupported_requirements(
    requirements: &[RequiredRepositoryCapability],
) -> Vec<UnsupportedRepositoryCapability> {
    let mut unsupported = requirements
        .iter()
        .filter_map(|required| {
            let supported_version = SUPPORTED_REPOSITORY_CAPABILITIES
                .iter()
                .find(|supported| supported.id == required.id)
                .map(|supported| supported.minimum_version);
            if supported_version.is_some_and(|version| version >= required.minimum_version) {
                None
            } else {
                Some(UnsupportedRepositoryCapability {
                    id: required.id.clone(),
                    required_version: required.minimum_version,
                    supported_version,
                })
            }
        })
        .collect::<Vec<_>>();
    unsupported.sort_by(|left, right| left.id.cmp(&right.id));
    unsupported
}
