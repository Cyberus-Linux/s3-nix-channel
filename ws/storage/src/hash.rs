use std::{fmt, path::Path};

use sha2::Digest;
use tokio::io::AsyncReadExt;

/// How much of a local file we read at once while hashing it.
const READ_CHUNK_SIZE: usize = 1 << 20;

/// A SHA-256 hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256([u8; 32]);

impl Sha256 {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for Sha256 {
    fn from(bytes: [u8; 32]) -> Self {
        Sha256(bytes)
    }
}

impl From<Sha256> for Vec<u8> {
    fn from(hash: Sha256) -> Self {
        hash.0.to_vec()
    }
}

impl TryFrom<Vec<u8>> for Sha256 {
    /// The bytes, if they are not exactly 32.
    type Error = Vec<u8>;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        bytes.try_into().map(Sha256)
    }
}

/// Lowercase hex, like `sha256sum` prints it.
impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256({self})")
    }
}

/// Compute the SHA-256 hash of a local file.
///
/// The file is streamed, so this also works for large files.
pub async fn hash_file(path: &Path) -> std::io::Result<Sha256> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Hasher::default();
    let mut buffer = vec![0; READ_CHUNK_SIZE];

    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Ok(hasher.finalize());
        }

        hasher.update(&buffer[..read]);
    }
}

/// Computes a [`Sha256`] from data that arrives in chunks.
#[derive(Default)]
pub(crate) struct Hasher(sha2::Sha256);

impl Hasher {
    pub(crate) fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub(crate) fn finalize(self) -> Sha256 {
        Sha256(self.0.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_like_sha256sum() {
        let mut hasher = Hasher::default();
        hasher.update(b"hello ");
        hasher.update(b"world\n");

        // echo "hello world" | sha256sum
        assert_eq!(
            hasher.finalize().to_string(),
            "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447"
        );
    }

    #[test]
    fn converts_from_and_to_bytes() {
        let hash = Sha256::from([7; 32]);
        let bytes = Vec::from(hash);
        assert_eq!(Sha256::try_from(bytes), Ok(hash));

        assert_eq!(Sha256::try_from(vec![1, 2, 3]), Err(vec![1, 2, 3]));
    }
}
