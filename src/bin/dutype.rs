//! `dutype` is the pre-rename name of diskbytype, kept as a compatibility
//! alias: the very same program under the old command.
fn main() -> std::process::ExitCode {
    diskbytype::run_cli()
}
