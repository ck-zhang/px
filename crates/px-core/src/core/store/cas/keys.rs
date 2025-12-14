use super::*;

/// Deterministic key for a source download request.
#[must_use]
pub fn source_lookup_key(header: &SourceHeader) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        header.name.to_ascii_lowercase(),
        header.version,
        header.filename,
        header.index_url,
        header.sha256
    )
}

/// Deterministic key for a pkg-build.
#[must_use]
pub fn pkg_build_lookup_key(header: &PkgBuildHeader) -> String {
    format!(
        "{}|{}|{}|{}",
        header.source_oid, header.runtime_abi, header.builder_id, header.build_options_hash
    )
}
