use serde::{Deserialize, Serialize};

/// Vector space identity: equal stamps produce comparable vectors.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stamp {
    pub provider: String,
    pub model: String,
    pub dimensions: usize,
    /// Preprocessing descriptor: projection, piece budget, token counter, and passage prefix.
    pub preprocessing: String,
}

impl Stamp {
    pub fn key(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Short SHA-256 prefix identifying this space's dense index file.
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(self.key().as_bytes()))[..16].to_owned()
    }
}
