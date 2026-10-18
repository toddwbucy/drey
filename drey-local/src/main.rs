use drey_local::{Client, Request};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("serve") if args.len() == 4 => drey_local::serve(&args[2], &args[3])?,
        Some("call") if args.len() == 4 => {
            let request: Request = serde_json::from_slice(&std::fs::read(&args[3])?)?;
            let response = Client::connect(&args[2])?.call(request)?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            if response.result.is_err() {
                std::process::exit(1);
            }
        }
        _ => {
            return Err(
                "usage: drey-local serve DATABASE SOCKET | drey-local call SOCKET REQUEST.json"
                    .into(),
            )
        }
    }
    Ok(())
}
