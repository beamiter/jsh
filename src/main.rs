fn main() {
    let first_argument = std::env::args_os().nth(1);
    if first_argument.as_deref() == Some(std::ffi::OsStr::new("research")) {
        std::process::exit(jsh::research::entrypoint());
    }
    std::process::exit(jsh::cli::entrypoint());
}
