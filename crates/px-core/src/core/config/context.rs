use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Result;
use pep508_rs::MarkerEnvironment;
use px_domain::current_project_root;

use crate::config::{Config, EnvSnapshot, GlobalOptions};
use crate::effects::{self, Effects, SharedEffects};
use crate::python_sys::detect_marker_environment;
use crate::store::CacheLocation;
use crate::CommandGroup;
use crate::ExecutionOutcome;

#[derive(Clone, Copy, Debug)]
pub struct CommandInfo {
    pub group: CommandGroup,
    pub name: &'static str,
}

impl CommandInfo {
    #[must_use]
    pub const fn new(group: CommandGroup, name: &'static str) -> Self {
        Self { group, name }
    }
}

pub trait CommandHandler<R> {
    /// Executes a command handler within the provided context.
    ///
    /// # Errors
    /// Returns an error if command execution fails unexpectedly.
    fn handle(&self, ctx: &CommandContext, request: R) -> Result<ExecutionOutcome>;
}

pub struct CommandContext<'a> {
    pub global: &'a GlobalOptions,
    env: EnvSnapshot,
    config: Config,
    project_root: OnceLock<PathBuf>,
    effects: SharedEffects,
}

impl<'a> CommandContext<'a> {
    /// Creates a new command context with the provided global options.
    ///
    /// # Errors
    /// Returns an error if the environment snapshot or configuration cannot be prepared.
    pub fn new(global: &'a GlobalOptions, effects: SharedEffects) -> Result<Self> {
        let env = EnvSnapshot::capture();
        let config = Config::from_snapshot(&env, effects.cache())?;
        Ok(Self {
            global,
            env,
            config,
            project_root: OnceLock::new(),
            effects,
        })
    }

    pub fn effects(&self) -> &dyn Effects {
        self.effects.as_ref()
    }

    pub fn shared_effects(&self) -> SharedEffects {
        self.effects.clone()
    }

    pub fn cache(&self) -> &CacheLocation {
        &self.config.cache().store
    }

    pub fn is_online(&self) -> bool {
        self.config.network().online
    }

    pub fn fs(&self) -> &dyn effects::FileSystem {
        self.effects.fs()
    }

    pub fn python_runtime(&self) -> &dyn effects::PythonRuntime {
        self.effects.python()
    }

    pub fn git(&self) -> &dyn effects::GitClient {
        self.effects.git()
    }

    pub fn cache_store(&self) -> &dyn effects::CacheStore {
        self.effects.cache()
    }

    pub fn pypi(&self) -> &dyn effects::PypiClient {
        self.effects.pypi()
    }

    /// Detects the marker environment for PEP 508 resolution.
    ///
    /// # Errors
    /// Returns an error if interpreter detection fails.
    pub fn marker_environment(&self) -> Result<MarkerEnvironment> {
        let python = self.python_runtime().detect_interpreter()?;
        let resolver_env = detect_marker_environment(&python)?;
        resolver_env.to_marker_environment()
    }

    /// Resolves the current project's root directory.
    ///
    /// # Errors
    /// Returns an error if the working directory cannot be inspected.
    pub fn project_root(&self) -> Result<PathBuf> {
        if let Some(path) = self.project_root.get() {
            Ok(path.clone())
        } else {
            let path = current_project_root()?;
            let _ = self.project_root.set(path.clone());
            Ok(path)
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn env_contains(&self, key: &str) -> bool {
        self.env.contains(key)
    }

    pub fn env_flag_enabled(&self, key: &str) -> bool {
        self.env.flag_is_enabled(key)
    }
}
