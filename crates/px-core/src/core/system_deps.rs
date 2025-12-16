use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SYSTEM_DEPS_FINGERPRINT_VERSION: u32 = 3;

/// Normalized system dependency metadata derived from capability inference.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemDeps {
    pub capabilities: BTreeSet<String>,
    #[serde(default, alias = "apt")]
    pub apt_packages: BTreeSet<String>,
    #[serde(default)]
    pub apt_versions: BTreeMap<String, String>,
}

/// Capability matchers keyed by package names (PyPI/conda).
pub(crate) fn package_capability_rules() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        (
            "postgres",
            &[
                "psycopg2",
                "psycopg2-binary",
                "asyncpg",
                "pg8000",
                "postgresql",
                "libpq",
            ],
        ),
        (
            "mysql",
            &["mysqlclient", "mariadb", "libmysqlclient", "libmariadb"],
        ),
        (
            "imagecodecs",
            &[
                "pillow", "jpeg", "libjpeg", "libpng", "zlib", "tiff", "libtiff", "libwebp",
            ],
        ),
        ("xml", &["lxml", "libxml2", "libxslt", "xmlsec"]),
        ("ldap", &["ldap", "python-ldap", "pyldap", "libldap"]),
        ("ffi", &["cffi", "libffi"]),
        ("curl", &["curl", "libcurl", "openssl", "pycurl"]),
        (
            "gdal",
            &[
                "gdal", "osgeo", "rasterio", "fiona", "pyproj", "shapely", "geos", "proj",
            ],
        ),
    ]
}

/// Capability matchers keyed by shared library filename fragments.
pub(crate) fn library_capability_rules() -> &'static [(&'static str, &'static str)] {
    &[
        ("libpq", "postgres"),
        ("libmysqlclient", "mysql"),
        ("libmysql", "mysql"),
        ("libjpeg", "imagecodecs"),
        ("libpng", "imagecodecs"),
        ("libz", "imagecodecs"),
        ("libtiff", "imagecodecs"),
        ("libxml2", "xml"),
        ("libxslt", "xml"),
        ("libldap", "ldap"),
        ("libffi", "ffi"),
        ("libcurl", "curl"),
        ("libssl", "curl"),
        ("libgdal", "gdal"),
        ("libproj", "gdal"),
        ("libgeos", "gdal"),
    ]
}

/// Debian-style apt packages to satisfy each capability.
pub(crate) fn capability_apt_map() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("postgres", &["libpq-dev", "libpq5"]),
        ("mysql", &["libmysqlclient-dev", "libmysqlclient21"]),
        (
            "imagecodecs",
            &[
                "libjpeg62-turbo",
                "libjpeg62-turbo-dev",
                "zlib1g",
                "zlib1g-dev",
                "libpng-dev",
                "libpng16-16",
                "libtiff-dev",
                "libtiff6",
            ],
        ),
        ("xml", &["libxml2", "libxml2-dev", "libxslt1-dev"]),
        ("ldap", &["libldap-2.5-0", "libldap2-dev"]),
        ("ffi", &["libffi8", "libffi-dev"]),
        (
            "curl",
            &[
                "ca-certificates",
                "libcurl4",
                "libcurl4-openssl-dev",
                "libssl-dev",
            ],
        ),
        (
            "gdal",
            &[
                "gdal-bin",
                "libgdal-dev",
                "proj-bin",
                "libproj-dev",
                "libgeos-dev",
                "libgeos-c1v5",
            ],
        ),
    ]
}

pub(crate) fn base_apt_packages() -> &'static [&'static str] {
    &[
        "build-essential",
        "pkg-config",
        "git",
        "curl",
        "ca-certificates",
        "rustc",
        "cargo",
        "bash",
        "coreutils",
    ]
}

/// Infer capabilities from dependency or package names.
pub(crate) fn capabilities_from_names<I, S>(names: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut capabilities = BTreeSet::new();
    for name in names {
        let lower = name.as_ref().to_ascii_lowercase();
        for (capability, patterns) in package_capability_rules() {
            if patterns.iter().any(|pat| lower.starts_with(pat)) {
                capabilities.insert((*capability).to_string());
            }
        }
    }
    capabilities
}

/// Infer capabilities from shared library filenames.
pub(crate) fn capabilities_from_libraries<I, S>(names: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut capabilities = BTreeSet::new();
    for name in names {
        let lower = name.as_ref().to_ascii_lowercase();
        for (pattern, capability) in library_capability_rules() {
            if lower.contains(pattern) {
                capabilities.insert((*capability).to_string());
            }
        }
    }
    capabilities
}

/// Resolve apt packages for the provided capability set.
pub(crate) fn apt_packages_for(capabilities: &BTreeSet<String>) -> BTreeSet<String> {
    let mut pkgs = BTreeSet::new();
    for cap in capabilities {
        for (name, packages) in capability_apt_map() {
            if name == cap {
                pkgs.extend(packages.iter().map(|pkg| pkg.to_string()));
            }
        }
    }
    pkgs
}

/// Resolve full system dependencies (capabilities + apt packages) for a sandbox or builder flow.
pub(crate) fn resolve_system_deps(
    capabilities: &BTreeSet<String>,
    site_packages: Option<&Path>,
) -> SystemDeps {
    let mut deps = SystemDeps {
        capabilities: capabilities.clone(),
        ..Default::default()
    };
    deps.apt_packages.extend(apt_packages_for(capabilities));
    if let Some(site) = site_packages {
        let meta = read_sys_deps_metadata(site);
        deps.apt_packages.extend(meta.apt_packages);
        for (pkg, ver) in meta.apt_versions {
            deps.apt_versions.entry(pkg).or_insert(ver);
        }
    }
    deps
}

impl SystemDeps {
    pub(crate) fn is_empty(&self) -> bool {
        self.capabilities.is_empty() && self.apt_packages.is_empty() && self.apt_versions.is_empty()
    }

    pub(crate) fn merge(&mut self, other: &SystemDeps) {
        self.capabilities.extend(other.capabilities.iter().cloned());
        self.apt_packages.extend(other.apt_packages.iter().cloned());
        for (name, version) in &other.apt_versions {
            self.apt_versions
                .entry(name.clone())
                .or_insert_with(|| version.clone());
        }
    }

    pub(crate) fn fingerprint(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        #[derive(Serialize)]
        struct Fingerprint<'a> {
            version: u32,
            capabilities: &'a BTreeSet<String>,
            apt_packages: &'a BTreeSet<String>,
            apt_versions: &'a BTreeMap<String, String>,
        }
        let payload = Fingerprint {
            version: SYSTEM_DEPS_FINGERPRINT_VERSION,
            capabilities: &self.capabilities,
            apt_packages: &self.apt_packages,
            apt_versions: &self.apt_versions,
        };
        let bytes = serde_json::to_vec(&payload).unwrap_or_default();
        Some(hex::encode(Sha256::digest(bytes)))
    }
}

/// Generate system dependency metadata from dependency names.
pub(crate) fn system_deps_from_names<I, S>(names: I) -> SystemDeps
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let capabilities = capabilities_from_names(names);
    let mut apt_packages = apt_packages_for(&capabilities);
    apt_packages.extend(base_apt_packages().iter().map(|pkg| (*pkg).to_string()));
    SystemDeps {
        capabilities,
        apt_packages,
        apt_versions: BTreeMap::new(),
    }
}

/// Path to the metadata directory under a site-packages root.
pub(crate) fn sys_deps_dir(root: &Path) -> PathBuf {
    root.join(".px-sys-deps")
}

/// Persist system dependency metadata next to a built wheel dist.
pub(crate) fn write_sys_deps_metadata(root: &Path, package: &str, deps: &SystemDeps) -> Result<()> {
    if deps.capabilities.is_empty() && deps.apt_packages.is_empty() {
        return Ok(());
    }
    let dir = sys_deps_dir(root);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{package}.json"));
    let body = serde_json::to_vec_pretty(deps)?;
    fs::write(path, body)?;
    Ok(())
}

/// Read system dependency metadata from the given site-packages root.
pub(crate) fn read_sys_deps_metadata(root: &Path) -> SystemDeps {
    let mut combined = SystemDeps::default();
    let dir = sys_deps_dir(root);
    let mut entries = match fs::read_dir(&dir) {
        Ok(iter) => iter
            .flatten()
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            })
            .collect::<Vec<_>>(),
        Err(_) => return combined,
    };
    entries.sort_by_key(|entry| entry.file_name());
    if entries.is_empty() {
        return combined;
    }
    for entry in entries {
        let path = entry.path();
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<SystemDeps>(&contents) else {
            continue;
        };
        combined.merge(&meta);
    }
    combined
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn infers_capabilities_from_dependency_names() {
        let deps = ["libpq", "jpeg", "pycurl", "unknown"];
        let caps = capabilities_from_names(deps);
        assert!(caps.contains("postgres"));
        assert!(caps.contains("imagecodecs"));
        assert!(
            caps.contains("curl"),
            "http deps should map to curl capability"
        );
    }

    #[test]
    fn infers_capabilities_from_libraries() {
        let libs = ["libpq.so.5", "libxml2.so", "libsomething"];
        let caps = capabilities_from_libraries(libs);
        assert!(caps.contains("postgres"));
        assert!(caps.contains("xml"));
        assert!(!caps.contains("curl"));
    }

    #[test]
    fn resolves_apt_packages_for_capabilities() {
        let caps = ["postgres", "curl"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let pkgs = apt_packages_for(&caps);
        assert!(pkgs.contains("libpq-dev"));
        assert!(pkgs.contains("libcurl4"));
    }

    #[test]
    fn writes_and_reads_metadata() -> Result<()> {
        let dir = tempdir()?;
        let deps = SystemDeps {
            capabilities: ["postgres".into(), "gdal".into()].into_iter().collect(),
            apt_packages: ["libpq-dev".into(), "libgdal-dev".into()]
                .into_iter()
                .collect(),
            apt_versions: [("libpq-dev".into(), "15.0".into())].into_iter().collect(),
        };
        write_sys_deps_metadata(dir.path(), "demo", &deps)?;
        let read = read_sys_deps_metadata(dir.path());
        assert!(read.capabilities.contains("postgres"));
        assert!(read.capabilities.contains("gdal"));
        assert!(read.apt_packages.contains("libgdal-dev"));
        assert_eq!(
            read.apt_versions.get("libpq-dev"),
            Some(&"15.0".to_string())
        );
        Ok(())
    }

    #[test]
    fn conda_like_deps_map_to_apt_packages() {
        let deps = system_deps_from_names(["osgeo", "libpq", "libffi"]);
        assert!(deps.capabilities.contains("gdal"));
        assert!(deps.capabilities.contains("postgres"));
        assert!(deps.apt_packages.contains("libgdal-dev"));
        assert!(deps.apt_packages.contains("libpq-dev"));
    }

    #[test]
    fn pure_python_mysql_drivers_do_not_trigger_mysql_capability() {
        let caps = capabilities_from_names(["pymysql", "aiomysql", "mysql-connector-python"]);
        assert!(
            !caps.contains("mysql"),
            "pure-python drivers should not require mysql system deps"
        );

        let caps = capabilities_from_names(["mysqlclient"]);
        assert!(caps.contains("mysql"));
    }

    #[test]
    fn fingerprint_changes_when_versions_change() {
        let mut deps = SystemDeps::default();
        deps.capabilities.insert("postgres".into());
        deps.apt_packages.insert("libpq-dev".into());
        deps.apt_versions.insert("libpq-dev".into(), "1.0".into());
        let first = deps.fingerprint();
        deps.apt_versions.insert("libpq-dev".into(), "2.0".into());
        let second = deps.fingerprint();
        assert_ne!(
            first, second,
            "apt version change should affect fingerprint"
        );
    }
}
