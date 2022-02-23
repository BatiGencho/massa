fn main() {
    // Build also the wasm files massa-execution if test
    let _ = std::process::Command::new("yarn")
        .current_dir("./src/tests/wasm_tests")
        .args(&["install"])
        .status();
    let _ = std::process::Command::new("yarn")
        .current_dir("./src/tests/wasm_tests")
        .args(&["build"])
        .status();
}
