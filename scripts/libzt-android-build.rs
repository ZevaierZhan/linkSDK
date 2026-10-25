fn build_android() {
    let target = env::var("TARGET").unwrap();
    let triple = if target.starts_with("aarch64") { "aarch64-linux-android" } else if target.starts_with("x86_64") { "x86_64-linux-android" } else { panic!("Unsupported Android ABI: {target}") };
    let ndk = PathBuf::from(env::var("NDK_HOME").or_else(|_| env::var("ANDROID_NDK_HOME")).expect("Set NDK_HOME"));
    let prebuilt = ndk.join("toolchains/llvm/prebuilt");
    let host = std::fs::read_dir(prebuilt).unwrap().next().unwrap().unwrap().path();
    let sysroot = host.join("sysroot");
    let clang_resources = std::fs::read_dir(host.join("lib/clang")).unwrap().next().unwrap().unwrap().path();
    let source = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let native = source.join("../../../dist/android").join(&target);
    assert!(native.join("libzt.a").exists(), "Run npm run android:prepare before building {target}");
    println!("cargo:rustc-link-search=native={}", native.display());
    // Only expose the two STL archives. Adding the whole NDK ABI directory
    // makes -lc find libc.a before the API-specific libc.so and embeds a second,
    // uninitialized Bionic runtime in our shared library.
    let stl = PathBuf::from(env::var("OUT_DIR").unwrap()).join("ndk-stl");
    std::fs::create_dir_all(&stl).unwrap();
    for library in ["libc++_static.a", "libc++abi.a"] {
        let source = sysroot.join("usr/lib").join(triple).join(library);
        std::fs::copy(&source, stl.join(library)).unwrap();
        println!("cargo:rerun-if-changed={}", source.display());
    }
    println!("cargo:rustc-link-search=native={}", stl.display());
    for lib in ["zt", "c++_static", "c++abi"] { println!("cargo:rustc-link-lib=static={lib}"); }
    println!("cargo:rustc-link-lib=log");
    println!("cargo:rerun-if-changed={}", native.join("libzt.a").display());
    println!("cargo:rerun-if-env-changed=NDK_HOME");
    let bindings = bindgen::Builder::default()
        .header("src/include/ZeroTierSockets.h")
        .clang_arg(format!("--target={triple}26"))
        .clang_arg(format!("--sysroot={}", sysroot.display()))
        .clang_arg(format!("-resource-dir={}", clang_resources.display()))
        .parse_callbacks(Box::new(bindgen::CargoCallbacks))
        .size_t_is_usize(true)
        .generate().expect("Unable to generate Android libzt bindings");
    bindings.write_to_file(PathBuf::from(env::var("OUT_DIR").unwrap()).join("libzt.rs")).unwrap();
}
