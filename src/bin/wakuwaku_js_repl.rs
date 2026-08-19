#[cfg(not(target_family = "wasm"))]
#[path = "../js_repl.rs"]
mod js_repl;

/// Run the dedicated stdio transport without initializing the Waku GUI.
#[cfg(not(target_family = "wasm"))]
fn main() {
    if let Err(error) = js_repl::serve_stdio() {
        eprintln!("WakuWakuWaku JavaScript REPL: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(target_family = "wasm")]
fn main() {}
