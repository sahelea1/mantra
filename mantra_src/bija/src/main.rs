//! Bija — the seed harness. See `SPEC.md` for the contract this crate implements.

mod agent;
mod api;
mod cli;
mod control;
mod proto;
mod session;
mod tools;
mod types;

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let code = rt.block_on(cli::run());
    rt.shutdown_timeout(std::time::Duration::from_millis(500));
    std::process::exit(code);
}
