//! Print which shell a new terminal would run, and why.
//!
//! Useful when a user reports "the terminal opens in the wrong shell": this shows
//! which layer answered and what the parent chain looked like, without a debugger.
//!
//!     cargo run -p qcode-terminal --example detect-shell

use qcode_terminal::shell;

fn main() {
    let choice = shell::detect();
    println!("shell:  {}", choice.program.display());
    println!("source: {}", choice.source.label());

    let me = std::process::id();
    println!("\nparent chain:");
    let mut pid = me;
    for _ in 0..12 {
        let Some(row) = shell::system_process_row(pid) else {
            println!("  {pid}: <not readable>");
            break;
        };
        let exe = row
            .exe
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<exe not readable>".to_string());
        let marker = if pid == me { " (self)" } else { "" };
        println!(
            "  {:>7} <- {:>7}  {:<16} {exe}{marker}",
            row.pid, row.ppid, row.comm
        );
        if row.ppid == 0 || row.ppid == pid {
            break;
        }
        pid = row.ppid;
    }
}
