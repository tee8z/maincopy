#[path = "../git_ssh.rs"]
mod git_ssh;
#[path = "../git_ssh_contract.rs"]
mod git_ssh_contract;

fn main() -> std::process::ExitCode {
    git_ssh::run()
}
