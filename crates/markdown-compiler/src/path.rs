use crate::LogicalTreePathError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PortableLogicalPath(pub(crate) String);

impl PortableLogicalPath {
    pub(crate) fn parse(value: &str, max_bytes: usize) -> Result<Self, LogicalTreePathError> {
        if value.is_empty() {
            return Err(LogicalTreePathError::Empty);
        }
        if value.len() > max_bytes {
            return Err(LogicalTreePathError::TooLong);
        }
        if value.starts_with('/') || value.starts_with('\\') {
            return Err(LogicalTreePathError::Absolute);
        }
        if value.contains(['%', '\\']) {
            return Err(LogicalTreePathError::EncodedTraversal);
        }
        if !value.split('/').all(is_portable_component) {
            return Err(LogicalTreePathError::UnsupportedComponent);
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn case_collision_key(&self) -> String {
        self.0.to_ascii_lowercase()
    }
}

fn is_portable_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
