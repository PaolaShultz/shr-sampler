use std::process::ExitCode;

fn main() -> ExitCode {
    match shr_sampler::run(std::env::args().skip(1).collect()) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}\n\n{}", shr_sampler::USAGE);
            ExitCode::FAILURE
        }
    }
}
