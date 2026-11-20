use std::path::PathBuf;

/// The underlying error of a failed bucket operation.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Why an object couldn't be fetched from the bucket to serve it to a client.
#[derive(thiserror::Error, Debug)]
pub enum FetchError {
    #[error("There is no such object: {object_key:?}")]
    NoSuchObject { object_key: String },
    #[error("Failed to fetch object {object_key:?}")]
    BackendFailure { object_key: String },
}

/// Why reading, checking or uploading objects failed.
#[derive(thiserror::Error, Debug)]
pub enum StorageError {
    #[error("Invalid S3 configuration")]
    Configuration { source: BoxError },
    #[error("Failed to read {object_key:?} from the bucket")]
    Read {
        object_key: String,
        source: BoxError,
    },
    #[error("Failed to check whether {object_key:?} exists in the bucket")]
    ExistenceCheck {
        object_key: String,
        source: BoxError,
    },
    #[error("Failed to upload {object_key:?} to the bucket")]
    Upload {
        object_key: String,
        source: BoxError,
    },
    #[error("Refusing to overwrite {object_key:?} in the bucket")]
    AlreadyExists { object_key: String },
    #[error("Failed to read local file {}", path.display())]
    LocalFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} needs a file name that is valid UTF-8", path.display())]
    InvalidFileName { path: PathBuf },
}
