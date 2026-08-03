pub mod connection;
pub mod defmt;
pub mod logger;
pub mod mantra;
pub mod probe;

use std::path::PathBuf;

pub use embsinth_procm::*;
use mantra_schema::time::OffsetDateTime;

#[derive(Debug, Clone, clap::Parser)]
pub enum Cmd {
    PostProcess(PostProcess),
}

impl Cmd {
    pub fn run(self) -> Result<(), anyhow::Error> {
        match self {
            Cmd::PostProcess(args) => {
                let test_run_schema = crate::mantra::process(
                    &args.logs_dir,
                    args.test_run_name,
                    args.test_run_date
                        .unwrap_or(mantra_schema::time::OffsetDateTime::now_utc()),
                )?;

                let content = serde_json::to_string_pretty(&test_run_schema)?;
                std::fs::write(args.out, content)?;
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, clap::Args)]
pub struct PostProcess {
    logs_dir: PathBuf,
    #[clap(long)]
    out: PathBuf,
    #[clap(long)]
    test_run_name: String,
    #[clap(long, value_parser = mantra_schema::test_runs::test_date_from_str)]
    test_run_date: Option<OffsetDateTime>,
}
