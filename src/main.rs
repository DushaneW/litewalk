mod cli;

use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let code = cli::run(&argv, &mut out, &mut io::stderr());
    let _ = out.flush();
    ExitCode::from(code as u8)
}
