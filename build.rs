//! 把岛仔图标作为 RT_GROUP_ICON #1 编进 exe，供 LoadImageW(hInst, 1, ...) 取用。
//! 不引入额外依赖：Windows 上调用平台资源编译器（MSVC 用 rc.exe，GNU 用 windres），
//! 把 .res 交给链接器。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// MSVC 目标：在 Windows SDK 里找最新的 rc.exe。
fn find_rc() -> Option<PathBuf> {
    let roots = [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ];
    let mut found: Vec<PathBuf> = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else { continue };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let rc = dir.join("x64").join("rc.exe");
            if rc.exists() {
                found.push(rc);
            }
        }
    }
    found.sort();
    found.pop()
}

/// GNU 目标：优先用 rustc 同目录工具链里的 windres，其次在 PATH 中查找。
fn find_windres() -> Option<PathBuf> {
    // rustc 的兄弟目录里常带 mingw 的 windres。
    if let Ok(rustc) = env::var("RUSTC") {
        if let Some(dir) = Path::new(&rustc).parent() {
            for name in ["windres.exe", "x86_64-w64-mingw32-windres.exe"] {
                let p = dir.join(name);
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }
    for dir in env::split_paths(&env::var("PATH").unwrap_or_default()) {
        for name in ["windres.exe", "x86_64-w64-mingw32-windres.exe"] {
            let p = dir.join(name);
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets/daozai.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("daozai.res");
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    let ok = if msvc {
        let Some(rc) = find_rc() else {
            println!("cargo:warning=rc.exe 未找到，跳过图标资源");
            return;
        };
        Command::new(&rc)
            .arg("/nologo")
            .arg("/fo")
            .arg(&out)
            .arg("app.rc")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        let Some(windres) = find_windres() else {
            println!("cargo:warning=windres 未找到，跳过图标资源");
            return;
        };
        Command::new(&windres)
            .arg("-I")
            .arg(".")
            .arg("-O")
            .arg("coff")
            .arg("-i")
            .arg("app.rc")
            .arg("-o")
            .arg(&out)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !ok {
        println!("cargo:warning=编译 app.rc 失败");
        return;
    }
    println!("cargo:rustc-link-arg={}", out.display());
}
