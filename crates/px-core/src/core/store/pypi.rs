use serde::Deserialize;

#[derive(Deserialize)]
pub struct PypiReleaseResponse {
    pub urls: Vec<PypiFile>,
}

#[derive(Clone, Deserialize)]
pub struct PypiFile {
    pub filename: String,
    pub url: String,
    pub packagetype: String,
    pub yanked: Option<bool>,
    pub digests: PypiDigests,
}

#[derive(Clone, Deserialize)]
pub struct PypiDigests {
    pub sha256: String,
}
