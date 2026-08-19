#[cfg(not(target_family = "wasm"))]
fn main() {
    wakuwaku::run();
}

#[cfg(target_family = "wasm")]
fn main() {}
