use std::env;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let output_file = format!("{}/include/ai_cli_session_db.h", crate_dir);

    // 生成 C header
    cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_language(cbindgen::Language::C)
        .with_include_guard("CLAUDE_SESSION_DB_H")
        .with_pragma_once(true)
        .with_include("stdint.h")
        .with_include("stdbool.h")
        .with_include("stddef.h")
        .with_style(cbindgen::Style::Both)
        .with_documentation(true)
        .with_tab_width(4)
        .exclude_item("std")
        .exclude_item("parking_lot")
        .exclude_item("rusqlite")
        .exclude_item("tokio")
        .generate()
        .expect("Unable to generate bindings")
        .write_to_file(output_file);
}
