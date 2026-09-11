use std::env;
use std::path::Path;
use std::process::Command;

/// 比较 a 是否比 b 新（任一文件不存在时给出合理结果）
fn mtime_newer(a: &str, b: &str) -> bool {
    let ma = Path::new(a).metadata().and_then(|m| m.modified()).ok();
    let mb = Path::new(b).metadata().and_then(|m| m.modified()).ok();
    match (ma, mb) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        _ => false,
    }
}

fn main() {
    let rc_path = "assets/app.rc";
    let res_path = "assets/app.res";

    let target = env::var("TARGET").unwrap_or_default();
    let is_gnu = target.ends_with("-windows-gnu");

    if is_gnu {
        // GNU 工具链：用 windres 把 .rc 编译成 COFF 对象后链接（GNU ld 不能直接链接 .res）
        let obj_path = "assets/app_res.o";
        let need_build = !Path::new(obj_path).exists() || mtime_newer(rc_path, obj_path);
        if need_build {
            let candidates = ["x86_64-w64-mingw32-windres", "windres"];
            let mut ok = false;
            for windres in candidates {
                if let Ok(status) = Command::new(windres)
                    .args(["-i", rc_path, "-O", "coff", "-o", obj_path])
                    .status()
                {
                    if status.success() {
                        ok = true;
                        break;
                    }
                }
            }
            if !ok {
                println!("cargo:warning=windres 不可用，GNU 目标将不带图标资源（托盘/窗口图标将缺失）");
            }
        }
        if Path::new(obj_path).exists() {
            let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
            println!("cargo:rustc-link-arg={}/assets/app_res.o", manifest_dir);
        }
    } else {
        // MSVC 流程：优先 rc.exe；失败时保留仓库预置的 app.res，不要用空文件覆盖
        let should_compile = !Path::new(res_path).exists() || mtime_newer(rc_path, res_path);
        if should_compile {
            // 尝试找到并使用 rc.exe
            let rc_result = if let Ok(vs_path) = env::var("VS2022INSTALLDIR") {
                println!("cargo:rerun-if-env-changed=VS2022INSTALLDIR");
                let rc_exe = format!(
                    "{}\\VC\\Tools\\MSVC\\14.XX.XXXXX\\bin\\Hostx64\\x64\\rc.exe",
                    vs_path
                );
                Command::new(&rc_exe)
                    .args(["/fo", res_path, rc_path])
                    .status()
            } else {
                // 尝试直接使用 rc.exe（如果在 PATH 中）
                Command::new("rc")
                    .args(["/fo", res_path, rc_path])
                    .status()
            };

            match rc_result {
                Ok(status) if status.success() => {
                    println!("cargo:warning=Resource file compiled successfully");
                }
                _ => {
                    println!("cargo:warning=Failed to compile resource file with rc.exe");
                    println!("cargo:warning=You may need to run from a Visual Studio Developer Command Prompt");
                    if Path::new(res_path).exists() {
                        println!("cargo:warning=保留仓库预置的 assets/app.res 继续使用");
                    } else {
                        // 没有预置资源时才创建空占位文件
                        let _ = std::fs::write(res_path, b"");
                    }
                }
            }
        }

        // 链接资源文件（如果存在）
        if Path::new(res_path).exists() {
            println!("cargo:rustc-link-arg=assets/app.res");
        }
    }

    // 当资源文件改变时重新编译
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/app_active.ico");
}
