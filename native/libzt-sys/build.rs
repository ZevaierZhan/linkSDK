use std::{env, path::PathBuf};
fn main() {
    let root = env::var_os("LINKSDK_NATIVE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../.."));
    println!("cargo:rerun-if-env-changed=LINKSDK_NATIVE_ROOT");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let target = env::var("TARGET").unwrap();
    let dir = env::var_os("LINKSDK_LIBZT_LIB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if target_os == "ios" {
                root.join("vendor/libzt/dist/ios").join(&target)
            } else if target_os == "android" {
                root.join("vendor/libzt/dist/android").join(&target)
            } else {
                root.join("vendor/libzt/dist/native/lib")
            }
        });
    let archive = dir.join(if target_os == "windows" {
        "zt.lib"
    } else {
        "libzt.a"
    });
    assert!(archive.is_file(),"Missing libzt archive: {}. Run node scripts/prepare-libzt.mjs for the host target; cross targets need their own archive.",archive.display());
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=zt");
    println!("cargo:rerun-if-env-changed=LINKSDK_LIBZT_LIB_DIR");
    println!("cargo:rerun-if-changed={}", archive.display());
    let header = root.join("vendor/libzt/include/ZeroTierSockets.h");
    let mut builder = bindgen::Builder::default()
        .header(header.to_str().unwrap())
        .parse_callbacks(Box::new(bindgen::CargoCallbacks))
        .size_t_is_usize(target_os != "windows");
    match target_os.as_str() {
        "macos" => println!("cargo:rustc-link-lib=dylib=c++"),
        "ios" => {
            println!("cargo:rustc-link-lib=dylib=c++");
            let sdk = if target.contains("-sim") || target.starts_with("x86_64") {
                "iphonesimulator"
            } else {
                "iphoneos"
            };
            let output = std::process::Command::new("xcrun")
                .args(["--sdk", sdk, "--show-sdk-path"])
                .output()
                .expect("iOS SDK query");
            assert!(output.status.success(), "Full Xcode iOS SDK required");
            let sysroot = String::from_utf8(output.stdout).unwrap();
            builder = builder
                .clang_arg(format!("--target={target}"))
                .clang_arg(format!("-isysroot{}", sysroot.trim()));
        }
        "linux" => println!("cargo:rustc-link-lib=dylib=stdc++"),
        "windows" => println!("cargo:rustc-link-lib=Iphlpapi"),
        "android" => {
            let ndk = PathBuf::from(
                env::var("NDK_HOME")
                    .or_else(|_| env::var("ANDROID_NDK_HOME"))
                    .expect("NDK_HOME required"),
            );
            let prebuilt = std::fs::read_dir(ndk.join("toolchains/llvm/prebuilt"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let triple = if target.starts_with("aarch64") {
                "aarch64-linux-android"
            } else if target.starts_with("x86_64") {
                "x86_64-linux-android"
            } else {
                panic!("unsupported Android ABI")
            };
            let sysroot = prebuilt.join("sysroot");
            let resources = std::fs::read_dir(prebuilt.join("lib/clang"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            builder = builder
                .clang_arg(format!("--target={triple}26"))
                .clang_arg(format!("--sysroot={}", sysroot.display()))
                .clang_arg(format!("-resource-dir={}", resources.display()));
            let stl = PathBuf::from(env::var("OUT_DIR").unwrap()).join("ndk-stl");
            std::fs::create_dir_all(&stl).unwrap();
            for library in ["libc++_static.a", "libc++abi.a"] {
                std::fs::copy(
                    sysroot.join("usr/lib").join(triple).join(library),
                    stl.join(library),
                )
                .unwrap();
            }
            println!("cargo:rustc-link-search=native={}", stl.display());
            for lib in ["c++_static", "c++abi"] {
                println!("cargo:rustc-link-lib=static={lib}");
            }
            println!("cargo:rustc-link-lib=log");
        }
        _ => panic!("unsupported target OS: {target_os}"),
    }
    builder
        .generate()
        .expect("generate libzt bindings")
        .write_to_file(PathBuf::from(env::var("OUT_DIR").unwrap()).join("libzt.rs"))
        .unwrap();
}
