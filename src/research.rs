//! Research orchestration with shared jagent proposal and source-edit boundaries.
//!
//! Python controllers are embedded; normal shell startup never launches them.
//! Native validation helpers perform no model IO and grant no execution rights.

use jagent::research::author::{EditRequest, MAX_EDIT_REQUEST_BYTES, MAX_EDIT_RESPONSE_BYTES};
use jagent::research::{
    ResearchContext, ResearchProposal, MAX_RESEARCH_CONTEXT_BYTES, MAX_RESEARCH_PROPOSAL_BYTES,
};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

const ENGINE: &str = include_str!("../scripts/jsh_research.py");
const BRIDGE: &str = include_str!("../scripts/jsh_research_bridge.py");
const AUTO: &str = include_str!("../scripts/jsh_research_auto.py");

fn read_json_file(path: &Path, limit: usize) -> Result<Vec<u8>, &'static str> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| "cannot open research JSON file")?;
    let metadata = file.metadata().map_err(|_| "cannot inspect research JSON file")?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err("research JSON must be a bounded regular file");
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes).map_err(|_| "cannot read research JSON")?;
    if bytes.len() > limit { return Err("research JSON byte limit exceeded"); }
    Ok(bytes)
}

fn native(args: &[OsString]) -> Result<serde_json::Value, String> {
    if args[0] == "author-prepare" || args[0] == "author-decode" {
        let input = read_json_file(Path::new(&args[1]), MAX_EDIT_REQUEST_BYTES)?;
        let request = EditRequest::from_json(&input).map_err(|error| error.to_string())?;
        if args[0] == "author-prepare" {
            return request.prepare().map_err(|error| error.to_string());
        }
        let response = read_json_file(Path::new(&args[2]), MAX_EDIT_RESPONSE_BYTES)?;
        return request.parse_response(&response).map(|plan| plan.as_json().clone()).map_err(|error| error.to_string());
    }
    let context_bytes = read_json_file(Path::new(&args[1]), MAX_RESEARCH_CONTEXT_BYTES)?;
    let context = ResearchContext::from_json(&context_bytes).map_err(|error| error.to_string())?;
    if args[0] == "prepare-request" {
        context.prepare_request().map_err(|error| error.to_string())
    } else {
        let proposal_bytes = read_json_file(Path::new(&args[2]), MAX_RESEARCH_PROPOSAL_BYTES)?;
        let proposal = ResearchProposal::from_json(&proposal_bytes, &context).map_err(|error| error.to_string())?;
        Ok(proposal.as_json().clone())
    }
}

fn write_json(value: &serde_json::Value) -> i32 {
    let mut bytes = match serde_json::to_vec(value) { Ok(bytes) => bytes, Err(_) => return 74 };
    bytes.push(b'\n');
    match std::io::stdout().lock().write_all(&bytes) {
        Ok(()) => 0,
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => 0,
        Err(_) => 74,
    }
}

/// Dispatch research commands, without implicit shell evaluation or execution.
pub fn entrypoint() -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(2).collect();
    let helper = args.first().and_then(|arg| arg.to_str());
    if matches!(helper, Some("prepare-request" | "validate-proposal" | "author-prepare" | "author-decode")) {
        let expected = if matches!(helper, Some("prepare-request" | "author-prepare")) { 2 } else { 3 };
        if args.len() != expected {
            eprintln!("jsh research: prepare-request CONTEXT | validate-proposal CONTEXT PROPOSAL | author-prepare INPUT | author-decode INPUT RESPONSE");
            return 2;
        }
        return match native(&args) {
            Ok(value) => write_json(&value),
            Err(error) => {
                let error = serde_json::json!({"protocol": "jagent.research.request/v1", "error": error});
                eprintln!("{error}");
                2
            }
        };
    }
    let binary = match std::env::current_exe() {
        Ok(binary) => binary,
        Err(_) => { eprintln!("jsh research: cannot resolve native validator executable"); return 126; }
    };
    let mut command = Command::new("python3");
    command.args(["-I", "-X", "utf8", "-c", AUTO, "--embedded", ENGINE, BRIDGE]).arg(binary).args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        eprintln!("jsh research: cannot start python3 (requires Python 3.10+): {error}");
        if error.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 }
    }
    #[cfg(not(unix))]
    {
        eprintln!("jsh research: the local executor currently requires Unix");
        2
    }
}
