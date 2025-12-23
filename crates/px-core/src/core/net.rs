use std::env;

/// Decide whether px should honor standard proxy environment variables.
///
/// Behavior:
/// - `PX_KEEP_PROXIES=1/true/yes/on` forces proxies on.
/// - `PX_KEEP_PROXIES=0/false/no/off/""` forces proxies off.
/// - If unset, proxies are enabled only when at least one proxy env var is set.
pub(crate) fn keep_proxies() -> bool {
    match env::var("PX_KEEP_PROXIES") {
        Ok(raw) => {
            let value = raw.trim().to_ascii_lowercase();
            !matches!(value.as_str(), "" | "0" | "false" | "no" | "off")
        }
        Err(_) => {
            const PROXY_KEYS: &[&str] = &[
                "HTTP_PROXY",
                "http_proxy",
                "HTTPS_PROXY",
                "https_proxy",
                "ALL_PROXY",
                "all_proxy",
                "NO_PROXY",
                "no_proxy",
            ];
            PROXY_KEYS.iter().any(|key| {
                env::var(key)
                    .ok()
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let previous = env::var(key).ok();
            match value {
                Some(value) => env::set_var(key, value),
                None => env::remove_var(key),
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => env::set_var(self.key, value),
                None => env::remove_var(self.key),
            }
        }
    }

    #[test]
    #[serial]
    fn keep_proxies_defaults_to_enabled_when_proxy_env_is_set() {
        let _keep = EnvGuard::set("PX_KEEP_PROXIES", None);
        let _http_proxy_lower = EnvGuard::set("http_proxy", None);
        let _http_proxy = EnvGuard::set("HTTP_PROXY", Some("http://proxy.example"));
        let _https_proxy_lower = EnvGuard::set("https_proxy", None);
        let _https_proxy = EnvGuard::set("HTTPS_PROXY", None);
        let _all_proxy_lower = EnvGuard::set("all_proxy", None);
        let _all_proxy = EnvGuard::set("ALL_PROXY", None);
        let _no_proxy_lower = EnvGuard::set("no_proxy", None);
        let _no_proxy = EnvGuard::set("NO_PROXY", None);
        assert!(keep_proxies());
    }

    #[test]
    #[serial]
    fn keep_proxies_defaults_to_disabled_without_proxy_env() {
        let _keep = EnvGuard::set("PX_KEEP_PROXIES", None);
        let _http_proxy_lower = EnvGuard::set("http_proxy", None);
        let _http_proxy = EnvGuard::set("HTTP_PROXY", None);
        let _https_proxy_lower = EnvGuard::set("https_proxy", None);
        let _https_proxy = EnvGuard::set("HTTPS_PROXY", None);
        let _all_proxy_lower = EnvGuard::set("all_proxy", None);
        let _all_proxy = EnvGuard::set("ALL_PROXY", None);
        let _no_proxy_lower = EnvGuard::set("no_proxy", None);
        let _no_proxy = EnvGuard::set("NO_PROXY", None);
        assert!(!keep_proxies());
    }

    #[test]
    #[serial]
    fn keep_proxies_env_var_forces_enabled() {
        let _keep = EnvGuard::set("PX_KEEP_PROXIES", Some("1"));
        let _http_proxy_lower = EnvGuard::set("http_proxy", None);
        let _http_proxy = EnvGuard::set("HTTP_PROXY", None);
        let _https_proxy_lower = EnvGuard::set("https_proxy", None);
        let _https_proxy = EnvGuard::set("HTTPS_PROXY", None);
        let _all_proxy_lower = EnvGuard::set("all_proxy", None);
        let _all_proxy = EnvGuard::set("ALL_PROXY", None);
        let _no_proxy_lower = EnvGuard::set("no_proxy", None);
        let _no_proxy = EnvGuard::set("NO_PROXY", None);
        assert!(keep_proxies());
    }

    #[test]
    #[serial]
    fn keep_proxies_env_var_forces_disabled() {
        let _keep = EnvGuard::set("PX_KEEP_PROXIES", Some("0"));
        let _http_proxy_lower = EnvGuard::set("http_proxy", None);
        let _http_proxy = EnvGuard::set("HTTP_PROXY", Some("http://proxy.example"));
        let _https_proxy_lower = EnvGuard::set("https_proxy", None);
        let _https_proxy = EnvGuard::set("HTTPS_PROXY", None);
        let _all_proxy_lower = EnvGuard::set("all_proxy", None);
        let _all_proxy = EnvGuard::set("ALL_PROXY", None);
        let _no_proxy_lower = EnvGuard::set("no_proxy", None);
        let _no_proxy = EnvGuard::set("NO_PROXY", None);
        assert!(!keep_proxies());
    }
}
