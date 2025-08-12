pub struct UserAgent(String);

impl UserAgent {
    pub fn new(value: String) -> Self {
        Self(value) // @TODO validate
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl Default for UserAgent {
    fn default() -> Self {
        UserAgent(crate::client_name_and_version().into()) // @TODO move the version implementation
    }
}

impl fmt::Display for UserAgent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

use std::fmt;
