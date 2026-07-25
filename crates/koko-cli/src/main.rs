fn main() -> std::process::ExitCode {
    std::panic::set_hook(Box::new(|_| {
        eprintln!("koko: internal failure; this is a Koko bug.");
    }));
    std::process::ExitCode::from(koko_cli::run_process(std::env::args_os()).code())
}
