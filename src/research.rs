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
const REMOTE: &str = include_str!("../scripts/jsh_research_remote.py");
const AUTO: &str = include_str!("../scripts/jsh_research_auto.py");
const HELP: &str = concat!(
    "jsh research — bounded research orchestration\n\n",
    "Controller: jsh research [--root DIR] COMMAND [OPTIONS]\n",
    "  init MANIFEST                  Freeze a contract and queue the baseline\n",
    "  run --execute [--limit N]       Evaluate queued experiments\n",
    "  status | export | bridge       Inspect state or bounded agent context\n",
    "  enqueue PROPOSAL | submit PROPOSAL\n",
    "  stop | resume | recover        Stop or explicitly recover the controller\n",
    "  retry TRIAL --request-id ID     Queue a new, explicit attempt\n\n",
    "Model loop: jsh research [--root DIR] COMMAND [OPTIONS]\n",
    "  auto-init CONFIG [--allow-remote]  Freeze model, selected files and budgets\n",
    "  auto-run --execute [--limit N]     Author, commit and evaluate candidates\n",
    "  auto-status | auto-recover        Inspect or recover without HTTP replay\n",
    "  auto-preflight                    Check sources/validator offline\n",
    "  auto-report [--recent N]          Inspect budgets and paired-seed outcomes\n\n",
    "Remote staging: remote-init CONFIG | remote-ledger\n",
    "  remote-submit TASK TRIAL --stage STAGE --seed N --execute\n",
    "  remote-status | remote-result | remote-cancel | remote-recover TASK\n\n",
    "Native jagent helpers (no network IO or execution authorization):\n",
    "  jsh research prepare-request CONTEXT\n",
    "  jsh research validate-proposal CONTEXT PROPOSAL\n",
    "  jsh research author-prepare INPUT\n",
    "  jsh research author-decode INPUT RESPONSE\n\n",
    "Use COMMAND --help for controller/model-loop options. Local execution is\n",
    "not a security sandbox. Review docs/autoresearch-auto.md before --execute.\n",
);

fn read_json_file(path: &Path, limit: usize) -> Result<Vec<u8>, &'static str> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| "cannot open research JSON file")?;
    let metadata = file
        .metadata()
        .map_err(|_| "cannot inspect research JSON file")?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err("research JSON must be a bounded regular file");
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read research JSON")?;
    if bytes.len() > limit {
        return Err("research JSON byte limit exceeded");
    }
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
        return request
            .parse_response(&response)
            .map(|plan| plan.as_json().clone())
            .map_err(|error| error.to_string());
    }
    let context_bytes = read_json_file(Path::new(&args[1]), MAX_RESEARCH_CONTEXT_BYTES)?;
    let context = ResearchContext::from_json(&context_bytes).map_err(|error| error.to_string())?;
    if args[0] == "prepare-request" {
        context.prepare_request().map_err(|error| error.to_string())
    } else {
        let proposal_bytes = read_json_file(Path::new(&args[2]), MAX_RESEARCH_PROPOSAL_BYTES)?;
        let proposal = ResearchProposal::from_json(&proposal_bytes, &context)
            .map_err(|error| error.to_string())?;
        Ok(proposal.as_json().clone())
    }
}

fn write_bytes(bytes: &[u8]) -> i32 {
    match std::io::stdout().lock().write_all(bytes) {
        Ok(()) => 0,
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => 0,
        Err(_) => 74,
    }
}

fn write_json(value: &serde_json::Value) -> i32 {
    let mut bytes = match serde_json::to_vec(value) {
        Ok(bytes) => bytes,
        Err(_) => return 74,
    };
    bytes.push(b'\n');
    write_bytes(&bytes)
}

/// Dispatch research commands, without implicit shell evaluation or execution.
pub fn entrypoint() -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(2).collect();
    let helper = args.first().and_then(|arg| arg.to_str());
    if matches!(helper, Some("--help" | "-h")) && args.len() == 1 {
        return write_bytes(HELP.as_bytes());
    }
    if matches!(
        helper,
        Some("prepare-request" | "validate-proposal" | "author-prepare" | "author-decode")
    ) {
        let expected = if matches!(helper, Some("prepare-request" | "author-prepare")) {
            2
        } else {
            3
        };
        if args.len() != expected {
            eprintln!("jsh research: see 'jsh research --help' for native helper arguments");
            return 2;
        }
        return match native(&args) {
            Ok(value) => write_json(&value),
            Err(error) => {
                let error =
                    serde_json::json!({"protocol": "jagent.research.request/v1", "error": error});
                eprintln!("{error}");
                2
            }
        };
    }
    let binary = match std::env::current_exe() {
        Ok(binary) => binary,
        Err(_) => {
            eprintln!("jsh research: cannot resolve native validator executable");
            return 126;
        }
    };
    let mut command = Command::new("python3");
    let controller = if args.first().is_some_and(|arg| arg == "--root") {
        args.get(2)
    } else if args
        .first()
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| arg.starts_with("--root="))
    {
        args.get(1)
    } else {
        args.first()
    };
    let remote = controller
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| arg.starts_with("remote-"));
    if remote {
        command.args([
            "-I",
            "-X",
            "utf8",
            "-c",
            REMOTE,
            "--embedded",
            ENGINE,
            BRIDGE,
        ]);
    } else {
        command
            .args(["-I", "-X", "utf8", "-c", AUTO, "--embedded", ENGINE, BRIDGE])
            .arg(binary);
    }
    command.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        eprintln!("jsh research: cannot start python3 (requires Python 3.10+): {error}");
        if error.kind() == std::io::ErrorKind::NotFound {
            127
        } else {
            126
        }
    }
    #[cfg(not(unix))]
    {
        eprintln!("jsh research: the local executor currently requires Unix");
        2
    }
}
