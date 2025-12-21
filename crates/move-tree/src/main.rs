use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use move_binary_format::file_format::{CompiledModule, SignatureToken, Visibility};
use move_package_alt::{package::RootPackage, schema::Environment};
use move_package_alt_compilation::{build_config::BuildConfig, compiled_package::CompiledPackage};
use sui_package_alt::SuiFlavor;
use walkdir::{DirEntry, WalkDir};

#[derive(Parser, Debug)]
#[command(about = "Render a tree of Move modules and public function signatures")]
struct Args {
    /// Path to a Move package directory (or a folder containing Move packages)
    path: PathBuf,
    /// Disable ANSI colors
    #[arg(long)]
    no_color: bool,
}

struct ModuleInfo {
    name: String,
    functions: Vec<FunctionInfo>,
}

struct FunctionInfo {
    name: String,
    type_params: Vec<String>,
    params: Vec<String>,
    returns: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.no_color {
        colored::control::set_override(false);
    }

    let package_roots = find_move_packages(&args.path)?;
    if package_roots.is_empty() {
        bail!("No Move.toml found under {}", args.path.display());
    }

    let mut first = true;
    for root in package_roots {
        let compiled = compile_package(&root)
            .await
            .with_context(|| format!("Failed to compile Move package at {}", root.display()))?;
        let modules = collect_modules(&compiled);
        let package_name = compiled.compiled_package_info.package_name.as_str().to_string();

        if !first {
            println!();
        }
        first = false;

        print_package_tree(&args.path, &root, &package_name, &modules);
    }

    Ok(())
}

fn find_move_packages(path: &Path) -> Result<Vec<PathBuf>> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("Unable to access {}", path.display()))?;

    let mut roots = BTreeSet::new();

    if metadata.is_file() {
        if path
            .file_name()
            .map(|name| name == "Move.toml")
            .unwrap_or(false)
        {
            if let Some(parent) = path.parent() {
                roots.insert(parent.to_path_buf());
            }
        }
    } else {
        for entry in WalkDir::new(path)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| !should_skip_dir(entry))
        {
            let entry = entry?;
            if entry.file_type().is_file()
                && entry.file_name() == "Move.toml"
                && entry.path().parent().is_some()
            {
                roots.insert(entry.path().parent().unwrap().to_path_buf());
            }
        }
    }

    Ok(roots.into_iter().collect())
}

fn should_skip_dir(entry: &DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }

    let name = entry.file_name().to_string_lossy();
    matches!(name.as_ref(), ".git" | "target" | "build" | "node_modules")
}

async fn compile_package(path: &Path) -> Result<CompiledPackage> {
    let build_config = BuildConfig::default();
    let envs = RootPackage::<SuiFlavor>::environments(path)
        .with_context(|| format!("Failed to read environments for {}", path.display()))?;

    let mut last_error = None;

    for (name, id) in envs {
        let env = Environment::new(name.clone(), id.clone());
        let mut sink = std::io::sink();
        match build_config
            .compile_package::<SuiFlavor, _>(path, &env, &mut sink)
            .await
        {
            Ok(compiled) => return Ok(compiled),
            Err(err) => {
                last_error = Some((name, err));
            }
        }
    }

    if let Some((name, err)) = last_error {
        Err(anyhow!(
            "unable to compile package for any environment; last attempt with `{}` failed: {}",
            name,
            err
        ))
    } else {
        Err(anyhow!(
            "no environments available to compile package at {}",
            path.display()
        ))
    }
}

fn collect_modules(compiled: &CompiledPackage) -> Vec<ModuleInfo> {
    let mut modules = Vec::new();

    for unit in compiled.root_modules() {
        let module = &unit.unit.module;
        let mut functions = Vec::new();

        for function_def in module.function_defs() {
            if function_def.visibility != Visibility::Public {
                continue;
            }

            let handle = module.function_handle_at(function_def.function);
            let name = module.identifier_at(handle.name).to_string();
            let type_params = (0..handle.type_parameters.len())
                .map(|idx| format!("T{}", idx))
                .collect();
            let params = module
                .signature_at(handle.parameters)
                .0
                .iter()
                .map(|token| format_signature_token(module, token))
                .collect();
            let returns = module
                .signature_at(handle.return_)
                .0
                .iter()
                .map(|token| format_signature_token(module, token))
                .collect();

            functions.push(FunctionInfo {
                name,
                type_params,
                params,
                returns,
            });
        }

        functions.sort_by(|a, b| a.name.cmp(&b.name));
        modules.push(ModuleInfo {
            name: module.name().to_string(),
            functions,
        });
    }

    modules.sort_by(|a, b| a.name.cmp(&b.name));
    modules
}

fn format_signature_token(module: &CompiledModule, token: &SignatureToken) -> String {
    match token {
        SignatureToken::Bool => "bool".to_string(),
        SignatureToken::U8 => "u8".to_string(),
        SignatureToken::U16 => "u16".to_string(),
        SignatureToken::U32 => "u32".to_string(),
        SignatureToken::U64 => "u64".to_string(),
        SignatureToken::U128 => "u128".to_string(),
        SignatureToken::U256 => "u256".to_string(),
        SignatureToken::Address => "address".to_string(),
        SignatureToken::Signer => "signer".to_string(),
        SignatureToken::Vector(inner) => {
            format!("vector<{}>", format_signature_token(module, inner))
        }
        SignatureToken::Datatype(handle) => format_datatype(module, *handle, &[]),
        SignatureToken::DatatypeInstantiation(inner) => {
            format_datatype(module, inner.0, &inner.1)
        }
        SignatureToken::Reference(inner) => {
            format!("&{}", format_signature_token(module, inner))
        }
        SignatureToken::MutableReference(inner) => {
            format!("&mut {}", format_signature_token(module, inner))
        }
        SignatureToken::TypeParameter(index) => format!("T{}", index),
    }
}

fn format_datatype(
    module: &CompiledModule,
    handle: move_binary_format::file_format::DatatypeHandleIndex,
    type_args: &[SignatureToken],
) -> String {
    let handle = module.datatype_handle_at(handle);
    let module_handle = module.module_handle_at(handle.module);
    let module_name = module.identifier_at(module_handle.name).to_string();
    let type_name = module.identifier_at(handle.name).to_string();
    let is_self = handle.module == module.self_handle_idx();

    let mut name = if is_self {
        type_name
    } else {
        format!("{}::{}", module_name, type_name)
    };

    if !type_args.is_empty() {
        let args = type_args
            .iter()
            .map(|token| format_signature_token(module, token))
            .collect::<Vec<_>>()
            .join(", ");
        name = format!("{}<{}>", name, args);
    }

    name
}

fn print_package_tree(root: &Path, package_path: &Path, name: &str, modules: &[ModuleInfo]) {
    let package_label = "package".bold().blue();
    let package_name = name.bold();
    let mut line = format!("{} {}", package_label, package_name);

    if let Ok(relative) = package_path.strip_prefix(root) {
        if !relative.as_os_str().is_empty() {
            line.push(' ');
            line.push_str(&format!("({})", relative.display()).dimmed().to_string());
        }
    }

    println!("{}", line);

    for (module_index, module) in modules.iter().enumerate() {
        let is_last_module = module_index + 1 == modules.len();
        let module_prefix = if is_last_module { "`-- " } else { "|-- " };
        let module_line = format!(
            "{}{} {}",
            module_prefix,
            "module".cyan().bold(),
            module.name.cyan()
        );
        println!("{}", module_line);

        let child_prefix = if is_last_module { "    " } else { "|   " };
        for (func_index, function) in module.functions.iter().enumerate() {
            let is_last_function = func_index + 1 == module.functions.len();
            let function_prefix = if is_last_function { "`-- " } else { "|-- " };
            let line = format!(
                "{}{}{}",
                child_prefix,
                function_prefix,
                render_function(function)
            );
            println!("{}", line);
        }
    }
}

fn render_function(function: &FunctionInfo) -> String {
    let name = function.name.green().bold();
    let type_params = if function.type_params.is_empty() {
        String::new()
    } else {
        let params = function
            .type_params
            .iter()
            .map(|param| param.yellow().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!("<{}>", params)
    };

    let params = function
        .params
        .iter()
        .map(|param| param.yellow().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let params = format!("({})", params);

    let returns = if function.returns.is_empty() {
        "()".magenta().to_string()
    } else {
        let rendered = function
            .returns
            .iter()
            .map(|ret| ret.magenta().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        if function.returns.len() == 1 {
            rendered
        } else {
            format!("({})", rendered)
        }
    };

    format!(
        "{} {}{}{}: {}",
        "fun".bright_black(),
        name,
        type_params,
        params,
        returns
    )
}
