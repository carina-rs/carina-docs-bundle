use semver::Version;

const MAX_NAME_COMPONENT_LENGTH: usize = 64;
const MAX_VERSION_LENGTH: usize = 128;

/// Coordinates of a stable release that may carry documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocsCoordinate {
    namespace: String,
    name: String,
    version: String,
}

impl DocsCoordinate {
    /// Apply the same coordinate rules as a publish request with documentation.
    pub fn new(
        namespace: &str,
        name: &str,
        version: &str,
    ) -> Result<Self, CoordinateValidationError> {
        for (field, value) in [("namespace", namespace), ("name", name)] {
            validate_name_component(value).map_err(|error| CoordinateValidationError {
                field,
                detail: error.to_string(),
            })?;
        }
        let version = validate_publish_version(version)?;
        validate_docs_version(&version)?;
        Ok(Self {
            namespace: namespace.to_string(),
            name: name.to_string(),
            version: version.to_string(),
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &str {
        &self.version
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid {field}: {detail}")]
pub struct CoordinateValidationError {
    pub field: &'static str,
    pub detail: String,
}

/// Shared namespace and provider-name rules for request and policy keys.
pub fn validate_name_component(value: &str) -> Result<(), NameComponentError> {
    if value.len() > MAX_NAME_COMPONENT_LENGTH {
        return Err(NameComponentError::TooLong);
    }

    let mut characters = value.chars();
    let first_is_valid = characters
        .next()
        .is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit());
    let rest_is_valid = characters.all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    });
    if !first_is_valid || !rest_is_valid {
        return Err(NameComponentError::InvalidCharacters);
    }

    Ok(())
}

/// The reason a namespace or provider name was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NameComponentError {
    #[error("must be at most 64 bytes")]
    TooLong,
    #[error("must contain only lowercase ASCII letters, digits, and hyphens")]
    InvalidCharacters,
}

/// Publish and yank accept semver without build metadata, up to 128 bytes.
pub fn validate_publish_version(version: &str) -> Result<Version, CoordinateValidationError> {
    if version.len() > MAX_VERSION_LENGTH {
        return Err(CoordinateValidationError {
            field: "version",
            detail: format!("must be at most {MAX_VERSION_LENGTH} bytes"),
        });
    }
    let parsed_version = Version::parse(version).map_err(|error| CoordinateValidationError {
        field: "version",
        detail: error.to_string(),
    })?;
    if !parsed_version.build.is_empty() {
        return Err(CoordinateValidationError {
            field: "version",
            detail: "build metadataは使用できません".to_string(),
        });
    }
    Ok(parsed_version)
}

/// Documentation is only accepted for stable publish versions.
pub fn validate_docs_version(version: &Version) -> Result<(), CoordinateValidationError> {
    if !version.pre.is_empty() {
        return Err(CoordinateValidationError {
            field: "docs",
            detail: "pre-release versions cannot include documentation".to_string(),
        });
    }
    Ok(())
}
