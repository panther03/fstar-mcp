//! F* configuration handling.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

const CONFIG_SUFFIX: &str = ".fst.config.json";

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Failed to parse config file: {0}")]
    ParseError(#[from] serde_json::Error),
    #[error("Environment variable '{variable}' referenced by {config_file} is not set")]
    MissingEnvironmentVariable {
        variable: String,
        config_file: PathBuf,
    },
    #[error("Environment variable '{variable}' referenced by {config_file} is not valid Unicode")]
    InvalidEnvironmentVariable {
        variable: String,
        config_file: PathBuf,
    },
    #[error("Invalid shell-like output from make: {0}")]
    InvalidMakeOutput(String),
    #[error("F* executable not found: '{executable}' (working directory: {cwd})")]
    ExecutableNotFound { executable: String, cwd: PathBuf },
}

/// F* configuration - all fields optional with sensible defaults
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FStarConfig {
    /// Include directories (--include paths)
    #[serde(default)]
    pub include_dirs: Vec<String>,

    /// Options to pass to fstar.exe
    #[serde(default)]
    pub options: Vec<String>,

    /// Path to fstar.exe (defaults to "fstar.exe" in PATH)
    #[serde(default)]
    pub fstar_exe: Option<String>,

    /// Working directory for fstar.exe
    #[serde(default)]
    pub cwd: Option<String>,
}

impl FStarConfig {
    /// Discover and resolve configuration for an F* source file.
    ///
    /// Discovery checks the nearest `*.fst.config.json` up to `workspace_root`,
    /// then `make <filename>-in`, then bare defaults.
    pub fn discover(file_path: &Path, workspace_root: Option<&Path>) -> Result<Self, ConfigError> {
        Self::discover_with_overrides(file_path, workspace_root, &Self::default())
    }

    /// Discover configuration and apply explicit caller-provided overrides.
    ///
    /// `Some` scalar fields and non-empty vectors replace discovered values.
    pub fn discover_with_overrides(
        file_path: &Path,
        workspace_root: Option<&Path>,
        overrides: &Self,
    ) -> Result<Self, ConfigError> {
        let file_path = absolute_path(file_path)?;
        let file_parent = file_path.parent().unwrap_or(Path::new("."));

        let mut config = if let Some(config_file) = find_config_file(&file_path, workspace_root)? {
            let mut config = parse_config_file(&config_file)?;
            if config.cwd.is_none() {
                config.cwd = Some(
                    config_file
                        .parent()
                        .unwrap_or(Path::new("."))
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            config
        } else if let Some(config) = config_from_makefile(&file_path)? {
            config
        } else {
            Self {
                cwd: Some(file_parent.to_string_lossy().into_owned()),
                ..Self::default()
            }
        };

        config.apply_overrides(overrides);
        if config.cwd.is_none() {
            config.cwd = Some(file_parent.to_string_lossy().into_owned());
        }

        let cwd = config.cwd_or(file_parent);
        let executable = config.fstar_exe().to_string();
        let resolved = resolve_executable(&executable, &cwd)?;
        config.fstar_exe = Some(resolved.to_string_lossy().into_owned());

        Ok(config)
    }

    /// Get the F* executable path (with default)
    pub fn fstar_exe(&self) -> &str {
        self.fstar_exe.as_deref().unwrap_or("fstar.exe")
    }

    /// Get the working directory (with default)
    pub fn cwd_or(&self, default: &Path) -> PathBuf {
        self.cwd
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| default.to_path_buf())
    }

    /// Build command-line arguments for F* IDE mode
    pub fn build_args(&self, file_path: &str, lax: bool) -> Vec<String> {
        let mut args = vec!["--ide".to_string(), file_path.to_string()];

        if lax {
            args.push("--admit_smt_queries".to_string());
            args.push("true".to_string());
        }

        // Add custom options
        args.extend(self.options.clone());

        // Add include directories
        for dir in &self.include_dirs {
            args.push("--include".to_string());
            args.push(dir.clone());
        }

        args
    }

    fn apply_overrides(&mut self, overrides: &Self) {
        if !overrides.include_dirs.is_empty() {
            self.include_dirs.clone_from(&overrides.include_dirs);
        }
        if !overrides.options.is_empty() {
            self.options.clone_from(&overrides.options);
        }
        if overrides.fstar_exe.is_some() {
            self.fstar_exe.clone_from(&overrides.fstar_exe);
        }
        if overrides.cwd.is_some() {
            self.cwd.clone_from(&overrides.cwd);
        }
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn find_config_file(
    file_path: &Path,
    workspace_root: Option<&Path>,
) -> Result<Option<PathBuf>, ConfigError> {
    let mut directory = file_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let workspace_root = workspace_root.map(absolute_path).transpose()?;

    if let Some(root) = &workspace_root {
        if !directory.starts_with(root) {
            return Ok(None);
        }
    }

    loop {
        let mut matches = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(CONFIG_SUFFIX))
            {
                matches.push(entry.path());
            }
        }
        matches.sort();
        if let Some(config_file) = matches.into_iter().next() {
            return Ok(Some(config_file));
        }

        if workspace_root.as_deref() == Some(directory.as_path()) {
            break;
        }
        let Some(parent) = directory.parent() else {
            break;
        };
        if workspace_root
            .as_ref()
            .is_some_and(|root| !parent.starts_with(root))
        {
            break;
        }
        directory = parent.to_path_buf();
    }

    Ok(None)
}

fn parse_config_file(config_file: &Path) -> Result<FStarConfig, ConfigError> {
    let contents = fs::read_to_string(config_file)?;
    let mut value: Value = serde_json::from_str(&contents)?;
    substitute_env_vars_in_value(&mut value, config_file)?;
    Ok(serde_json::from_value(value)?)
}

fn substitute_env_vars_in_value(value: &mut Value, config_file: &Path) -> Result<(), ConfigError> {
    match value {
        Value::String(string) => {
            *string = substitute_env_vars(string, config_file)?;
        }
        Value::Array(values) => {
            for value in values {
                substitute_env_vars_in_value(value, config_file)?;
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                substitute_env_vars_in_value(value, config_file)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn substitute_env_vars(input: &str, config_file: &Path) -> Result<String, ConfigError> {
    let chars: Vec<char> = input.chars().collect();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;

    while index < chars.len() {
        if chars[index] != '$' {
            output.push(chars[index]);
            index += 1;
            continue;
        }

        let (name, next_index) = if chars.get(index + 1) == Some(&'{') {
            let name_start = index + 2;
            let Some(relative_end) = chars[name_start..].iter().position(|ch| *ch == '}') else {
                output.push('$');
                index += 1;
                continue;
            };
            let name_end = name_start + relative_end;
            let name: String = chars[name_start..name_end].iter().collect();
            if !is_env_name(&name) {
                output.push('$');
                index += 1;
                continue;
            }
            (name, name_end + 1)
        } else {
            let name_start = index + 1;
            if chars
                .get(name_start)
                .is_none_or(|ch| !is_env_name_start(*ch))
            {
                output.push('$');
                index += 1;
                continue;
            }
            let mut name_end = name_start + 1;
            while chars
                .get(name_end)
                .is_some_and(|ch| is_env_name_continue(*ch))
            {
                name_end += 1;
            }
            (
                chars[name_start..name_end].iter().collect::<String>(),
                name_end,
            )
        };

        match env::var(&name) {
            Ok(value) => output.push_str(&value),
            Err(env::VarError::NotPresent) => {
                return Err(ConfigError::MissingEnvironmentVariable {
                    variable: name,
                    config_file: config_file.to_path_buf(),
                });
            }
            Err(env::VarError::NotUnicode(_)) => {
                return Err(ConfigError::InvalidEnvironmentVariable {
                    variable: name,
                    config_file: config_file.to_path_buf(),
                });
            }
        }
        index = next_index;
    }

    Ok(output)
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(is_env_name_start) && chars.all(is_env_name_continue)
}

fn is_env_name_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_env_name_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn config_from_makefile(file_path: &Path) -> Result<Option<FStarConfig>, ConfigError> {
    let cwd = file_path.parent().unwrap_or(Path::new("."));
    let Some(file_name) = file_path.file_name().and_then(OsStr::to_str) else {
        return Ok(None);
    };

    let output = match Command::new("make")
        .arg(format!("{file_name}-in"))
        .current_dir(cwd)
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(_) | Err(_) => return Ok(None),
    };

    let words = shell_split(&String::from_utf8_lossy(&output.stdout))?;
    let (options, include_dirs) = split_make_options(words)?;
    Ok(Some(FStarConfig {
        include_dirs,
        options,
        fstar_exe: None,
        cwd: Some(cwd.to_string_lossy().into_owned()),
    }))
}

fn split_make_options(words: Vec<String>) -> Result<(Vec<String>, Vec<String>), ConfigError> {
    let mut options = Vec::new();
    let mut include_dirs = Vec::new();
    let mut words = words.into_iter();

    while let Some(word) = words.next() {
        if word == "--include" {
            let include = words.next().ok_or_else(|| {
                ConfigError::InvalidMakeOutput(
                    "'--include' must be followed by a directory".to_string(),
                )
            })?;
            include_dirs.push(include);
        } else {
            options.push(word);
        }
    }

    Ok((options, include_dirs))
}

fn shell_split(input: &str) -> Result<Vec<String>, ConfigError> {
    #[derive(Clone, Copy)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut started = false;

    for ch in input.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
            started = true;
            continue;
        }

        match quote {
            Quote::None => match ch {
                '\\' => {
                    escaped = true;
                    started = true;
                }
                '\'' => {
                    quote = Quote::Single;
                    started = true;
                }
                '"' => {
                    quote = Quote::Double;
                    started = true;
                }
                ch if ch.is_whitespace() => {
                    if started {
                        words.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                _ => {
                    word.push(ch);
                    started = true;
                }
            },
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    word.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '\\' => escaped = true,
                _ => word.push(ch),
            },
        }
    }

    if escaped {
        return Err(ConfigError::InvalidMakeOutput(
            "trailing backslash".to_string(),
        ));
    }
    if !matches!(quote, Quote::None) {
        return Err(ConfigError::InvalidMakeOutput(
            "unterminated quoted string".to_string(),
        ));
    }
    if started {
        words.push(word);
    }

    Ok(words)
}

fn resolve_executable(executable: &str, cwd: &Path) -> Result<PathBuf, ConfigError> {
    resolve_executable_with_path(executable, cwd, env::var_os("PATH").as_deref())
}

fn resolve_executable_with_path(
    executable: &str,
    cwd: &Path,
    path: Option<&OsStr>,
) -> Result<PathBuf, ConfigError> {
    if has_path_separator(executable) {
        let candidate = Path::new(executable);
        let candidate = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            absolute_path(cwd)
                .unwrap_or_else(|_| cwd.to_path_buf())
                .join(candidate)
        };
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    } else if let Some(path) = path {
        for directory in env::split_paths(path) {
            for candidate in executable_candidates(&directory, executable) {
                if is_executable(&candidate) {
                    return absolute_path(&candidate).map_err(ConfigError::IoError);
                }
            }
        }
    }

    Err(ConfigError::ExecutableNotFound {
        executable: executable.to_string(),
        cwd: cwd.to_path_buf(),
    })
}

#[cfg(not(windows))]
fn has_path_separator(path: &str) -> bool {
    path.contains(std::path::MAIN_SEPARATOR)
}

#[cfg(windows)]
fn has_path_separator(path: &str) -> bool {
    path.contains(['/', '\\'])
}

#[cfg(not(windows))]
fn executable_candidates(directory: &Path, executable: &str) -> Vec<PathBuf> {
    vec![directory.join(executable)]
}

#[cfg(windows)]
fn executable_candidates(directory: &Path, executable: &str) -> Vec<PathBuf> {
    let path = Path::new(executable);
    if path.extension().is_some() {
        return vec![directory.join(path)];
    }

    let extensions = env::var_os("PATHEXT")
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into())
        .to_string_lossy()
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| directory.join(format!("{executable}{extension}")))
        .collect::<Vec<_>>();

    let mut candidates = vec![directory.join(path)];
    candidates.extend(extensions);
    candidates
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(name: &str) -> Self {
            let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("config-tests")
                .join(format!("{name}-{}-{id}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn join(&self, path: impl AsRef<Path>) -> PathBuf {
            self.path.join(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn executable_override() -> FStarConfig {
        FStarConfig {
            fstar_exe: Some(env::current_exe().unwrap().to_string_lossy().into_owned()),
            ..FStarConfig::default()
        }
    }

    #[test]
    fn test_default_config() {
        let config = FStarConfig::default();
        assert_eq!(config.fstar_exe(), "fstar.exe");
        assert!(config.include_dirs.is_empty());
        assert!(config.options.is_empty());
    }

    #[test]
    fn test_build_args() {
        let config = FStarConfig {
            include_dirs: vec!["/path/to/lib".to_string()],
            options: vec!["--cache_dir".to_string(), ".cache".to_string()],
            fstar_exe: Some("fstar".to_string()),
            cwd: Some("/project".to_string()),
        };

        let args = config.build_args("/path/to/Test.fst", false);
        assert_eq!(args[0], "--ide");
        assert_eq!(args[1], "/path/to/Test.fst");
        assert!(args.contains(&"--include".to_string()));
        assert!(args.contains(&"/path/to/lib".to_string()));
        assert!(args.contains(&"--cache_dir".to_string()));
    }

    #[test]
    fn test_build_args_lax() {
        let config = FStarConfig::default();
        let args = config.build_args("Test.fst", true);
        assert!(args.contains(&"--admit_smt_queries".to_string()));
        assert!(args.contains(&"true".to_string()));
    }

    #[test]
    fn shell_split_supports_quotes_backslashes_and_empty_words() {
        let words = shell_split(r#"one 'two three' "four five" six\ seven "" ''"#).unwrap();
        assert_eq!(
            words,
            ["one", "two three", "four five", "six seven", "", ""]
        );
    }

    #[test]
    fn shell_split_rejects_unfinished_input() {
        assert!(matches!(
            shell_split("'unfinished"),
            Err(ConfigError::InvalidMakeOutput(_))
        ));
        assert!(matches!(
            shell_split("unfinished\\"),
            Err(ConfigError::InvalidMakeOutput(_))
        ));
    }

    #[test]
    fn make_options_separate_include_pairs() {
        let words =
            shell_split(r#"--foo "two words" --include "lib one" --include lib\ two"#).unwrap();
        let (options, includes) = split_make_options(words).unwrap();
        assert_eq!(options, ["--foo", "two words"]);
        assert_eq!(includes, ["lib one", "lib two"]);
    }

    #[test]
    fn discovers_nearest_config_and_substitutes_environment_recursively() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        let test_dir = TestDir::new("nearest-config");
        let workspace = test_dir.join("workspace");
        let nested = workspace.join("src/nested");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("Test.fst");
        fs::write(&file, "module Test").unwrap();

        let variable = format!("FSTAR_MCP_CONFIG_TEST_{}", std::process::id());
        env::set_var(&variable, workspace.to_string_lossy().as_ref());
        fs::write(
            workspace.join("outer.fst.config.json"),
            serde_json::json!({
                "options": ["--outer"],
                "fstar_exe": env::current_exe().unwrap()
            })
            .to_string(),
        )
        .unwrap();
        let nearest_config = nested.join("nearest.fst.config.json");
        fs::write(
            &nearest_config,
            serde_json::json!({
                "include_dirs": [format!("${{{variable}}}/lib")],
                "options": [format!("${variable}")],
                "fstar_exe": env::current_exe().unwrap()
            })
            .to_string(),
        )
        .unwrap();

        let config = FStarConfig::discover(&file, Some(&workspace)).unwrap();
        assert_eq!(
            config.include_dirs,
            [format!("{}/lib", workspace.to_string_lossy())]
        );
        assert_eq!(config.options, [workspace.to_string_lossy().into_owned()]);
        assert_eq!(
            config.cwd,
            Some(
                nearest_config
                    .parent()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            )
        );
        assert_eq!(
            config.fstar_exe,
            Some(env::current_exe().unwrap().to_string_lossy().into_owned())
        );

        env::remove_var(variable);
    }

    #[test]
    fn missing_environment_variable_is_a_clear_error() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        let test_dir = TestDir::new("missing-env");
        let file = test_dir.join("Test.fst");
        fs::write(&file, "module Test").unwrap();
        let variable = format!(
            "FSTAR_MCP_DEFINITELY_MISSING_{}_{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        );
        env::remove_var(&variable);
        fs::write(
            test_dir.join("test.fst.config.json"),
            serde_json::json!({
                "options": [format!("${{{variable}}}")],
                "fstar_exe": env::current_exe().unwrap()
            })
            .to_string(),
        )
        .unwrap();

        let error = FStarConfig::discover(&file, Some(&test_dir.path)).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::MissingEnvironmentVariable {
                variable: ref missing,
                ..
            } if missing == &variable
        ));
        assert!(error.to_string().contains(&variable));
    }

    #[test]
    fn workspace_root_prevents_searching_higher_parents() {
        let test_dir = TestDir::new("workspace-boundary");
        let workspace = test_dir.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join("Test.fst");
        fs::write(&file, "module Test").unwrap();
        fs::write(
            test_dir.join("outside.fst.config.json"),
            serde_json::json!({
                "options": ["--outside"],
                "fstar_exe": env::current_exe().unwrap()
            })
            .to_string(),
        )
        .unwrap();

        let config =
            FStarConfig::discover_with_overrides(&file, Some(&workspace), &executable_override())
                .unwrap();
        assert!(config.options.is_empty());
        assert_eq!(config.cwd, Some(workspace.to_string_lossy().into_owned()));
    }

    #[test]
    fn falls_back_to_make_and_parses_shell_like_output() {
        if Command::new("make").arg("--version").output().is_err() {
            return;
        }

        let test_dir = TestDir::new("make-fallback");
        let file = test_dir.join("Sample.fst");
        fs::write(&file, "module Sample").unwrap();
        fs::write(
            test_dir.join("Makefile"),
            "Sample.fst-in:\n\t@printf '%s\\n' '--foo \"two words\" --include \"lib one\" --include lib\\ two'\n",
        )
        .unwrap();

        let config =
            FStarConfig::discover_with_overrides(&file, None, &executable_override()).unwrap();
        assert_eq!(config.options, ["--foo", "two words"]);
        assert_eq!(config.include_dirs, ["lib one", "lib two"]);
        assert_eq!(
            config.cwd,
            Some(test_dir.path.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn uses_bare_defaults_when_config_and_make_are_absent() {
        let test_dir = TestDir::new("bare-defaults");
        let file = test_dir.join("NoMakefile.fst");
        fs::write(&file, "module NoMakefile").unwrap();

        let config =
            FStarConfig::discover_with_overrides(&file, None, &executable_override()).unwrap();
        assert!(config.options.is_empty());
        assert!(config.include_dirs.is_empty());
        assert_eq!(
            config.cwd,
            Some(test_dir.path.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn explicit_nonempty_overrides_replace_discovered_values() {
        let test_dir = TestDir::new("overrides");
        let file = test_dir.join("Test.fst");
        fs::write(&file, "module Test").unwrap();
        fs::write(
            test_dir.join("test.fst.config.json"),
            serde_json::json!({
                "include_dirs": ["discovered-lib"],
                "options": ["--discovered"],
                "fstar_exe": "missing-discovered-executable"
            })
            .to_string(),
        )
        .unwrap();

        let overrides = FStarConfig {
            include_dirs: Vec::new(),
            options: vec!["--override".to_string()],
            fstar_exe: Some(env::current_exe().unwrap().to_string_lossy().into_owned()),
            cwd: Some(test_dir.path.to_string_lossy().into_owned()),
        };
        let config = FStarConfig::discover_with_overrides(&file, None, &overrides).unwrap();

        assert_eq!(config.include_dirs, ["discovered-lib"]);
        assert_eq!(config.options, ["--override"]);
        assert_eq!(config.cwd, overrides.cwd);
        assert_eq!(config.fstar_exe, overrides.fstar_exe);
    }

    #[test]
    fn resolves_relative_executable_against_cwd() {
        let test_dir = TestDir::new("relative-executable");
        let executable = test_dir.join("fake-fstar");
        fs::write(&executable, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&executable, permissions).unwrap();
        }

        let resolved = resolve_executable("./fake-fstar", &test_dir.path).unwrap();
        assert_eq!(resolved, executable);
    }

    #[test]
    fn resolves_bare_executable_on_supplied_path() {
        let test_dir = TestDir::new("path-executable");
        let executable = test_dir.join("fstar-test");
        fs::write(&executable, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&executable, permissions).unwrap();
        }

        let path = OsString::from(test_dir.path.as_os_str());
        let resolved =
            resolve_executable_with_path("fstar-test", Path::new("."), Some(&path)).unwrap();
        assert_eq!(resolved, executable);
    }

    #[test]
    fn missing_executable_has_a_clear_error() {
        let error = resolve_executable_with_path(
            "definitely-missing-fstar",
            Path::new("/project"),
            Some(OsStr::new("")),
        )
        .unwrap_err();

        assert!(matches!(error, ConfigError::ExecutableNotFound { .. }));
        assert!(error.to_string().contains("definitely-missing-fstar"));
    }
}
