use clap::Parser;
use embsinth::Cmd;

pub fn main() {
    let cli = Cmd::parse();

    let res = cli.run();

    if let Err(err) = res {
        panic!("Failed post processing!\nCause: {err:?}");
    }
}
