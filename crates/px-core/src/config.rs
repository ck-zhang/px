use std::collections::HashMap;
use std::env;

use serde::{Deserialize, Serialize};

use crate::effects;
use crate::store::CacheLocation;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalOptions {
    pub quiet: bool,
    pub verbose: u8,
    pub trace: bool,
    pub json: bool,
    pub config: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct EnvSnapshot {
    vars: HashMap<String, String>,
}

impl EnvSnapshot {
    pub(crate) fn capture() -> Self {
        Self {
            vars: env::vars().collect(),
        }
    }

    pub(crate) fn flag_is_enabled(&self, key: &str) -> bool {
        matches!(self.vars.get(key).map(String::as_str), Some("1"))
    }

    pub(crate) fn var(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    pub(crate) fn contains(&self, key: &str) -> bool {
        self.vars.contains_key(key)
    }

    #[cfg(test)]
    pub(crate) fn testing(pairs: &[(&str, &str)]) -> Self {
        let vars = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Self { vars }
    }
}

#[derive(Debug)]
pub struct Config {
    pub(crate) cache: CacheConfig,
    pub(crate) network: NetworkConfig,
    pub(crate) resolver: ResolverConfig,
    pub(crate) test: TestConfig,
    pub(crate) publish: PublishConfig,
}

impl Config {
    /// Builds a configuration snapshot from the current process environment.
    ///
    /// # Errors
    /// Returns an error if cache paths cannot be resolved or inspected.
    pub fn from_env(effects: &dyn effects::Effects) -> anyhow::Result<Self> {
        let snapshot = EnvSnapshot::capture();
        Self::from_snapshot(&snapshot, effects.cache())
    }

    pub(crate) fn from_snapshot(
        snapshot: &EnvSnapshot,
        cache_store: &dyn effects::CacheStore,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            cache: CacheConfig {
                store: cache_store.resolve_store_path()?,
            },
            network: NetworkConfig {
                online: match snapshot.var("PX_ONLINE") {
                    Some(value) => {
                        let lowered = value.to_ascii_lowercase();
                        lowered != "0" && lowered != "false"
                    }
                    None => true,
                },
            },
            resolver: ResolverConfig {
                enabled: match snapshot.var("PX_RESOLVER") {
                    Some(value) => value == "1",
                    None => true,
                },
                force_sdist: snapshot.flag_is_enabled("PX_FORCE_SDIST"),
            },
            test: TestConfig {
                fallback_builtin: snapshot.flag_is_enabled("PX_TEST_FALLBACK_STD"),
                skip_tests_flag: snapshot.var("PX_SKIP_TESTS").map(ToOwned::to_owned),
            },
            publish: PublishConfig {
                default_token_env: "PX_PUBLISH_TOKEN",
            },
        })
    }

    #[must_use]
    pub fn cache(&self) -> &CacheConfig {
        &self.cache
    }

    #[must_use]
    pub fn network(&self) -> &NetworkConfig {
        &self.network
    }

    #[must_use]
    pub fn resolver(&self) -> &ResolverConfig {
        &self.resolver
    }

    #[must_use]
    pub fn test(&self) -> &TestConfig {
        &self.test
    }

    #[must_use]
    pub fn publish(&self) -> &PublishConfig {
        &self.publish
    }
}

#[derive(Debug)]
pub struct CacheConfig {
    pub store: CacheLocation,
}

#[derive(Debug, Clone, Copy)]
pub struct NetworkConfig {
    pub online: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ResolverConfig {
    pub enabled: bool,
    pub force_sdist: bool,
}

#[derive(Debug, Clone)]
pub struct TestConfig {
    pub fallback_builtin: bool,
    pub skip_tests_flag: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct PublishConfig {
    pub default_token_env: &'static str,
}
