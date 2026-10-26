// Cargo build scripts execute on the host; TARGET selects the SDK and archive.
fn build_ios() {
    let target = env::var("TARGET").unwrap();
    let (sdk, arch, simulator) = match target.as_str() {
        "aarch64-apple-ios" => ("iphoneos", "arm64", false),
        "aarch64-apple-ios-sim" => ("iphonesimulator", "arm64", true),
        "x86_64-apple-ios" => ("iphonesimulator", "x86_64", true),
        _ => panic!("Unsupported iOS target: {}", target),
    };
    fn xcrun(sdk: &str, args: &[&str]) -> String {
        let output = std::process::Command::new("xcrun").args(["--sdk", sdk]).args(args)
            .output().expect("iOS libzt requires full Xcode and xcrun");
        assert!(output.status.success(), "xcrun failed: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).expect("Invalid SDK path").trim().to_owned()
    }
    let sdk_root = xcrun(sdk, &["--show-sdk-path"]);
    let resources = xcrun(sdk, &["clang", "--print-resource-dir"]);
    let deployment = env::var("IPHONEOS_DEPLOYMENT_TARGET").unwrap_or_else(|_| "18.0".into());
    assert!(deployment.split('.').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())), "Invalid IPHONEOS_DEPLOYMENT_TARGET");
    let clang_target = format!("{arch}-apple-ios{deployment}{}", if simulator { "-simulator" } else { "" });
    let source = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let native = source.join("../../../dist/ios").join(&target);
    let library = native.join("libzt.a");
    assert!(library.is_file(), "Run npm run ios:prepare -- {} before building the iOS target", target);
    println!("cargo:rustc-link-search=native={}", native.display());
    println!("cargo:rustc-link-lib=static=zt");
    println!("cargo:rustc-link-lib=dylib=c++");
    println!("cargo:rerun-if-changed={}", library.display());
    println!("cargo:rerun-if-changed={}", native.join("build-revision").display());
    for name in ["TARGET", "DEVELOPER_DIR", "IPHONEOS_DEPLOYMENT_TARGET"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let bindings = bindgen::Builder::default()
        .header("src/include/ZeroTierSockets.h")
        .clang_arg(format!("--target={clang_target}"))
        .clang_args(["-isysroot", sdk_root.as_str()])
        .clang_arg(format!("-resource-dir={resources}"))
        .parse_callbacks(Box::new(bindgen::CargoCallbacks))
        .size_t_is_usize(true)
        .generate().expect("Unable to generate iOS libzt bindings using the selected SDK");
    bindings.write_to_file(PathBuf::from(env::var("OUT_DIR").unwrap()).join("libzt.rs")).unwrap();
}
