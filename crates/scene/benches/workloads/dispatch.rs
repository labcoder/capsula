use std::process::ExitCode;

pub(super) fn run(workloads: &[(&str, fn())]) -> ExitCode {
    let mut args = std::env::args().skip(1);
    let name = args.next();
    if args.next().is_none() {
        match name.as_deref() {
            None | Some("--list") => {
                for (name, _) in workloads {
                    println!("{name}");
                }
                return ExitCode::SUCCESS;
            }
            Some(name) => {
                if let Some((_, run)) = workloads.iter().find(|(key, _)| *key == name) {
                    run();
                    return ExitCode::SUCCESS;
                }
            }
        }
    }
    eprintln!("pass one workload name or --list");
    ExitCode::FAILURE
}
