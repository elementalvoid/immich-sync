//! SHA-1 hashing and the checksum encodings Immich accepts (R3.1).

use std::fmt;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use base64::Engine as _;
use sha1::{Digest as _, Sha1 as DigestSha1};

/// The size of a SHA-1 digest in bytes.
const DIGEST_LEN: usize = 20;

/// A SHA-1 digest, kept as its 20 raw bytes.
///
/// Immich accepts the checksum base64- or hex-encoded (R3.1). The raw bytes are
/// the one unambiguous form, so the store keeps those and the wire encodings
/// are produced at the request. `Debug` prints hex, so a test failure is
/// readable and a log line is comparable with the server's own hex checksums.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha1([u8; DIGEST_LEN]);

impl Sha1 {
    /// Wraps a digest that was computed elsewhere.
    #[must_use]
    pub fn from_bytes(bytes: [u8; DIGEST_LEN]) -> Self {
        Self(bytes)
    }

    /// Reads a digest from a byte slice, for a value read back from the store.
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        let bytes: [u8; DIGEST_LEN] = bytes.try_into().ok()?;
        Some(Self(bytes))
    }

    /// Parses a 40-character hex digest. Returns `None` for anything else.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        if text.len() != DIGEST_LEN * 2 {
            return None;
        }
        let mut bytes = [0u8; DIGEST_LEN];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(Self(bytes))
    }

    /// The raw digest bytes, which is what the store keeps.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; DIGEST_LEN] {
        &self.0
    }

    /// The lowercase hex encoding, as `@immich/cli` sends it.
    #[must_use]
    pub fn hex(&self) -> String {
        use fmt::Write as _;
        let mut text = String::with_capacity(DIGEST_LEN * 2);
        for byte in self.0 {
            // Writing into a String cannot fail.
            let _ = write!(text, "{byte:02x}");
        }
        text
    }

    /// The standard base64 encoding, as immich-go sends it.
    #[must_use]
    pub fn base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.0)
    }
}

impl fmt::Debug for Sha1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Sha1({})", self.hex())
    }
}

/// Why hashing a local file failed.
#[derive(Debug, thiserror::Error)]
pub enum HashError {
    /// The file could not be opened or read (R6.4 `local_read_error`).
    #[error("the file {path} could not be read: {source}")]
    Read {
        /// The file that failed.
        path: String,
        /// The operating system's description of the failure.
        #[source]
        source: std::io::Error,
    },
}

/// The SHA-1 digest of `bytes`.
#[must_use]
pub fn sha1(bytes: &[u8]) -> Sha1 {
    let mut hasher = DigestSha1::new();
    hasher.update(bytes);
    Sha1(hasher.finalize().into())
}

/// The SHA-1 digest of a file's contents, read in one streaming pass.
///
/// The file is never buffered whole, so hashing a large file has no memory
/// cost (R3.1).
///
/// # Errors
///
/// Returns [`HashError::Read`] when the file cannot be opened or read.
pub fn sha1_file(path: &Path) -> Result<Sha1, HashError> {
    let mut file = File::open(path).map_err(|source| HashError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let mut hasher = DigestSha1::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|source| HashError::Read {
            path: path.display().to_string(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Sha1(hasher.finalize().into()))
}
