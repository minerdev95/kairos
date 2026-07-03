//! Build script — best-effort embedding of the KAIROS icon into the Windows
//! executable. If no resource compiler (rc.exe / llvm-rc) is available, this is
//! a silent no-op and the build still succeeds; the branded launcher created by
//! `scripts/install-windows.ps1` provides the icon regardless.

fn main() {
    println!("cargo:rerun-if-changed=assets/kairos.ico");

    // Bake the private developer overlay (dev/dev.toml) into the binary if it is
    // present in THIS build environment. The public/open source tree has no such
    // file, so nothing is baked; the project owner's build embeds their dev-fee
    // addresses + telemetry endpoint so a shipped binary can't have the fee edited
    // away via a config file. (Client-side fees are never fully un-bypassable, but
    // this matches how commercial miners protect a disclosed fee.)
    bake_dev_config();

    // GPU backend: compile KAIROS's own CUDA kernels with nvcc, but ONLY when the
    // `gpu` feature is enabled. The default build never invokes nvcc, so it has no
    // CUDA dependency and stays fully portable.
    if std::env::var("CARGO_FEATURE_GPU").is_ok() {
        compile_cuda();
    }

    #[cfg(windows)]
    {
        if std::path::Path::new("assets/kairos.ico").exists() {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/kairos.ico");
            res.set("ProductName", "KAIROS");
            res.set("FileDescription", "KAIROS — intelligent mining control plane");
            res.set("CompanyName", "KAIROS");
            // Ignore failures (e.g. no Windows SDK rc.exe) so the build never breaks.
            let _ = res.compile();
        }
    }
}

/// Embed `dev/dev.toml` into the binary **obfuscated** (or `None` if absent).
/// Written to `$OUT_DIR/dev_baked.rs` and `include!`d by `devconfig`.
///
/// The TOML is XOR-masked with an xorshift keystream so the payout addresses do
/// NOT appear as plaintext in the shipped binary — you can't `grep` the exe for
/// `kaspa:`/a BTC address, and a naive hex-editor swap of the address won't work.
/// (A determined reverse-engineer can still recover it — client-side protection is
/// never absolute — but this defeats casual tampering and address-scraping.)
fn bake_dev_config() {
    use std::path::PathBuf;
    println!("cargo:rerun-if-changed=dev/dev.toml");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("dev_baked.rs");
    let baked = match std::fs::read_to_string("dev/dev.toml") {
        // Only bake a dev.toml the owner has actually filled in — an untouched
        // template (still carrying the placeholder marker) bakes nothing, so a
        // demo/public build stays clean.
        Ok(toml) if !toml.contains("PASTE_OUTPUT") => Some(toml),
        _ => None,
    };
    let code = match baked {
        Some(toml) => {
            let key: u64 = 0x9E37_79B9_7F4A_7C15;
            let mut st = key;
            let bytes: Vec<u8> = toml
                .bytes()
                .map(|b| {
                    st ^= st << 13;
                    st ^= st >> 7;
                    st ^= st << 17;
                    b ^ (st as u8)
                })
                .collect();
            let list = bytes.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(",");
            format!(
                "pub const BAKED_KEY: u64 = {key};\npub const BAKED_OBF: Option<&[u8]> = Some(&[{list}]);\n"
            )
        }
        None => "pub const BAKED_KEY: u64 = 0;\npub const BAKED_OBF: Option<&[u8]> = None;\n".to_string(),
    };
    let _ = std::fs::write(out, code);
}

/// Compile `src/gpu/kairos_kernels.cu` into a static lib with `nvcc` and link it.
/// Only called when `--features gpu` is set. Requires the CUDA toolkit on PATH.
fn compile_cuda() {
    use std::path::PathBuf;
    use std::process::Command;
    println!("cargo:rerun-if-changed=src/gpu/kairos_kernels.cu");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let obj = out.join("kairos_kernels.o");
    let lib = out.join("libkairos_gpu.a");
    let nvcc = std::env::var("NVCC").unwrap_or_else(|_| "nvcc".to_string());

    let run = Command::new(&nvcc)
        .args(["-O3", "-c", "src/gpu/kairos_kernels.cu", "-o"])
        .arg(&obj)
        .arg("-Xcompiler")
        .arg(if cfg!(windows) { "/MD" } else { "-fPIC" })
        .status();
    let ok = matches!(&run, Ok(s) if s.success());
    if !ok {
        // nvcc missing, or present but its host compiler (cl.exe/gcc) isn't on
        // PATH. Skip compiling/linking the kernels rather than break the build —
        // `cargo check` still type-checks the FFI Rust. A real GPU build needs the
        // full CUDA toolchain (run nvcc from an environment where cl.exe/gcc is on
        // PATH, e.g. a VS Developer prompt on Windows).
        println!("cargo:warning=GPU kernels not compiled (nvcc + host compiler required for --features gpu); CPU engine unaffected");
        return;
    }

    // Archive the object into a static lib the linker can consume.
    let ar = if cfg!(windows) { "lib" } else { "ar" };
    if cfg!(windows) {
        let _ = std::fs::remove_file(&lib);
        let status = Command::new(ar)
            .arg(format!("/OUT:{}", lib.display()))
            .arg(&obj)
            .status()
            .expect("failed to run lib.exe to archive CUDA object");
        assert!(status.success(), "lib.exe failed");
    } else {
        let status = Command::new(ar)
            .arg("crus")
            .arg(&lib)
            .arg(&obj)
            .status()
            .expect("failed to run ar to archive CUDA object");
        assert!(status.success(), "ar failed");
    }

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=kairos_gpu");
    println!("cargo:rustc-link-lib=dylib=cudart");
    // CUDA runtime lib search path (typical install locations).
    if let Ok(cuda_path) = std::env::var("CUDA_PATH") {
        if cfg!(windows) {
            println!("cargo:rustc-link-search=native={}\\lib\\x64", cuda_path);
        } else {
            println!("cargo:rustc-link-search=native={}/lib64", cuda_path);
        }
    }
}
