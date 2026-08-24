//! Small, deterministic classification of commands intercepted by GuardWSL.
//!
//! Classification happens before NVM/Corepack resolution so the logical tool
//! identity is preserved when `yarn` becomes `node yarn.js`.

use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionClass {
    HeavyBuild,
    TestOrCheck,
    Install,
    Other,
}

impl AdmissionClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HeavyBuild => "heavy_build",
            Self::TestOrCheck => "test_or_check",
            Self::Install => "install",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandIntent {
    pub logical_tool: String,
    pub args: Vec<String>,
    pub class: AdmissionClass,
}

impl CommandIntent {
    #[must_use]
    pub fn classify(tool: &OsStr, args: &[OsString]) -> Self {
        let logical_tool = normalized_tool(tool);
        let args = args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let class = classify_strings(&logical_tool, &args);
        Self {
            logical_tool,
            args,
            class,
        }
    }
}

#[must_use]
pub fn is_supported_shim(tool: &str) -> bool {
    matches!(
        normalized_tool(OsStr::new(tool)).as_str(),
        "cargo"
            | "rustc"
            | "go"
            | "npm"
            | "npx"
            | "yarn"
            | "pnpm"
            | "bun"
            | "corepack"
            | "next"
            | "vite"
            | "tsc"
            | "docker"
            | "docker-compose"
            | "make"
            | "ninja"
            | "cmake"
            | "gradle"
            | "gradlew"
            | "mvn"
            | "mvnw"
            | "dotnet"
    )
}

#[must_use]
pub fn classify_strings(tool: &str, args: &[String]) -> AdmissionClass {
    let tool = normalized_tool(OsStr::new(tool));
    match tool.as_str() {
        "cargo" => classify_cargo(args),
        "rustc" => AdmissionClass::HeavyBuild,
        "go" => classify_go(args),
        "npm" | "yarn" | "pnpm" | "bun" => classify_javascript(&tool, args),
        "npx" => classify_npx(args),
        "corepack" => classify_corepack(args),
        "node" => classify_node(args),
        "next" => classify_simple_subcommand(args, &["build"], &["lint", "test"]),
        "vite" => classify_simple_subcommand(args, &["build"], &["test", "lint"]),
        "tsc" => AdmissionClass::TestOrCheck,
        "docker" => classify_docker(args),
        "docker-compose" => classify_docker_compose(args),
        "make" | "ninja" | "cmake" => classify_native_build(args),
        "gradle" | "gradlew" | "mvn" | "mvnw" => classify_jvm(args),
        "dotnet" => classify_dotnet(args),
        _ => AdmissionClass::Other,
    }
}

fn normalized_tool(tool: &OsStr) -> String {
    let basename = Path::new(tool)
        .file_name()
        .unwrap_or(tool)
        .to_string_lossy()
        .to_ascii_lowercase();
    basename
        .strip_suffix(".exe")
        .or_else(|| basename.strip_suffix(".cmd"))
        .or_else(|| basename.strip_suffix(".bat"))
        .unwrap_or(&basename)
        .to_owned()
}

fn first_positional_index(args: &[String], options_with_value: &[&str]) -> Option<usize> {
    let mut index = 0;
    while index < args.len() {
        let value = args[index].as_str();
        if options_with_value.contains(&value) {
            index = index.saturating_add(2);
            continue;
        }
        if options_with_value
            .iter()
            .any(|option| value.starts_with(&format!("{option}=")))
            || value == "--"
            || value.starts_with('-')
            || value.starts_with('+')
        {
            index += 1;
            continue;
        }
        return Some(index);
    }
    None
}

fn first_subcommand(args: &[String]) -> Option<&str> {
    first_positional_index(args, &[]).map(|index| args[index].as_str())
}

fn classify_cargo(args: &[String]) -> AdmissionClass {
    const OPTIONS_WITH_VALUE: &[&str] = &[
        "--color",
        "--config",
        "--jobs",
        "-j",
        "--lockfile-path",
        "--manifest-path",
        "--target",
        "--target-dir",
        "-Z",
    ];
    let command =
        first_positional_index(args, OPTIONS_WITH_VALUE).map(|index| args[index].as_str());
    match command {
        Some("test" | "check" | "clippy" | "fmt" | "miri" | "doc") => AdmissionClass::TestOrCheck,
        Some("install" | "add" | "update" | "fetch" | "vendor") => AdmissionClass::Install,
        Some("build" | "run" | "bench" | "rustc") => AdmissionClass::HeavyBuild,
        _ => AdmissionClass::Other,
    }
}

fn classify_go(args: &[String]) -> AdmissionClass {
    match first_subcommand(args) {
        Some("test" | "vet" | "fmt") => AdmissionClass::TestOrCheck,
        Some("install" | "get" | "mod" | "work") => AdmissionClass::Install,
        Some("build" | "run") => AdmissionClass::HeavyBuild,
        _ => AdmissionClass::Other,
    }
}

fn classify_javascript(tool: &str, args: &[String]) -> AdmissionClass {
    let Some((command, tail)) = javascript_command(tool, args) else {
        return AdmissionClass::Other;
    };
    if matches!(command, "install" | "i" | "add" | "update" | "up" | "ci") {
        return AdmissionClass::Install;
    }
    if is_test_or_check(command) {
        return AdmissionClass::TestOrCheck;
    }
    if is_build_name(command) {
        return AdmissionClass::HeavyBuild;
    }
    if matches!(command, "exec" | "dlx" | "x") {
        return classify_nested(tail);
    }
    AdmissionClass::Other
}

fn javascript_command<'a>(tool: &str, args: &'a [String]) -> Option<(&'a str, &'a [String])> {
    let options_with_value: &[&str] = match tool {
        "npm" => &[
            "--cache",
            "--prefix",
            "--registry",
            "--userconfig",
            "--workspace",
            "-w",
        ],
        "pnpm" => &[
            "--cache",
            "--dir",
            "-C",
            "--filter",
            "--filter-prod",
            "-F",
            "--registry",
            "--reporter",
        ],
        "yarn" => &["--cache-folder", "--cwd", "--mutex", "--use-yarnrc"],
        "bun" => &["--cwd"],
        _ => &[],
    };
    let position = first_positional_index(args, options_with_value)?;
    let mut command = args[position].as_str();
    let mut tail = &args[position + 1..];
    if matches!(command, "run" | "run-script") {
        let nested = first_positional_index(tail, &[])?;
        command = tail[nested].as_str();
        tail = &tail[nested + 1..];
    }
    Some((command, tail))
}

fn classify_npx(args: &[String]) -> AdmissionClass {
    const OPTIONS_WITH_VALUE: &[&str] = &["--cache", "--node-options", "--package", "-p"];
    let Some(index) = first_positional_index(args, OPTIONS_WITH_VALUE) else {
        return AdmissionClass::Other;
    };
    classify_nested(&args[index..])
}

fn classify_corepack(args: &[String]) -> AdmissionClass {
    let Some(index) = first_positional_index(args, &["--install-directory"]) else {
        return AdmissionClass::Other;
    };
    let tool = normalized_tool(OsStr::new(&args[index]));
    classify_strings(&tool, &args[index + 1..])
}

fn classify_node(args: &[String]) -> AdmissionClass {
    let Some(index) = first_positional_index(args, &["--loader", "--require", "-r"]) else {
        return AdmissionClass::Other;
    };
    let script = args[index].to_ascii_lowercase();
    let nested = &args[index + 1..];
    if script.contains("yarn") {
        return classify_strings("yarn", nested);
    }
    if script.contains("pnpm") {
        return classify_strings("pnpm", nested);
    }
    if script.contains("npm-cli") || script.ends_with("/npm") {
        return classify_strings("npm", nested);
    }
    if script.contains("corepack") {
        return classify_strings("corepack", nested);
    }
    if script.contains("next") {
        return classify_strings("next", nested);
    }
    if script.contains("vite") {
        return classify_strings("vite", nested);
    }
    AdmissionClass::Other
}

fn classify_nested(args: &[String]) -> AdmissionClass {
    let Some(tool_index) = first_positional_index(args, &["--call", "-c", "--package", "-p"])
    else {
        return AdmissionClass::Other;
    };
    let tool = &args[tool_index];
    let nested = &args[tool_index + 1..];
    let classified = classify_strings(tool, nested);
    if classified != AdmissionClass::Other {
        return classified;
    }
    let normalized = normalized_tool(OsStr::new(tool));
    if matches!(normalized.as_str(), "nx" | "turbo") {
        if nested.iter().any(|value| is_test_or_check(value)) {
            return AdmissionClass::TestOrCheck;
        }
        if nested.iter().any(|value| is_build_name(value)) {
            return AdmissionClass::HeavyBuild;
        }
    }
    if matches!(
        normalized.as_str(),
        "ava"
            | "cypress"
            | "eslint"
            | "jest"
            | "mocha"
            | "playwright"
            | "prettier"
            | "pytest"
            | "ruff"
            | "vitest"
    ) && !nested
        .iter()
        .any(|value| matches!(value.as_str(), "install" | "build"))
    {
        return AdmissionClass::TestOrCheck;
    }
    match first_subcommand(nested) {
        Some(command) if is_test_or_check(command) => AdmissionClass::TestOrCheck,
        Some(command) if is_build_name(command) => AdmissionClass::HeavyBuild,
        Some("install") => AdmissionClass::Install,
        _ => AdmissionClass::Other,
    }
}

fn classify_docker(args: &[String]) -> AdmissionClass {
    let Some(command) = first_subcommand(args) else {
        return AdmissionClass::Other;
    };
    match command {
        "build" => AdmissionClass::HeavyBuild,
        "buildx"
            if args
                .iter()
                .any(|value| matches!(value.as_str(), "build" | "bake")) =>
        {
            AdmissionClass::HeavyBuild
        }
        "compose" => {
            if args
                .iter()
                .any(|value| matches!(value.as_str(), "build" | "--build"))
            {
                AdmissionClass::HeavyBuild
            } else {
                AdmissionClass::Other
            }
        }
        _ => AdmissionClass::Other,
    }
}

fn classify_docker_compose(args: &[String]) -> AdmissionClass {
    if args
        .iter()
        .any(|value| matches!(value.as_str(), "build" | "--build"))
    {
        AdmissionClass::HeavyBuild
    } else {
        AdmissionClass::Other
    }
}

fn classify_native_build(args: &[String]) -> AdmissionClass {
    if args.iter().any(|value| is_test_or_check(value)) {
        AdmissionClass::TestOrCheck
    } else {
        AdmissionClass::HeavyBuild
    }
}

fn classify_jvm(args: &[String]) -> AdmissionClass {
    if args.iter().any(|value| is_test_or_check(value)) {
        AdmissionClass::TestOrCheck
    } else if args.iter().any(|value| {
        matches!(
            value.as_str(),
            "build" | "assemble" | "package" | "install" | "shadowJar"
        )
    }) {
        AdmissionClass::HeavyBuild
    } else {
        AdmissionClass::Other
    }
}

fn classify_dotnet(args: &[String]) -> AdmissionClass {
    match first_subcommand(args) {
        Some("test" | "format") => AdmissionClass::TestOrCheck,
        Some("restore" | "tool" | "add") => AdmissionClass::Install,
        Some("build" | "publish" | "run" | "pack") => AdmissionClass::HeavyBuild,
        _ => AdmissionClass::Other,
    }
}

fn classify_simple_subcommand(args: &[String], heavy: &[&str], checks: &[&str]) -> AdmissionClass {
    match first_subcommand(args) {
        Some(command) if heavy.contains(&command) => AdmissionClass::HeavyBuild,
        Some(command) if checks.contains(&command) => AdmissionClass::TestOrCheck,
        _ => AdmissionClass::Other,
    }
}

fn is_build_name(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value == "build"
        || value == "compile"
        || value == "bundle"
        || value == "package"
        || value.starts_with("build:")
        || value.ends_with(":build")
}

fn is_test_or_check(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    matches!(
        value.as_str(),
        "test"
            | "tests"
            | "check"
            | "lint"
            | "typecheck"
            | "type-check"
            | "fmt"
            | "format"
            | "e2e"
            | "coverage"
            | "verify"
    ) || value.starts_with("test:")
        || value.starts_with("check:")
        || value.starts_with("lint:")
        || value.starts_with("e2e:")
        || value.ends_with(":test")
        || value.ends_with(":check")
        || value.ends_with(":e2e")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(tool: &str, args: &[&str]) -> AdmissionClass {
        classify_strings(
            tool,
            &args
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn cargo_uses_the_real_subcommand_instead_of_bag_of_words() {
        assert_eq!(
            classify("cargo", &["run", "--bin", "install"]),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(classify("cargo", &["test"]), AdmissionClass::TestOrCheck);
        assert_eq!(
            classify("cargo", &["install", "ripgrep"]),
            AdmissionClass::Install
        );
        assert_eq!(
            classify(
                "cargo",
                &["--manifest-path", "workspace/Cargo.toml", "build"]
            ),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(
            classify("cargo", &["--manifest-path", "build", "test"]),
            AdmissionClass::TestOrCheck
        );
        assert_eq!(
            classify("cargo", &["-Z", "unstable-options", "build"]),
            AdmissionClass::HeavyBuild
        );
    }

    #[test]
    fn javascript_package_managers_preserve_build_intent() {
        for tool in ["npm", "yarn", "pnpm", "bun"] {
            assert_eq!(
                classify(tool, &["run", "build"]),
                AdmissionClass::HeavyBuild
            );
            assert_eq!(classify(tool, &["test"]), AdmissionClass::TestOrCheck);
        }
        assert_eq!(
            classify("corepack", &["yarn", "build"]),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(
            classify("npx", &["next", "build"]),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(
            classify("pnpm", &["--filter", "app", "build"]),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(
            classify("pnpm", &["--filter=app", "test"]),
            AdmissionClass::TestOrCheck
        );
        assert_eq!(
            classify("pnpm", &["-w", "build"]),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(
            classify("npm", &["-w", "app", "run", "build"]),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(
            classify("npm", &["run", "check:types"]),
            AdmissionClass::TestOrCheck
        );
        assert_eq!(
            classify("npm", &["run", "e2e"]),
            AdmissionClass::TestOrCheck
        );
        assert_eq!(
            classify("npx", &["playwright", "test"]),
            AdmissionClass::TestOrCheck
        );
        assert_eq!(
            classify("npm", &["exec", "--", "next", "build"]),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(
            classify("pnpm", &["exec", "turbo", "run", "build"]),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(
            classify("pnpm", &["exec", "turbo", "run", "test"]),
            AdmissionClass::TestOrCheck
        );
    }

    #[test]
    fn resolved_node_wrappers_do_not_hide_the_logical_build() {
        assert_eq!(
            classify("node", &["/home/u/.nvm/yarn.js", "build"]),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(
            classify("node", &["node_modules/next/dist/bin/next", "build"]),
            AdmissionClass::HeavyBuild
        );
    }

    #[test]
    fn docker_and_native_builds_are_heavy_but_checks_keep_their_direct_class() {
        assert_eq!(
            classify("docker", &["build", "."]),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(
            classify("docker", &["buildx", "bake"]),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(
            classify("docker-compose", &["-f", "compose.yml", "build"]),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(
            classify("docker", &["compose", "up", "--build"]),
            AdmissionClass::HeavyBuild
        );
        assert_eq!(classify("make", &["test"]), AdmissionClass::TestOrCheck);
        assert_eq!(classify("ninja", &[]), AdmissionClass::HeavyBuild);
    }

    #[test]
    fn unknown_commands_are_not_guessed_from_unrelated_arguments() {
        assert_eq!(
            classify("printf", &["please", "build"]),
            AdmissionClass::Other
        );
    }
}
