#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // 这个 build script 是被**宿主**编译器编、在宿主机上跑的，所以 `cfg(target_os = ...)`
    // 与 `cfg(debug_assertions)` 在这里说的都是宿主，不是目标。qcode 的补丁：上游正是按
    // 宿主写的，于是从 macOS（或 Linux）交叉编 Windows 的 release 版时，compile_shaders()
    // 一次都没跑，shaders_bytes.rs 不存在；而 directx_renderer.rs 里的 include! 恰好只在
    // `not(debug_assertions)` 下生效——它生效了、文件却没有，编译以
    //
    //   error: couldn't find file `.../out/shaders_bytes.rs`
    //
    // 结束。那句话一个字都没提到 fxc.exe，看不出真正缺的是什么。
    //
    // 目标的 os 与 profile 要用 cargo 给**目标**设的 CARGO_CFG_*，与宿主无关。
    let target_is_windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    // cargo 的布尔 cfg 是「变量存在即真」，值是空串——实测：dev profile 下
    // `CARGO_CFG_DEBUG_ASSERTIONS=`（存在、为空），release 下这个变量根本不存在。
    // 所以这里判 is_some()，写成 `== Ok("true")` 会把 dev 也当成 release，然后把
    // 「本机能跑的 debug 交叉构建」也一起拦掉。
    let target_debug_assertions = std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some();

    if !target_is_windows || target_debug_assertions {
        // debug 构建在运行期用 D3DCompile 从 HLSL 源码编 shader（见 directx_renderer.rs 的
        // `#[cfg(debug_assertions)]` 分支），不需要预编译产物。
        return;
    }

    shader_compilation::compile_shaders();
}

mod shader_compilation {
    use std::{
        fs,
        io::Write,
        path::{Path, PathBuf},
        process::{self, Command},
    };

    pub fn compile_shaders() {
        let shader_path =
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("src/shaders.hlsl");
        let out_dir = std::env::var("OUT_DIR").unwrap();

        println!("cargo:rerun-if-changed={}", shader_path.display());

        // Check if fxc.exe is available
        let fxc_path = find_fxc_compiler();

        // Define all modules
        let modules = [
            "quad",
            "shadow",
            "path_rasterization",
            "path_sprite",
            "underline",
            "monochrome_sprite",
            "subpixel_sprite",
            "polychrome_sprite",
        ];

        let rust_binding_path = format!("{}/shaders_bytes.rs", out_dir);
        if Path::new(&rust_binding_path).exists() {
            fs::remove_file(&rust_binding_path)
                .expect("Failed to remove existing Rust binding file");
        }
        for module in modules {
            compile_shader_for_module(
                module,
                &out_dir,
                &fxc_path,
                shader_path.to_str().unwrap(),
                &rust_binding_path,
            );
        }
    }

    /// Locate `binary` in the newest installed Windows SDK.
    #[cfg(windows)]
    pub fn find_latest_windows_sdk_binary(
        binary: &str,
    ) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
        let key = windows_registry::LOCAL_MACHINE
            .open("SOFTWARE\\WOW6432Node\\Microsoft\\Microsoft SDKs\\Windows\\v10.0")?;

        let install_folder: String = key.get_string("InstallationFolder")?; // "C:\Program Files (x86)\Windows Kits\10\"
        let install_folder_bin = Path::new(&install_folder).join("bin");

        let mut versions: Vec<_> = std::fs::read_dir(&install_folder_bin)?
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();

        versions.sort_by_key(|s| {
            s.split('.')
                .filter_map(|p| p.parse().ok())
                .collect::<Vec<u32>>()
        });

        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            _ => Err(format!(
                "Unsupported architecture: {}",
                std::env::consts::ARCH
            ))?,
        };

        if let Some(highest_version) = versions.last() {
            return Ok(Some(
                install_folder_bin
                    .join(highest_version)
                    .join(arch)
                    .join(binary),
            ));
        }

        Ok(None)
    }

    /// 非 Windows 宿主上没有 `windows-registry`（它是 host-cfg 的 build-dependency），
    /// 也就无从在 Windows SDK 里找 fxc.exe。返回 None，让调用方给出那条人话报错。
    #[cfg(not(windows))]
    pub fn find_latest_windows_sdk_binary(
        _binary: &str,
    ) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
        Ok(None)
    }

    /// You can set the `GPUI_FXC_PATH` environment variable to specify the path to the fxc.exe compiler.
    fn find_fxc_compiler() -> String {
        // Check environment variable
        if let Ok(path) = std::env::var("GPUI_FXC_PATH")
            && Path::new(&path).exists()
        {
            return path;
        }

        // Try to find in PATH
        // NOTE: This has to be `where.exe` on Windows, not `where`, it must be ended with `.exe`
        if let Ok(output) = std::process::Command::new("where.exe")
            .arg("fxc.exe")
            .output()
            && output.status.success()
        {
            let path = String::from_utf8_lossy(&output.stdout);
            return path.trim().to_string();
        }

        if let Ok(Some(path)) = find_latest_windows_sdk_binary("fxc.exe") {
            return path.to_string_lossy().into_owned();
        }

        // 原来是 panic!。在宿主不是 Windows 的交叉构建里，需要的不是「某个内部断言挂了」，
        // 而是「这条路走不通、为什么、怎么办」：
        //
        // fxc.exe 是 Windows SDK 里的 DirectX 着色器编译器（DXBC / SM 4.1），没有 macOS
        // 或 Linux 的原生版本，也没有开源替代能产出 SM 4.1（dxc 只出 SM 6+ 的 DXIL）。
        // 所以 release 版 Windows 产物必须在 Windows 上编：CI 的 release.yml 用
        // windows-latest 原生编；本机想交叉编就用 dev profile（着色器在运行期编）。
        println!(
            "cargo::error=release 版 Windows 目标需要 fxc.exe（Windows SDK 的 HLSL 编译器），\
             而当前宿主是 {}，它没有原生版本。\n\
             \x20 * 正式产物：在 Windows 上构建，或交给 release.yml 的 windows-latest\n\
             \x20 * 本机调试：用 dev profile 交叉编（着色器在运行期由 D3DCompile 编译）\n\
             \x20 * 若你有一份能跑的 fxc.exe（比如 wine 包装），用 GPUI_FXC_PATH 指过去",
            std::env::var("HOST").unwrap_or_else(|_| "非 Windows".into())
        );
        process::exit(1);
    }

    fn compile_shader_for_module(
        module: &str,
        out_dir: &str,
        fxc_path: &str,
        shader_path: &str,
        rust_binding_path: &str,
    ) {
        // Compile vertex shader
        let output_file = format!("{}/{}_vs.h", out_dir, module);
        let const_name = format!("{}_VERTEX_BYTES", module.to_uppercase());
        compile_shader_impl(
            fxc_path,
            &format!("{module}_vertex"),
            &output_file,
            &const_name,
            shader_path,
            "vs_4_1",
        );
        generate_rust_binding(&const_name, &output_file, rust_binding_path);

        // Compile fragment shader
        let output_file = format!("{}/{}_ps.h", out_dir, module);
        let const_name = format!("{}_FRAGMENT_BYTES", module.to_uppercase());
        compile_shader_impl(
            fxc_path,
            &format!("{module}_fragment"),
            &output_file,
            &const_name,
            shader_path,
            "ps_4_1",
        );
        generate_rust_binding(&const_name, &output_file, rust_binding_path);
    }

    fn compile_shader_impl(
        fxc_path: &str,
        entry_point: &str,
        output_path: &str,
        var_name: &str,
        shader_path: &str,
        target: &str,
    ) {
        let output = Command::new(fxc_path)
            .args([
                "/T",
                target,
                "/E",
                entry_point,
                "/Fh",
                output_path,
                "/Vn",
                var_name,
                "/O3",
                shader_path,
            ])
            .output();

        match output {
            Ok(result) => {
                if result.status.success() {
                    return;
                }
                println!(
                    "cargo::error=Shader compilation failed for {}:\n{}",
                    entry_point,
                    String::from_utf8_lossy(&result.stderr)
                );
                process::exit(1);
            }
            Err(e) => {
                println!("cargo::error=Failed to run fxc for {}: {}", entry_point, e);
                process::exit(1);
            }
        }
    }

    fn generate_rust_binding(const_name: &str, head_file: &str, output_path: &str) {
        let header_content = fs::read_to_string(head_file).expect("Failed to read header file");
        let const_definition = {
            let global_var_start = header_content.find("const BYTE").unwrap();
            let global_var = &header_content[global_var_start..];
            let equal = global_var.find('=').unwrap();
            global_var[equal + 1..].trim()
        };
        let rust_binding = format!(
            "const {}: &[u8] = &{}\n",
            const_name,
            const_definition.replace('{', "[").replace('}', "]")
        );
        let mut options = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(output_path)
            .expect("Failed to open Rust binding file");
        options
            .write_all(rust_binding.as_bytes())
            .expect("Failed to write Rust binding file");
    }
}
