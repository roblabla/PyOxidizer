// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use {
    crate::py_packaging::distribution::DistributionCache,
    anyhow::{Context, Result},
    path_dedot::ParseDot,
    slog::warn,
    starlark::{
        environment::{Environment, EnvironmentError, TypeValues},
        values::{
            error::{RuntimeError, ValueError},
            none::NoneType,
            {Mutable, TypedValue, Value, ValueResult},
        },
        {
            starlark_fun, starlark_module, starlark_parse_param_type, starlark_signature,
            starlark_signature_extraction, starlark_signatures,
        },
    },
    starlark_dialect_build_targets::{
        build_targets_module, BuildContext, EnvironmentContext, GetStateError,
    },
    std::{
        path::{Path, PathBuf},
        sync::Arc,
    },
};

/// Holds state for evaluating a Starlark config file.
#[derive(Debug)]
pub struct PyOxidizerEnvironmentContext {
    logger: slog::Logger,

    /// Whether executing in verbose mode.
    pub verbose: bool,

    /// Directory the environment should be evaluated from.
    ///
    /// Typically used to resolve filenames.
    pub cwd: PathBuf,

    /// Path to the configuration file.
    pub config_path: PathBuf,

    /// Host triple we are building from.
    pub build_host_triple: String,

    /// Target triple we are building for.
    pub build_target_triple: String,

    /// Whether we are building a debug or release binary.
    pub build_release: bool,

    /// Optimization level when building binaries.
    pub build_opt_level: String,

    /// Base directory to use for build state.
    pub build_path: PathBuf,

    /// Path where Python distributions are written.
    pub python_distributions_path: PathBuf,

    /// Cache of ready-to-clone Python distribution objects.
    ///
    /// This exists because constructing a new instance can take a
    /// few seconds in debug builds. And this adds up, especially in tests!
    pub distribution_cache: Arc<DistributionCache>,
}

impl PyOxidizerEnvironmentContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        logger: &slog::Logger,
        verbose: bool,
        config_path: &Path,
        build_host_triple: &str,
        build_target_triple: &str,
        build_release: bool,
        build_opt_level: &str,
        distribution_cache: Option<Arc<DistributionCache>>,
    ) -> Result<PyOxidizerEnvironmentContext> {
        let parent = config_path
            .parent()
            .with_context(|| "resolving parent directory of config".to_string())?;

        let parent = if parent.is_relative() {
            std::env::current_dir()?.join(parent)
        } else {
            parent.to_path_buf()
        };

        let build_path = parent.join("build");

        let python_distributions_path = build_path.join("python_distributions");
        let distribution_cache = distribution_cache
            .unwrap_or_else(|| Arc::new(DistributionCache::new(Some(&python_distributions_path))));

        Ok(PyOxidizerEnvironmentContext {
            logger: logger.clone(),
            verbose,
            cwd: parent,
            config_path: config_path.to_path_buf(),
            build_host_triple: build_host_triple.to_string(),
            build_target_triple: build_target_triple.to_string(),
            build_release,
            build_opt_level: build_opt_level.to_string(),
            build_path: build_path.clone(),
            python_distributions_path: python_distributions_path.clone(),
            distribution_cache,
        })
    }

    pub fn logger(&self) -> &slog::Logger {
        &self.logger
    }

    pub fn set_build_path(&mut self, path: &Path) -> Result<()> {
        let path = if path.is_relative() {
            self.cwd.join(path)
        } else {
            path.to_path_buf()
        }
        .parse_dot()?
        .to_path_buf();

        self.build_path = path.clone();
        self.python_distributions_path = path.join("python_distributions");

        Ok(())
    }
}

impl TypedValue for PyOxidizerEnvironmentContext {
    type Holder = Mutable<PyOxidizerEnvironmentContext>;
    const TYPE: &'static str = "EnvironmentContext";

    fn values_for_descendant_check_and_freeze(&self) -> Box<dyn Iterator<Item = Value>> {
        Box::new(std::iter::empty())
    }
}

/// Starlark type holding context for PyOxidizer.
pub struct PyOxidizerContext {}

impl Default for PyOxidizerContext {
    fn default() -> Self {
        PyOxidizerContext {}
    }
}

impl TypedValue for PyOxidizerContext {
    type Holder = Mutable<PyOxidizerContext>;
    const TYPE: &'static str = "PyOxidizer";

    fn values_for_descendant_check_and_freeze(&self) -> Box<dyn Iterator<Item = Value>> {
        Box::new(std::iter::empty())
    }
}

/// Holds the build context for PyOxidizer's Starlark types.
pub struct PyOxidizerBuildContext {
    /// Logger where messages can be written.
    pub logger: slog::Logger,

    /// Rust target triple for build host.
    pub host_triple: String,

    /// Rust target triple for build target.
    pub target_triple: String,

    /// Whether we are building in release mode.
    ///
    /// Debug if false.
    pub release: bool,

    /// Optimization level for Rust compiler.
    pub opt_level: String,

    /// Where generated files should be written.
    pub output_path: PathBuf,
}

impl BuildContext for PyOxidizerBuildContext {
    fn logger(&self) -> &slog::Logger {
        &self.logger
    }

    fn get_state_string(&self, key: &str) -> Result<&str, GetStateError> {
        match key {
            "host_triple" => Ok(&self.host_triple),
            "target_triple" => Ok(&self.target_triple),
            "opt_level" => Ok(&self.opt_level),
            _ => Err(GetStateError::InvalidKey(key.to_string())),
        }
    }

    fn get_state_bool(&self, key: &str) -> Result<bool, GetStateError> {
        match key {
            "release" => Ok(self.release),
            _ => Err(GetStateError::InvalidKey(key.to_string())),
        }
    }

    fn get_state_path(&self, key: &str) -> Result<&Path, GetStateError> {
        match key {
            "output_path" => Ok(&self.output_path),
            _ => Err(GetStateError::InvalidKey(key.to_string())),
        }
    }
}

/// Obtain the PyOxidizerContext for the Starlark execution environment.
pub fn get_context(type_values: &TypeValues) -> ValueResult {
    type_values
        .get_type_value(&Value::new(PyOxidizerContext::default()), "CONTEXT")
        .ok_or_else(|| {
            ValueError::from(RuntimeError {
                code: "PYOXIDIZER",
                message: "Unable to resolve context (this should never happen)".to_string(),
                label: "".to_string(),
            })
        })
}

/// print(*args)
fn starlark_print(type_values: &TypeValues, args: &Vec<Value>) -> ValueResult {
    let raw_context = get_context(type_values)?;
    let context = raw_context
        .downcast_ref::<PyOxidizerEnvironmentContext>()
        .ok_or(ValueError::IncorrectParameterType)?;

    let mut parts = Vec::new();
    let mut first = true;
    for arg in args {
        if !first {
            parts.push(" ".to_string());
        }
        first = false;
        parts.push(arg.to_string());
    }

    warn!(context.logger(), "{}", parts.join(""));

    Ok(Value::new(NoneType::None))
}

/// set_build_path(path)
fn starlark_set_build_path(type_values: &TypeValues, path: String) -> ValueResult {
    let raw_context = get_context(type_values)?;
    let mut context = raw_context
        .downcast_mut::<PyOxidizerEnvironmentContext>()?
        .ok_or(ValueError::IncorrectParameterType)?;

    context.set_build_path(&PathBuf::from(&path)).map_err(|e| {
        ValueError::from(RuntimeError {
            code: "PYOXIDIZER_BUILD",
            message: e.to_string(),
            label: "set_build_path()".to_string(),
        })
    })?;

    Ok(Value::new(NoneType::None))
}

starlark_module! { global_module =>
    print(env env, *args) {
        starlark_print(&env, &args)
    }

    #[allow(clippy::ptr_arg)]
    set_build_path(env env, path: String) {
        starlark_set_build_path(&env, path)
    }
}

/// Obtain a Starlark environment for evaluating PyOxidizer configurations.
pub fn global_environment(
    context: PyOxidizerEnvironmentContext,
    resolve_targets: Option<Vec<String>>,
    build_script_mode: bool,
) -> Result<(Environment, TypeValues), EnvironmentError> {
    let mut build_targets_context = EnvironmentContext::new(context.logger());

    if let Some(targets) = resolve_targets {
        build_targets_context.set_resolve_targets(targets);
    }

    build_targets_context.build_script_mode = build_script_mode;

    let (mut env, mut type_values) = starlark::stdlib::global_environment();

    starlark_dialect_build_targets::populate_environment(
        &mut env,
        &mut type_values,
        build_targets_context,
    )?;

    build_targets_module(&mut env, &mut type_values);
    global_module(&mut env, &mut type_values);
    super::file_resource::file_resource_env(&mut env, &mut type_values);
    super::python_distribution::python_distribution_module(&mut env, &mut type_values);
    super::python_executable::python_executable_env(&mut env, &mut type_values);
    super::python_packaging_policy::python_packaging_policy_module(&mut env, &mut type_values);

    env.set("CWD", Value::from(context.cwd.display().to_string()))?;
    env.set(
        "CONFIG_PATH",
        Value::from(context.config_path.display().to_string()),
    )?;
    env.set(
        "BUILD_TARGET_TRIPLE",
        Value::from(context.build_target_triple.clone()),
    )?;

    env.set("CONTEXT", Value::new(context))?;

    // We alias various globals as PyOxidizer.* attributes so they are
    // available via the type object API. This is a bit hacky. But it allows
    // Rust code with only access to the TypeValues dictionary to retrieve
    // these globals.
    for f in &[
        "set_build_path",
        "CONTEXT",
        "CWD",
        "CONFIG_PATH",
        "BUILD_TARGET_TRIPLE",
    ] {
        type_values.add_type_value(PyOxidizerContext::TYPE, f, env.get(f)?);
    }

    Ok((env, type_values))
}

#[cfg(test)]
pub mod tests {
    use super::super::testutil::*;

    #[test]
    fn test_cwd() {
        let cwd = starlark_ok("CWD");
        let pwd = std::env::current_dir().unwrap();
        assert_eq!(cwd.to_str(), pwd.display().to_string());
    }

    #[test]
    fn test_build_target() {
        let target = starlark_ok("BUILD_TARGET_TRIPLE");
        assert_eq!(target.to_str(), crate::project_building::HOST);
    }

    #[test]
    fn test_print() {
        starlark_ok("print('hello, world')");
    }
}
