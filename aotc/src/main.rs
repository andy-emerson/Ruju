//! CLI: `ruju-aotc <fixture.json> <out.wasm> [--wat]`

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (fixture_path, out_path) = match (args.get(1), args.get(2)) {
        (Some(f), Some(o)) => (f, o),
        _ => {
            eprintln!("usage: ruju-aotc <fixture.json> <out.wasm> [--wat]");
            return ExitCode::FAILURE;
        }
    };
    let json = match std::fs::read_to_string(fixture_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ruju-aotc: reading {fixture_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let fx = match ruju_aotc::fixture::Fixture::parse(&json) {
        Ok(fx) => fx,
        Err(e) => {
            eprintln!("ruju-aotc: {fixture_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let bytes = match ruju_aotc::emit::emit_module(&fx) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("ruju-aotc: emit: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = std::fs::write(out_path, &bytes) {
        eprintln!("ruju-aotc: writing {out_path}: {e}");
        return ExitCode::FAILURE;
    }
    if args.iter().any(|a| a == "--wat") {
        match wasmprinter::print_bytes(&bytes) {
            Ok(wat) => println!("{wat}"),
            Err(e) => {
                eprintln!("ruju-aotc: wat: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}
