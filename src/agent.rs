//! Interactive review-first AI agent built on the shared `jagent` core.
//!
//! The model may only propose commands; every proposal goes through an
//! explicit approval prompt before jsh executes it. Approved commands run in a
//! fresh, one-shot jsh child through the normal parser/executor, with
//! stdout+stderr teed to the terminal and captured as the bounded observation
//! for the next model turn.
//!
//! Configuration reuses the `JSH_AI_*` environment contract from `crate::ai`,
//! plus:
//! - `JSH_AGENT_MAX_TURNS` — model-turn budget (default 16)
//! - `JSH_AGENT_PROTOCOL` — explicit `text`/`native-tools` request protocol
//! - `JSH_AGENT_PEER_CAPABILITIES` — strict bounded jagent capability token
//! - `JSH_AGENT_AUTO_APPROVE_READONLY` — retired compatibility switch; when
//!   set, jsh warns and continues to require explicit approval

use crate::ai::AiConfig;
use crate::environment::ShellState;
use jagent::provider::{ChatConfig, HttpRequest, Message, Provider};
use jagent::{
    agent_capabilities_for_peer, prepare_agent_request, AgentCapabilities, AgentDelivery,
    AgentProtocol, AgentRequestSpec, AgentResponse, AgentSession, AgentState, ApprovedCommand,
    CapabilityError, CommandExecutionFailure, CommandExecutionOutcome, EnvironmentMeta, GitMeta,
    ModelOutcome, Role, SessionError,
};
use std::fs::{self, File};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DEFAULT_MAX_TURNS: u32 = 16;
const AGENT_MAX_TOKENS: u32 = 1024;
/// Provider envelopes contain metadata around jagent's 256 KiB assistant-text
/// ceiling. One MiB leaves ample framing room without accepting ureq's much
/// larger generic body default on this command-execution surface.
const MAX_AGENT_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_AGENT_DISPLAY_BYTES: usize = 16 * 1024;
/// A validated jagent command is at most 16 KiB. Escaping every ambiguous
/// Unicode format character can expand it, so keep a separate lossless review
/// budget rather than truncating the exact command the user must approve.
const MAX_AGENT_COMMAND_DISPLAY_BYTES: usize = 64 * 1024;
/// Collect at most this much execution output; `AgentSession::observe` samples
/// it further down to its own observation budget.
const MAX_CAPTURED_OUTPUT_BYTES: usize = 128 * 1024;
/// Once the direct jsh child has exited, only pipe-buffered output belongs to
/// that execution. A detached descendant may keep or continuously write the
/// inherited descriptor; cap the final drain so it cannot pin the Agent loop.
const MAX_POST_EXIT_DRAIN_BYTES: usize = 1024 * 1024;
const MAX_CONSECUTIVE_PROTOCOL_RETRIES: u32 = 2;
const MAX_AGENT_SESSION_TURNS: u32 = 1_000;
const AGENT_PROTOCOL_ENV: &str = "JSH_AGENT_PROTOCOL";
const AGENT_PEER_CAPABILITIES_ENV: &str = "JSH_AGENT_PEER_CAPABILITIES";
/// A peer that predates capability discovery can only be assumed to understand
/// the historical JSON-in-text, complete-response path.
const LEGACY_AGENT_PEER_CAPABILITIES: &str = "jagent-agent/1;protocols=text;delivery=complete";
const INTERNAL_AGENT_CHILD_FLAG: &str = "--jsh-internal-agent-child";
const AGENT_CHILD_SESSION_ID: &str = "agent-child";
const AGENT_CHILD_STATE_DIR_ENV: &str = "JSH_AGENT_CHILD_STATE_DIR";
const AGENT_CHILD_CWD_ENV: &str = "JSH_AGENT_CHILD_CWD";
const AGENT_CHILD_COMMAND_ENV: &str = "JSH_AGENT_CHILD_COMMAND";
const AGENT_CHILD_CONTROL_FD_ENV: &str = "JSH_AGENT_CHILD_CONTROL_FD";
const AGENT_CHILD_CWD_REPORT_FD_ENV: &str = "JSH_AGENT_CHILD_CWD_REPORT_FD";
const AGENT_CHILD_CWD_NONCE_FD_ENV: &str = "JSH_AGENT_CHILD_CWD_NONCE_FD";
const AGENT_CHILD_CLAIM_DIR: &str = "claimed";
const AGENT_CHILD_READY: u8 = b'R';
const AGENT_CHILD_CWD_FRAME_MAGIC: [u8; 4] = *b"JCW1";
const AGENT_CHILD_CWD_NONCE_BYTES: usize = 32;
const AGENT_CHILD_CWD_FRAME_HEADER_BYTES: usize = 8 + AGENT_CHILD_CWD_NONCE_BYTES;
const MAX_AGENT_CHILD_CWD_BYTES: usize = 64 * 1024;
const MAX_AGENT_CHILD_CWD_FRAME_BYTES: usize =
    AGENT_CHILD_CWD_FRAME_HEADER_BYTES + MAX_AGENT_CHILD_CWD_BYTES;
const MAX_AGENT_CHILD_DESCRIPTOR_TEXT_BYTES: usize = 10;
const MAX_AGENT_CHILD_READINESS_DRAIN_BYTES: usize = 64;
const MAX_AGENT_CHILD_CWD_DRAIN_BYTES: usize = 64 * 1024;
static AGENT_CHILD_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Argument that turns this binary into a one-shot HTTP transport for its own
/// parent. See [`model_request`].
const INTERNAL_MODEL_REQUEST_FLAG: &str = "--jsh-internal-model-request";
/// Ceiling on the request envelope the parent writes to the child's stdin.
/// A chat request carries the system prompt, the transcript and the observation
/// sample, all of which jagent already bounds; this is the framing backstop.
const MAX_MODEL_REQUEST_ENVELOPE_BYTES: usize = 8 * 1024 * 1024;
/// Wall clock for one model request, including process startup. Comfortably
/// above the transport's own read timeout so the deadline is the outer bound
/// rather than the usual one.
const MODEL_REQUEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);
/// The child answers on stdout with one status byte and then the payload, so a
/// provider error is distinguishable from a transport error without parsing
/// prose. stderr stays empty and is only captured to keep a stuck child from
/// blocking on a full pipe.
const MODEL_CHILD_OK: u8 = b'+';
const MODEL_CHILD_ERR: u8 = b'-';
const MAX_MODEL_CHILD_STDERR_BYTES: usize = 8 * 1024;

/// The one-shot child now carries jagent's public typed result end to end.
/// Keeping a local name documents the process boundary without maintaining a
/// compatibility enum or duplicating the session-ingestion match.
type CapturedExecution = CommandExecutionOutcome;

/// Child-owned writer for the one-byte command-readiness channel.
///
/// The parent arranges for one pipe writer to survive only the exec that
/// starts the one-shot jsh. This constructor immediately restores CLOEXEC and
/// validates that the inherited descriptor is a write-only pipe. Signalling
/// readiness consumes the writer, so no user command can inherit it.
struct AgentChildControl(File);

impl AgentChildControl {
    fn from_env() -> std::io::Result<Self> {
        inherited_agent_child_pipe_writer(AGENT_CHILD_CONTROL_FD_ENV, "control", &[], &[]).map(Self)
    }

    fn signal_ready(mut self) -> std::io::Result<()> {
        self.0.write_all(&[AGENT_CHILD_READY])?;
        self.0.flush()
    }
}

/// Child-owned writer for the final cwd frame. It survives only the exec that
/// starts the one-shot jsh; taking it restores CLOEXEC before any approved
/// command can spawn an external process.
///
/// This deliberately keeps a raw descriptor rather than a `File`. An approved
/// command can run a persistent shell builtin such as `exec 9>&-` in this same
/// process. If it closes the report descriptor behind a Rust I/O owner, that
/// owner's destructor aborts on the resulting I/O-safety violation. Keeping
/// the descriptor raw lets the final report fail closed instead. The original
/// FIFO identity and CLOEXEC bit are revalidated before any write or close, so
/// a descriptor that the command closed and reused is never treated as ours.
struct AgentChildCwdReport {
    descriptor: std::os::fd::RawFd,
    identity: AgentChildPipeIdentity,
}

impl AgentChildCwdReport {
    fn from_env(
        control_descriptor: i32,
        control_identity: AgentChildPipeIdentity,
    ) -> std::io::Result<Self> {
        use std::os::fd::{AsRawFd, IntoRawFd};

        let writer = inherited_agent_child_pipe_writer(
            AGENT_CHILD_CWD_REPORT_FD_ENV,
            "cwd report",
            &[control_descriptor],
            &[control_identity],
        )?;
        let descriptor = writer.as_raw_fd();
        let identity = agent_child_pipe_identity(descriptor)?;
        Ok(Self {
            descriptor: writer.into_raw_fd(),
            identity,
        })
    }

    fn finish(self, cwd: &Path, nonce: &[u8; AGENT_CHILD_CWD_NONCE_BYTES]) -> std::io::Result<()> {
        let result = (|| {
            self.validate_identity()?;
            let frame = encode_agent_child_cwd_frame(cwd, nonce)?;
            write_all_agent_child_pipe(self.descriptor, &frame)
        })();
        // There is no `Drop` close: after an approved command, this number may
        // name no descriptor or a completely unrelated one. Close only while
        // it still identifies the inherited CLOEXEC FIFO writer.
        if self.validate_identity().is_ok() {
            unsafe {
                nix::libc::close(self.descriptor);
            }
        }
        result
    }

    fn validate_identity(&self) -> std::io::Result<()> {
        let descriptor_flags = unsafe { nix::libc::fcntl(self.descriptor, nix::libc::F_GETFD) };
        let status_flags = unsafe { nix::libc::fcntl(self.descriptor, nix::libc::F_GETFL) };
        let metadata = agent_child_pipe_metadata(self.descriptor)?;
        if descriptor_flags < 0
            || descriptor_flags & nix::libc::FD_CLOEXEC == 0
            || status_flags < 0
            || status_flags & nix::libc::O_ACCMODE != nix::libc::O_WRONLY
            || metadata.st_mode & nix::libc::S_IFMT != nix::libc::S_IFIFO
            || AgentChildPipeIdentity::from_metadata(&metadata) != self.identity
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Agent child cwd report descriptor was closed, replaced, or modified",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AgentChildPipeIdentity {
    device: nix::libc::dev_t,
    inode: nix::libc::ino_t,
}

impl AgentChildPipeIdentity {
    const fn from_metadata(metadata: &nix::libc::stat) -> Self {
        Self {
            device: metadata.st_dev,
            inode: metadata.st_ino,
        }
    }
}

fn agent_child_pipe_metadata(descriptor: std::os::fd::RawFd) -> std::io::Result<nix::libc::stat> {
    let mut metadata = std::mem::MaybeUninit::<nix::libc::stat>::zeroed();
    if unsafe { nix::libc::fstat(descriptor, metadata.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fstat` returned success and initialized the full structure.
    Ok(unsafe { metadata.assume_init() })
}

fn agent_child_pipe_identity(
    descriptor: std::os::fd::RawFd,
) -> std::io::Result<AgentChildPipeIdentity> {
    agent_child_pipe_metadata(descriptor)
        .map(|metadata| AgentChildPipeIdentity::from_metadata(&metadata))
}

fn agent_child_pipe_identities_are_distinct(identities: &[AgentChildPipeIdentity]) -> bool {
    identities.iter().enumerate().all(|(index, identity)| {
        !identities[..index]
            .iter()
            .any(|previous| previous == identity)
    })
}

fn write_all_agent_child_pipe(
    descriptor: std::os::fd::RawFd,
    mut bytes: &[u8],
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let written = unsafe {
            nix::libc::write(
                descriptor,
                bytes.as_ptr().cast::<nix::libc::c_void>(),
                bytes.len(),
            )
        };
        if written > 0 {
            bytes = &bytes[written as usize..];
        } else if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "Agent child cwd report pipe accepted zero bytes",
            ));
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn inherited_agent_child_pipe_writer(
    env_name: &str,
    purpose: &str,
    forbidden_descriptors: &[i32],
    forbidden_identities: &[AgentChildPipeIdentity],
) -> std::io::Result<File> {
    use std::os::fd::FromRawFd;

    let raw = std::env::var_os(env_name).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("missing Agent child {purpose} descriptor"),
        )
    })?;
    // Clear the capability name before parsing or validating it. Even a
    // malformed inherited value must not survive into restored shell state.
    std::env::remove_var(env_name);
    let descriptor = parse_agent_child_descriptor(&raw)
        .filter(|descriptor| {
            *descriptor > nix::libc::STDERR_FILENO && !forbidden_descriptors.contains(descriptor)
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid Agent child {purpose} descriptor"),
            )
        })?;

    let descriptor_flags = unsafe { nix::libc::fcntl(descriptor, nix::libc::F_GETFD) };
    let status_flags = unsafe { nix::libc::fcntl(descriptor, nix::libc::F_GETFL) };
    let mut metadata = std::mem::MaybeUninit::<nix::libc::stat>::zeroed();
    let stat_result = unsafe { nix::libc::fstat(descriptor, metadata.as_mut_ptr()) };
    if descriptor_flags < 0 || status_flags < 0 || stat_result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let metadata = unsafe { metadata.assume_init() };
    if metadata.st_mode & nix::libc::S_IFMT != nix::libc::S_IFIFO
        || status_flags & nix::libc::O_ACCMODE != nix::libc::O_WRONLY
        || forbidden_identities.contains(&AgentChildPipeIdentity::from_metadata(&metadata))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Agent child {purpose} descriptor is not a write-only pipe"),
        ));
    }
    if unsafe {
        nix::libc::fcntl(
            descriptor,
            nix::libc::F_SETFD,
            descriptor_flags | nix::libc::FD_CLOEXEC,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: the descriptor was inherited specifically for this child,
    // validated above, and is consumed exactly once by this owner.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

/// Parse the decimal descriptor spelling produced by `RawFd::to_string`.
/// Reject signs, whitespace, Unicode digits, and leading-zero aliases so an
/// inherited capability has exactly one accepted textual representation.
fn parse_agent_child_descriptor(value: &std::ffi::OsStr) -> Option<i32> {
    use std::os::unix::ffi::OsStrExt;

    if value.as_bytes().len() > MAX_AGENT_CHILD_DESCRIPTOR_TEXT_BYTES {
        return None;
    }
    let value = value.to_str()?;
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok()
}

fn read_agent_child_cwd_nonce_from_env(
    forbidden_descriptors: &[i32],
    forbidden_identities: &[AgentChildPipeIdentity],
) -> std::io::Result<[u8; AGENT_CHILD_CWD_NONCE_BYTES]> {
    use std::os::fd::FromRawFd;

    let raw = std::env::var_os(AGENT_CHILD_CWD_NONCE_FD_ENV).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "missing Agent child cwd nonce descriptor",
        )
    })?;
    std::env::remove_var(AGENT_CHILD_CWD_NONCE_FD_ENV);
    let descriptor = parse_agent_child_descriptor(&raw)
        .filter(|descriptor| {
            *descriptor > nix::libc::STDERR_FILENO && !forbidden_descriptors.contains(descriptor)
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid Agent child cwd nonce descriptor",
            )
        })?;

    let descriptor_flags = unsafe { nix::libc::fcntl(descriptor, nix::libc::F_GETFD) };
    let status_flags = unsafe { nix::libc::fcntl(descriptor, nix::libc::F_GETFL) };
    let mut metadata = std::mem::MaybeUninit::<nix::libc::stat>::zeroed();
    let stat_result = unsafe { nix::libc::fstat(descriptor, metadata.as_mut_ptr()) };
    if descriptor_flags < 0 || status_flags < 0 || stat_result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let metadata = unsafe { metadata.assume_init() };
    if metadata.st_mode & nix::libc::S_IFMT != nix::libc::S_IFIFO
        || status_flags & nix::libc::O_ACCMODE != nix::libc::O_RDONLY
        || forbidden_identities.contains(&AgentChildPipeIdentity::from_metadata(&metadata))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Agent child cwd nonce descriptor is not a read-only pipe",
        ));
    }
    if unsafe {
        nix::libc::fcntl(
            descriptor,
            nix::libc::F_SETFD,
            descriptor_flags | nix::libc::FD_CLOEXEC,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: validation above establishes a distinct inherited read end and
    // this function takes its sole ownership.
    let mut reader = unsafe { File::from_raw_fd(descriptor) };
    read_exact_agent_child_cwd_nonce(&mut reader)
}

fn read_exact_agent_child_cwd_nonce(
    reader: &mut impl Read,
) -> std::io::Result<[u8; AGENT_CHILD_CWD_NONCE_BYTES]> {
    let mut nonce = [0_u8; AGENT_CHILD_CWD_NONCE_BYTES];
    reader.read_exact(&mut nonce)?;
    let mut extra = [0_u8; 1];
    if reader.read(&mut extra)? != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Agent child cwd nonce channel contained extra bytes",
        ));
    }
    Ok(nonce)
}

fn encode_agent_child_cwd_frame(
    cwd: &Path,
    nonce: &[u8; AGENT_CHILD_CWD_NONCE_BYTES],
) -> std::io::Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;

    let raw = cwd.as_os_str().as_bytes();
    if raw.is_empty()
        || raw.len() > MAX_AGENT_CHILD_CWD_BYTES
        || raw.contains(&0)
        || !cwd.is_absolute()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Agent cwd is empty, relative, or exceeds its byte limit",
        ));
    }
    let length = u32::try_from(raw.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Agent cwd exceeds its frame limit",
        )
    })?;
    let mut frame = Vec::with_capacity(AGENT_CHILD_CWD_FRAME_HEADER_BYTES + raw.len());
    frame.extend_from_slice(&AGENT_CHILD_CWD_FRAME_MAGIC);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(nonce);
    frame.extend_from_slice(raw);
    Ok(frame)
}

/// Close every inherited descriptor outside the protocol set. The process
/// that launched the one-shot child may hold arbitrary non-CLOEXEC
/// descriptors (an inotify instance, a session socket, a stray log pipe) that
/// its own CLOEXEC discipline never covered; without this scrub they would be
/// inherited by the approved external command, leaking host capabilities the
/// Agent protocol is designed to withhold. Everything the child opens after
/// this point is CLOEXEC by Rust's default, so approved commands stay clean.
/// Failures are ignored on purpose: a descriptor that cannot be queried or
/// closed here was either never inherited or is already gone, and the
/// protocol descriptors themselves are validated before any use regardless.
fn close_inherited_descriptors_except(keep: &[i32]) {
    // Collect first: the directory iterator itself holds a descriptor into
    // /proc/self/fd, and closing it mid-iteration would truncate the scan.
    let inherited: Vec<i32> = match std::fs::read_dir("/proc/self/fd") {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_string_lossy().parse::<i32>().ok())
            .collect(),
        // No procfs means no way to enumerate; the CLOEXEC discipline above
        // still covers every descriptor jsh itself creates.
        Err(_) => return,
    };
    for descriptor in inherited {
        if descriptor <= nix::libc::STDERR_FILENO || keep.contains(&descriptor) {
            continue;
        }
        unsafe { nix::libc::close(descriptor) };
    }
}

fn decode_agent_child_cwd_frame(
    frame: &[u8],
    expected_nonce: &[u8; AGENT_CHILD_CWD_NONCE_BYTES],
) -> std::io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    if frame.len() < AGENT_CHILD_CWD_FRAME_HEADER_BYTES
        || frame[..4] != AGENT_CHILD_CWD_FRAME_MAGIC
        || !agent_child_nonce_matches(
            &frame[8..AGENT_CHILD_CWD_FRAME_HEADER_BYTES],
            expected_nonce,
        )
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid Agent cwd frame header or nonce",
        ));
    }
    let length =
        u32::from_be_bytes(frame[4..8].try_into().expect("fixed cwd frame header")) as usize;
    let expected = AGENT_CHILD_CWD_FRAME_HEADER_BYTES.checked_add(length);
    if length == 0 || length > MAX_AGENT_CHILD_CWD_BYTES || expected != Some(frame.len()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "truncated, extra, or oversized Agent cwd frame",
        ));
    }
    let raw = &frame[AGENT_CHILD_CWD_FRAME_HEADER_BYTES..];
    if raw.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Agent cwd frame contains a NUL byte",
        ));
    }
    let cwd = PathBuf::from(std::ffi::OsString::from_vec(raw.to_vec()));
    if !cwd.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Agent cwd frame contains a relative path",
        ));
    }
    Ok(cwd)
}

/// Compare the fixed-size capability without a data-dependent early return.
fn agent_child_nonce_matches(actual: &[u8], expected: &[u8; AGENT_CHILD_CWD_NONCE_BYTES]) -> bool {
    if actual.len() != AGENT_CHILD_CWD_NONCE_BYTES {
        return false;
    }
    let mut difference = 0_u8;
    for index in 0..AGENT_CHILD_CWD_NONCE_BYTES {
        difference |= actual[index] ^ expected[index];
    }
    std::hint::black_box(difference) == 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentChildReadinessIssue {
    UnexpectedMarker,
    DuplicateMarker,
    ReadError,
}

#[derive(Debug, Default)]
struct AgentChildReadiness {
    ready: bool,
    closed: bool,
    received_bytes: usize,
    issue: Option<AgentChildReadinessIssue>,
}

impl AgentChildReadiness {
    fn authenticated(&self) -> bool {
        self.ready && self.closed && self.issue.is_none()
    }

    fn invalid(&self) -> bool {
        self.issue.is_some()
    }
}

fn drain_agent_child_readiness(
    descriptor: std::os::fd::BorrowedFd<'_>,
    readiness: &mut AgentChildReadiness,
) {
    if readiness.closed || readiness.invalid() {
        return;
    }
    let mut bytes = [0_u8; 16];
    let mut drained = 0_usize;
    while drained < MAX_AGENT_CHILD_READINESS_DRAIN_BYTES {
        let available = (MAX_AGENT_CHILD_READINESS_DRAIN_BYTES - drained).min(bytes.len());
        match nix::unistd::read(descriptor, &mut bytes[..available]) {
            Ok(0) => {
                readiness.closed = true;
                return;
            }
            Ok(count) => {
                drained = drained.saturating_add(count);
                readiness.received_bytes = readiness.received_bytes.saturating_add(count);
                for byte in &bytes[..count] {
                    if *byte == AGENT_CHILD_READY && !readiness.ready {
                        readiness.ready = true;
                    } else if *byte == AGENT_CHILD_READY {
                        readiness.issue = Some(AgentChildReadinessIssue::DuplicateMarker);
                        return;
                    } else {
                        readiness.issue = Some(AgentChildReadinessIssue::UnexpectedMarker);
                        return;
                    }
                }
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(nix::errno::Errno::EAGAIN) => return,
            Err(_) => {
                readiness.issue = Some(AgentChildReadinessIssue::ReadError);
                readiness.closed = true;
                return;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentChildCwdFrameIssue {
    InvalidMagic,
    InvalidLength,
    NonceMismatch,
    ExtraBytes,
    TruncatedFrame,
    ReadError,
}

#[derive(Default)]
struct AgentChildCwdFrameBuffer {
    bytes: Vec<u8>,
    closed: bool,
    received_bytes: usize,
    expected_bytes: Option<usize>,
    issue: Option<AgentChildCwdFrameIssue>,
}

impl std::fmt::Debug for AgentChildCwdFrameBuffer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentChildCwdFrameBuffer")
            .field("stored_bytes", &self.bytes.len())
            .field("received_bytes", &self.received_bytes)
            .field("expected_bytes", &self.expected_bytes)
            .field("closed", &self.closed)
            .field("issue", &self.issue)
            .finish()
    }
}

impl AgentChildCwdFrameBuffer {
    fn observe(&mut self, bytes: &[u8], expected_nonce: &[u8; AGENT_CHILD_CWD_NONCE_BYTES]) {
        self.received_bytes = self.received_bytes.saturating_add(bytes.len());
        if self.issue.is_some() {
            return;
        }
        let remaining = MAX_AGENT_CHILD_CWD_FRAME_BYTES.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        if bytes.len() > remaining {
            self.issue = Some(AgentChildCwdFrameIssue::ExtraBytes);
            return;
        }

        let magic_prefix_bytes = self.bytes.len().min(AGENT_CHILD_CWD_FRAME_MAGIC.len());
        if self.bytes[..magic_prefix_bytes] != AGENT_CHILD_CWD_FRAME_MAGIC[..magic_prefix_bytes] {
            self.issue = Some(AgentChildCwdFrameIssue::InvalidMagic);
            return;
        }
        if self.bytes.len() >= 8 && self.expected_bytes.is_none() {
            let length = u32::from_be_bytes(
                self.bytes[4..8]
                    .try_into()
                    .expect("complete cwd length header"),
            ) as usize;
            if length == 0 || length > MAX_AGENT_CHILD_CWD_BYTES {
                self.issue = Some(AgentChildCwdFrameIssue::InvalidLength);
                return;
            }
            self.expected_bytes = AGENT_CHILD_CWD_FRAME_HEADER_BYTES.checked_add(length);
        }
        if self.bytes.len() >= AGENT_CHILD_CWD_FRAME_HEADER_BYTES
            && !agent_child_nonce_matches(
                &self.bytes[8..AGENT_CHILD_CWD_FRAME_HEADER_BYTES],
                expected_nonce,
            )
        {
            self.issue = Some(AgentChildCwdFrameIssue::NonceMismatch);
            return;
        }
        if self
            .expected_bytes
            .is_some_and(|expected| self.bytes.len() > expected)
        {
            self.issue = Some(AgentChildCwdFrameIssue::ExtraBytes);
        }
    }

    fn finish_eof(&mut self) {
        self.closed = true;
        if self.issue.is_none() && self.expected_bytes != Some(self.bytes.len()) {
            self.issue = Some(AgentChildCwdFrameIssue::TruncatedFrame);
        }
    }

    fn should_close_reader(&self) -> bool {
        self.closed || self.issue.is_some()
    }

    fn decode(
        &self,
        expected_nonce: &[u8; AGENT_CHILD_CWD_NONCE_BYTES],
    ) -> std::io::Result<PathBuf> {
        if !self.closed || self.issue.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Agent cwd frame did not close cleanly",
            ));
        }
        decode_agent_child_cwd_frame(&self.bytes, expected_nonce)
    }
}

fn drain_agent_child_cwd_frame(
    descriptor: std::os::fd::BorrowedFd<'_>,
    report: &mut AgentChildCwdFrameBuffer,
    expected_nonce: &[u8; AGENT_CHILD_CWD_NONCE_BYTES],
) {
    if report.should_close_reader() {
        return;
    }
    let mut bytes = [0_u8; 4096];
    let mut drained = 0_usize;
    while drained < MAX_AGENT_CHILD_CWD_DRAIN_BYTES {
        let available = (MAX_AGENT_CHILD_CWD_DRAIN_BYTES - drained).min(bytes.len());
        match nix::unistd::read(descriptor, &mut bytes[..available]) {
            Ok(0) => {
                report.finish_eof();
                return;
            }
            Ok(count) => {
                drained = drained.saturating_add(count);
                report.observe(&bytes[..count], expected_nonce);
                if report.issue.is_some() {
                    return;
                }
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(nix::errno::Errno::EAGAIN) => return,
            Err(_) => {
                report.issue = Some(AgentChildCwdFrameIssue::ReadError);
                report.closed = true;
                return;
            }
        }
    }
}

fn drain_agent_child_cwd_reader(
    reader: &mut Option<std::os::fd::OwnedFd>,
    report: &mut AgentChildCwdFrameBuffer,
    expected_nonce: &[u8; AGENT_CHILD_CWD_NONCE_BYTES],
) {
    use std::os::fd::AsFd;

    let Some(descriptor) = reader.as_ref() else {
        return;
    };
    drain_agent_child_cwd_frame(descriptor.as_fd(), report, expected_nonce);
    if report.should_close_reader() {
        // Closing on an invalid frame prevents a hostile same-process writer
        // from filling the pipe and blocking the one-shot child at shutdown.
        // Valid frames close here only after EOF has authenticated their end.
        reader.take();
    }
}

/// Create the Agent capture pipe with close-on-exec set on both original
/// descriptors. Linux and Android can do this atomically. Other Unix targets
/// (notably macOS, which has no `pipe2` in nix) set the descriptor flag before
/// either end is exposed to `Command`; owned descriptors close automatically
/// if either `fcntl` call fails.
fn capture_pipe_cloexec() -> nix::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        use nix::fcntl::{fcntl, FcntlArg, FdFlag};

        let (reader, writer) = nix::unistd::pipe()?;
        fcntl(&reader, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        fcntl(&writer, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        Ok((reader, writer))
    }
}

fn agent_child_cwd_nonce_channel(
) -> std::io::Result<([u8; AGENT_CHILD_CWD_NONCE_BYTES], std::os::fd::OwnedFd)> {
    let mut random = File::open("/dev/urandom")?;
    let mut nonce = [0_u8; AGENT_CHILD_CWD_NONCE_BYTES];
    loop {
        random.read_exact(&mut nonce)?;
        if nonce.iter().any(|byte| *byte != 0) {
            break;
        }
    }
    let (reader, writer) = capture_pipe_cloexec().map_err(std::io::Error::from)?;
    let mut writer = File::from(writer);
    writer.write_all(&nonce)?;
    writer.flush()?;
    drop(writer);
    Ok((nonce, reader))
}

pub fn builtin_agent(args: &[String], state: &mut ShellState) -> i32 {
    let Some(ai_config) = AiConfig::from_env() else {
        eprintln!(
            "agent: AI is not configured. Set JSH_AI_PROVIDER=anthropic|openai|ollama \
             (plus the provider API key) or JSH_AI_ENABLED=1; see README."
        );
        return 1;
    };
    let chat = chat_config(&ai_config);
    let share_context = ai_config.allows_extended_context();
    let protocol = match configured_agent_protocol_from_env(chat.provider) {
        Ok(protocol) => protocol,
        Err(error) => {
            eprintln!("agent: {error}");
            return 1;
        }
    };

    let goal = if args.is_empty() {
        match read_line("agent goal> ") {
            Some(line) if !line.trim().is_empty() => line,
            _ => return 0,
        }
    } else {
        args.join(" ")
    };

    let mut session = AgentSession::new(max_turns());
    if let Err(error) = session.submit_user(goal) {
        eprintln!("agent: {error}");
        return 1;
    }

    if env_truthy("JSH_AGENT_AUTO_APPROVE_READONLY") {
        eprintln!(
            "agent: JSH_AGENT_AUTO_APPROVE_READONLY is retired; every proposal now requires explicit approval"
        );
    }
    let mut protocol_retries = 0_u32;
    // The agent's working directory persists across its own turns (each
    // approved command starts here, and a command's `cd` carries forward)
    // without ever changing the interactive shell's cwd.
    let mut agent_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));

    loop {
        if let Some(status) = take_agent_interrupt(&mut session, state) {
            return status;
        }
        match session.state() {
            AgentState::AwaitingModel => {
                status_line(&format!(
                    "thinking… (turn {}/{})",
                    session.turns_used() + 1,
                    session.max_turns()
                ));
                let reply =
                    match request_model(&chat, &session, share_context, &agent_cwd, protocol) {
                        Ok(reply) => reply,
                        Err(error) => {
                            if let Some(status) = take_agent_interrupt(&mut session, state) {
                                return status;
                            }
                            let _ = session.model_failed(&error);
                            let error = crate::ai::redact_sensitive_text(&error);
                            eprintln!(
                                "{}",
                                terminal_safe_message(
                                    "agent: model request failed: ",
                                    &error,
                                    MAX_AGENT_DISPLAY_BYTES
                                )
                            );
                            if session.can_retry_model() && confirm("retry? [y/N] ") {
                                let _ = session.retry_model();
                                continue;
                            }
                            return 1;
                        }
                    };
                if let Some(status) = take_agent_interrupt(&mut session, state) {
                    return status;
                }
                match session.accept_agent_response(&reply) {
                    Ok(ModelOutcome::Proposal {
                        id,
                        command,
                        danger: _,
                    }) => {
                        let danger = jagent::is_dangerous(&command);
                        protocol_retries = 0;
                        let approved = match review_proposal(&mut session, id, &command, danger) {
                            ReviewOutcome::Approved(approved) => approved,
                            ReviewOutcome::Rejected => continue,
                            ReviewOutcome::Insert(command) => {
                                state.pending_editor_insert = Some(command);
                                println!(
                                    "agent: command moved to your next prompt for manual \
                                     review; it was not executed."
                                );
                                return 0;
                            }
                            ReviewOutcome::Quit => return 0,
                        };
                        let execution = run_captured(&approved.command, state, &mut agent_cwd);
                        if let Some(status) = take_agent_interrupt(&mut session, state) {
                            return status;
                        }
                        if let Err(error) =
                            session.observe_execution(approved.proposal_id, execution)
                        {
                            eprintln!("agent: {error}");
                            return 1;
                        }
                    }
                    Ok(ModelOutcome::Said(message)) => {
                        protocol_retries = 0;
                        let message = crate::ai::redact_sensitive_text(&message);
                        println!(
                            "{}",
                            terminal_safe_message("agent: ", &message, MAX_AGENT_DISPLAY_BYTES)
                        );
                    }
                    Ok(ModelOutcome::Completed(message)) => {
                        protocol_retries = 0;
                        let message = crate::ai::redact_sensitive_text(&message);
                        println!(
                            "{}",
                            terminal_safe_message(
                                "agent done: ",
                                &message,
                                MAX_AGENT_DISPLAY_BYTES
                            )
                        );
                    }
                    Err(SessionError::Protocol(error)) => {
                        let error = crate::ai::redact_sensitive_text(&error.to_string());
                        eprintln!(
                            "{}",
                            terminal_safe_message(
                                "agent: model reply violated the protocol: ",
                                &error,
                                MAX_AGENT_DISPLAY_BYTES
                            )
                        );
                        if protocol_retries < MAX_CONSECUTIVE_PROTOCOL_RETRIES
                            && session.can_retry_model()
                        {
                            protocol_retries += 1;
                            let _ = session.retry_model();
                        }
                    }
                    Err(error) => {
                        eprintln!("agent: {error}");
                        return 1;
                    }
                }
            }
            AgentState::Ready => {
                let Some(line) = read_line("you> ") else {
                    return 0;
                };
                let line = line.trim().to_string();
                if line.is_empty() || line == "q" || line == "quit" {
                    return 0;
                }
                if let Err(error) = session.submit_user(line) {
                    eprintln!("agent: {error}");
                    if matches!(error, SessionError::TurnLimitReached) {
                        return 1;
                    }
                }
            }
            AgentState::Completed => {
                if !session.can_continue_after_completion() {
                    return 0;
                }
                let Some(line) = read_line("follow-up (Enter to finish)> ") else {
                    return 0;
                };
                let line = line.trim().to_string();
                if line.is_empty() {
                    return 0;
                }
                if session.continue_after_completion().is_err() {
                    return 0;
                }
                if let Err(error) = session.submit_user(line) {
                    eprintln!("agent: {error}");
                    return 1;
                }
            }
            AgentState::TurnLimitReached => {
                eprintln!(
                    "agent: turn budget of {} reached (JSH_AGENT_MAX_TURNS to raise)",
                    session.max_turns()
                );
                return 1;
            }
            AgentState::Cancelled => return 0,
            AgentState::AwaitingApproval { .. } | AgentState::AwaitingObservation { .. } => {
                // Both are resolved inline in the proposal arm; reaching here
                // means an internal bug, so fail instead of spinning.
                eprintln!("agent: internal state error");
                return 1;
            }
        }
    }
}

enum ReviewOutcome {
    Approved(ApprovedCommand),
    Rejected,
    /// Insert-only manual review: the command text goes to the next editor
    /// prompt without executing, and the session records the non-execution.
    Insert(String),
    Quit,
}

fn review_proposal(
    session: &mut AgentSession,
    id: jagent::ProposalId,
    command: &str,
    danger: Option<&'static str>,
) -> ReviewOutcome {
    println!();
    println!(
        "  proposed: {}",
        emphasize(&terminal_safe_inline_text(
            command,
            MAX_AGENT_COMMAND_DISPLAY_BYTES
        ))
    );
    if let Some(reason) = danger {
        println!("  {}", warn(&format!("warning: {reason}")));
    }
    loop {
        let Some(choice) = read_line("  [y] run  [e] edit  [i] insert  [n] reject  [q] quit > ")
        else {
            session.cancel();
            return ReviewOutcome::Quit;
        };
        match choice.trim() {
            "y" | "yes" => {
                let approved = match session.approve(id) {
                    Ok(approved) => approved,
                    Err(error) => {
                        eprintln!("agent: {error}");
                        return ReviewOutcome::Quit;
                    }
                };
                if !confirm_danger(&approved, danger) {
                    // The state machine has already recorded the approval, so
                    // backing out means ending the session rather than
                    // pretending the proposal is pending again.
                    session.cancel();
                    return ReviewOutcome::Quit;
                }
                return ReviewOutcome::Approved(approved);
            }
            "e" | "edit" => {
                let Some(edited) = read_line("  edit> ") else {
                    continue;
                };
                if !crate::terminal_text::is_safe_inline(&edited) {
                    eprintln!(
                        "  agent: edited command contains invisible, bidirectional, or \
                         terminal-control characters"
                    );
                    continue;
                }
                match session.edit_and_approve(id, edited) {
                    Ok(approved) => {
                        let danger = jagent::is_dangerous(&approved.command);
                        if !confirm_danger(&approved, danger) {
                            session.cancel();
                            return ReviewOutcome::Quit;
                        }
                        return ReviewOutcome::Approved(approved);
                    }
                    Err(error) => {
                        eprintln!("  agent: {error}");
                    }
                }
            }
            "i" | "insert" => match session.edit_for_manual_review(id, command) {
                Ok(command) => return ReviewOutcome::Insert(command),
                Err(error) => {
                    eprintln!("  agent: {error}");
                }
            },
            "n" | "no" | "reject" => {
                if let Err(error) = session.reject(id) {
                    eprintln!("agent: {error}");
                    return ReviewOutcome::Quit;
                }
                return ReviewOutcome::Rejected;
            }
            "q" | "quit" => {
                session.cancel();
                return ReviewOutcome::Quit;
            }
            _ => {}
        }
    }
}

/// Recognized-dangerous commands need a second, deliberate confirmation after
/// approval, mirroring jterm4's exact-command confirmation gate.
fn confirm_danger(approved: &ApprovedCommand, danger: Option<&'static str>) -> bool {
    let ambiguous_text = approved
        .command
        .chars()
        .any(crate::terminal_text::is_terminal_ambiguous);
    if danger.is_none() && approved.danger.is_none() && !ambiguous_text {
        return true;
    }
    if let Some(reason) = danger.or(approved.danger) {
        println!("  {}", warn(&format!("dangerous: {reason}")));
    }
    if ambiguous_text {
        println!(
            "  {}",
            warn("dangerous: invisible or bidirectional Unicode is shown as an explicit escape")
        );
    }
    match read_line("  type RUN to execute, anything else aborts > ") {
        Some(line) => line.trim() == "RUN",
        None => false,
    }
}

fn request_model(
    chat: &ChatConfig,
    session: &AgentSession,
    share_context: bool,
    agent_cwd: &Path,
    protocol: AgentProtocol,
) -> Result<AgentResponse, String> {
    let environment = environment_meta(share_context, agent_cwd);
    if crate::signal::pending_status().is_some() {
        return Err("interrupted".to_string());
    }
    // Transcript observations replay real terminal output, which is where API
    // keys and connection strings show up. AgentRequestSpec's secure default
    // redacts every history turn and binds this protocol to its matching
    // system prompt, provider schema, and response decoder.
    let user_text = jagent::agent_user_prompt(
        &session.build_user_prompt_with(protocol),
        &environment,
        None,
    );
    let history = [Message {
        role: Role::User,
        text: user_text,
    }];
    let prepared = prepare_agent_request(chat, AgentRequestSpec::new(&history, protocol))
        .map_err(|error| error.to_string())?;
    debug_assert!(prepared.report.redaction_enabled);
    let raw = model_request(prepared.request.clone(), chat.provider)?;
    prepared
        .parse_response(&raw)
        .map_err(|error| error.to_string())
}

/// Perform one model request in a child process, so cancelling it ends it.
///
/// ureq is intentionally blocking, and a blocking socket read cannot be
/// interrupted in place. Running it on a worker *thread* therefore only ever
/// achieved half the job: INT released the foreground promptly, but the request
/// itself kept running — still connected, still being billed, and still holding
/// the single-flight slot — until the provider's own read timeout expired, and
/// a second request in that window was refused outright.
///
/// A child process has a handle the parent can actually use. `SIGKILL` to the
/// process group ends the request now, which is why there is no in-flight gate
/// here any more: there is never a previous request left to wait for.
///
/// The child is this same binary. That matters: TLS verification, the redirect
/// policy, the response header caps and the body ceiling are all the code in
/// [`perform_model_request`], unchanged and unduplicated. Only *where* it runs
/// moved.
///
/// The envelope travels on stdin rather than argv because it carries the API
/// key, and argv is world-readable through `/proc`.
fn model_request(request: HttpRequest, provider: Provider) -> Result<Vec<u8>, String> {
    if crate::signal::pending_status().is_some() {
        return Err("interrupted".to_string());
    }
    // `HttpRequest` is intentionally public and cloneable. Validate again at
    // the process boundary so an integration-side mutation is rejected before
    // credentials are serialized into the private child envelope.
    request
        .validate_transport()
        .map_err(|error| format!("invalid model request: {error}"))?;
    let envelope = encode_model_request(&request, provider);
    if envelope.len() > MAX_MODEL_REQUEST_ENVELOPE_BYTES {
        return Err("model request is too large to send".to_string());
    }

    let executable =
        std::env::current_exe().map_err(|error| format!("could not locate jsh: {error}"))?;
    let mut command = std::process::Command::new(executable);
    command.arg(INTERNAL_MODEL_REQUEST_FLAG);

    let cancelled = || crate::signal::pending_status().is_some();
    let output = crate::io_guard::bounded_command_session(
        &mut command,
        crate::io_guard::BoundedCommand {
            // One status byte of framing on top of the transport's own ceiling.
            stdout_limit: MAX_AGENT_RESPONSE_BYTES as usize + 1,
            stderr_limit: MAX_MODEL_CHILD_STDERR_BYTES,
            timeout: MODEL_REQUEST_DEADLINE,
            stdin: Some(envelope.as_bytes()),
            cancel: Some(&cancelled),
            // A shell that is killed outright must not leave a request running
            // against the provider with nobody to read the answer.
            die_with_parent: true,
            new_session: false,
        },
    );

    let output = match output {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
            return Err("interrupted".to_string())
        }
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            return Err("model request timed out".to_string())
        }
        Err(error) => return Err(format!("could not run the model request: {error}")),
    };

    match output.stdout.split_first() {
        Some((&MODEL_CHILD_OK, body)) => Ok(body.to_vec()),
        Some((&MODEL_CHILD_ERR, message)) => Err(String::from_utf8_lossy(message).into_owned()),
        // No framing byte at all means the child died before it could answer —
        // a signal, or a binary that is not this jsh. Its stderr is untrusted
        // and is deliberately not echoed.
        _ => Err("the model request did not complete".to_string()),
    }
}

/// Serialize a request for the child. Deliberately jsh's own small envelope
/// rather than serde on jagent's types: the wire format is private to this pair
/// of processes, and it should not move when a dependency adds a field.
fn encode_model_request(request: &HttpRequest, provider: Provider) -> String {
    serde_json::json!({
        "v": 1,
        "provider": provider.as_config_value(),
        "url": request.url,
        "headers": request.headers,
        "body": request.body,
    })
    .to_string()
}

/// Strict private wire schema for the independently invokable transport
/// child. Deriving directly into a struct makes serde reject repeated known
/// fields instead of applying `Value`'s last-member-wins rule; unknown fields
/// are rejected so the parent and child cannot silently disagree about a
/// future envelope revision.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRequestEnvelope {
    v: u64,
    provider: String,
    url: String,
    headers: Vec<(String, String)>,
    body: String,
}

fn decode_model_request(envelope: &str) -> Result<(HttpRequest, Provider), String> {
    // Do not reflect serde diagnostics: an unknown field or variant name is
    // controlled by the caller, while the child protocol only needs a stable
    // bounded classification.
    let envelope: ModelRequestEnvelope =
        serde_json::from_str(envelope).map_err(|_| "malformed request".to_string())?;
    if envelope.v != 1 {
        return Err("unsupported request version".to_string());
    }
    let provider = match envelope.provider.as_str() {
        "anthropic" => Provider::Anthropic,
        "openai-compatible" => Provider::OpenAiCompatible,
        "ollama" => Provider::Ollama,
        // Do not reflect an untrusted envelope value into the control reply.
        // The caller only needs the bounded classification, not the bytes it
        // supplied.
        _ => return Err("unknown provider".to_string()),
    };
    Ok((
        HttpRequest {
            url: envelope.url,
            headers: envelope.headers,
            body: envelope.body,
        },
        provider,
    ))
}

/// Child entry point for [`INTERNAL_MODEL_REQUEST_FLAG`]. Returns the process
/// exit code, or `None` when this invocation is not a model-request child.
///
/// Dispatched before any startup file, history, or session work: this process
/// exists to perform exactly one HTTP request and then die.
pub(crate) fn run_internal_model_request(args: &[std::ffi::OsString]) -> Option<i32> {
    if args.get(1).and_then(|arg| arg.to_str()) != Some(INTERNAL_MODEL_REQUEST_FLAG) {
        return None;
    }
    if args.len() != 2 {
        eprintln!("jsh: internal model request received unexpected arguments");
        return Some(2);
    }
    let answer = |marker: u8, payload: &str| {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(&[marker]);
        let _ = stdout.write_all(payload.as_bytes());
        let _ = stdout.flush();
    };

    let envelope = match crate::io_guard::read_to_end_bounded(
        std::io::stdin().lock(),
        MAX_MODEL_REQUEST_ENVELOPE_BYTES,
    ) {
        Ok(bytes) => bytes,
        Err(error) => {
            answer(MODEL_CHILD_ERR, &format!("could not read request: {error}"));
            return Some(1);
        }
    };
    let envelope = match String::from_utf8(envelope) {
        Ok(text) => text,
        Err(_) => {
            answer(MODEL_CHILD_ERR, "request was not valid UTF-8");
            return Some(1);
        }
    };
    match decode_model_request(&envelope) {
        Ok((request, provider)) => match perform_model_request(request, provider) {
            Ok(reply) => {
                answer(MODEL_CHILD_OK, &reply);
                Some(0)
            }
            Err(error) => {
                answer(MODEL_CHILD_ERR, &error);
                Some(1)
            }
        },
        Err(error) => {
            answer(MODEL_CHILD_ERR, &error);
            Some(1)
        }
    }
}

fn perform_model_request(request: HttpRequest, _provider: Provider) -> Result<String, String> {
    // The hidden child is an independently invokable process boundary. Its
    // stdin is untrusted even though the ordinary parent creates it, so the
    // decoded public request must pass jagent's complete transport contract
    // immediately before any HTTP client or resolver sees it.
    let bypass_environment_proxy = crate::ai::request_must_bypass_proxy(&request)
        .map_err(|error| format!("invalid model request: {error}"))?;
    let agent = agent_http_client(bypass_environment_proxy);
    let mut post = agent.post(&request.url);
    for (name, value) in &request.headers {
        post = post.header(name, value);
    }
    let mut response = post
        .send(request.body.as_str())
        .map_err(|error| error.to_string())?;
    let status = response.status();
    // ureq applies this configured limit to wire bytes below its content
    // decoder. Preserve that bound, then cap the decoded reader separately so
    // a small transparently decoded response (currently gzip) cannot expand
    // into an unbounded String in this independently invokable child.
    let decoded = response
        .body_mut()
        .with_config()
        .limit(MAX_AGENT_RESPONSE_BYTES)
        .reader();
    let text = read_model_response_bounded(decoded)?;
    if !status.is_success() {
        // The response body is untrusted terminal data. Provider diagnostics
        // are deliberately not echoed before protocol validation.
        return Err(format!("HTTP {}", status.as_u16()));
    }
    // Keep the provider body intact. The parent still owns the
    // PreparedAgentRequest that created this request and parses these bytes
    // through its bound provider/protocol decoder before session ingestion.
    // Parsing in this transport child used to erase completion metadata and
    // made native-tool replies impossible to carry back safely.
    Ok(text)
}

fn read_model_response_bounded(reader: impl Read) -> Result<String, String> {
    let mut reader = reader.take(MAX_AGENT_RESPONSE_BYTES + 1);
    let mut text = String::new();
    reader
        .read_to_string(&mut text)
        .map_err(|error| format!("read error: {error}"))?;
    if text.len() as u64 > MAX_AGENT_RESPONSE_BYTES {
        return Err(format!(
            "model response exceeds the {MAX_AGENT_RESPONSE_BYTES}-byte decoded limit"
        ));
    }
    Ok(text)
}

fn agent_http_client(bypass_environment_proxy: bool) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .max_redirects(0)
        .timeout_connect(Some(std::time::Duration::from_secs(5)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(120)))
        .timeout_recv_body(Some(std::time::Duration::from_secs(120)))
        .timeout_send_body(Some(std::time::Duration::from_secs(10)))
        // Keep non-2xx as a normal response so the provider's error body can be
        // read and reported instead of a bare status code.
        .http_status_as_error(false);
    let config = if bypass_environment_proxy {
        config.proxy(None)
    } else {
        config
    };
    config.build().into()
}

fn chat_config(ai_config: &AiConfig) -> ChatConfig {
    // Strict JSON protocol compliance beats creativity here.
    ai_config.chat_config(AGENT_MAX_TOKENS, Some(0.0))
}

fn environment_meta(share_context: bool, cwd: &Path) -> EnvironmentMeta {
    EnvironmentMeta {
        cwd: cwd.display().to_string(),
        shell: "jsh".to_string(),
        os: std::env::consts::OS.to_string(),
        git: if share_context { git_meta(cwd) } else { None },
    }
}

fn git_meta(cwd: &Path) -> Option<GitMeta> {
    let branch = bounded_git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"], 16 * 1024)
        .and_then(|output| String::from_utf8(output).ok())
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty())?;
    let dirty = git_worktree_dirty(cwd)?;
    Some(GitMeta {
        branch,
        dirty,
        // Not computed here; None is honest ("unknown/no upstream"), while 0
        // would claim the branch is exactly in sync.
        ahead: None,
        behind: None,
    })
}

fn bounded_git_stdout(cwd: &Path, args: &[&str], max_bytes: usize) -> Option<Vec<u8>> {
    crate::prompt::bounded_git_stdout(cwd, args, max_bytes)
}

/// Keep the Agent probe on the same trusted, process-group-bounded Git funnel
/// as prompt/completion metadata. Oversized repositories simply omit the
/// optional dirty bit.
fn git_worktree_dirty(cwd: &Path) -> Option<bool> {
    bounded_git_stdout(cwd, &["status", "--porcelain"], 128 * 1024).map(|output| !output.is_empty())
}

fn take_agent_interrupt(session: &mut AgentSession, state: &mut ShellState) -> Option<i32> {
    crate::signal::take_pending_status().inspect(|_| {
        session.cancel();
        // SIGINT is consumable so the interactive shell can return to its next
        // prompt. Preserve its control-flow meaning separately: do not run a
        // later command from the same parsed `agent ...; ...` list.
        state.abort_current_program = true;
        eprintln!("agent: interrupted");
    })
}

/// One-shot private state passed to a fresh jsh process for an approved Agent
/// command. Spawning a new process avoids executing Rust after `fork()` in the
/// interactive shell, which always has at least the AI worker thread and may
/// also have a PATH scanner alive.
struct AgentChildTransport {
    dir: PathBuf,
    snapshot: PathBuf,
    claim_dir: PathBuf,
    claimed_snapshot: PathBuf,
}

impl AgentChildTransport {
    fn new(state: &ShellState, cwd: &Path) -> std::io::Result<Self> {
        let uid = unsafe { nix::libc::geteuid() };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let mut last_collision = None;
        for _ in 0..32 {
            let counter = AGENT_CHILD_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "jsh-agent-{uid}-{}-{timestamp}-{counter}",
                std::process::id()
            ));
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {
                    let snapshot = dir.join(format!("{AGENT_CHILD_SESSION_ID}.json"));
                    let claim_dir = dir.join(AGENT_CHILD_CLAIM_DIR);
                    let claimed_snapshot = claim_dir.join(format!("{AGENT_CHILD_SESSION_ID}.json"));
                    if let Err(error) = fs::DirBuilder::new().mode(0o700).create(&claim_dir) {
                        let _ = fs::remove_dir(&dir);
                        return Err(error);
                    }
                    let mut persisted =
                        crate::session::SessionSnapshot::capture(state, AGENT_CHILD_SESSION_ID);
                    // `SessionSnapshot::apply` restores its cwd before the
                    // child executes anything. Keep that first restore aligned
                    // with the Agent's private cwd too, then set the original
                    // OsStr path exactly in the child for non-UTF-8 paths.
                    persisted.cwd = cwd.to_string_lossy().into_owned();
                    if let Err(error) = persisted.save_to_dir(&dir) {
                        let _ = fs::remove_dir(&claim_dir);
                        let _ = fs::remove_dir(&dir);
                        return Err(error);
                    }
                    return Ok(Self {
                        dir,
                        snapshot,
                        claim_dir,
                        claimed_snapshot,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_collision = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_collision.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not allocate private Agent child directory",
            )
        }))
    }
}

impl Drop for AgentChildTransport {
    fn drop(&mut self) {
        // Delete only the exact files jsh created. If an approved command or a
        // same-user process added anything else, leave the private directory
        // behind instead of recursively deleting an unexpected path.
        let _ = fs::remove_file(&self.snapshot);
        let _ = fs::remove_file(&self.claimed_snapshot);
        let _ = fs::remove_dir(&self.claim_dir);
        let _ = fs::remove_dir(&self.dir);
    }
}

/// Dispatch the undocumented one-shot child mode before normal CLI parsing.
/// The marker alone is insufficient: the two private path values, both pipe
/// writers, and the one-shot nonce reader must be present; the snapshot loader
/// independently enforces ownership/link/size rules.
pub(crate) fn internal_child_entrypoint() -> Option<i32> {
    let args = std::env::args_os().collect::<Vec<_>>();
    // The transport child is dispatched from the same place, and before the
    // Agent child, so that neither mode can reach startup-file, history, or
    // session work. Both exist to do one bounded thing and exit.
    if let Some(status) = run_internal_model_request(&args) {
        return Some(status);
    }
    if args.get(1).and_then(|arg| arg.to_str()) != Some(INTERNAL_AGENT_CHILD_FLAG) {
        return None;
    }
    let Some(command) = args.get(2).and_then(|arg| arg.to_str()) else {
        eprintln!("jsh: internal Agent child requires one UTF-8 command");
        return Some(2);
    };
    if args.len() != 3 {
        eprintln!("jsh: internal Agent child received unexpected arguments");
        return Some(2);
    }
    Some(run_internal_agent_child(command))
}

fn run_internal_agent_child(command: &str) -> i32 {
    use std::os::fd::AsRawFd;

    let control = match AgentChildControl::from_env() {
        Ok(control) => control,
        Err(error) => {
            eprintln!("jsh: internal Agent child control setup failed: {error}");
            return 2;
        }
    };
    let control_identity = match agent_child_pipe_identity(control.0.as_raw_fd()) {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("jsh: internal Agent child control identity failed: {error}");
            return 2;
        }
    };
    let cwd_report = match AgentChildCwdReport::from_env(control.0.as_raw_fd(), control_identity) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("jsh: internal Agent child cwd report setup failed: {error}");
            return 2;
        }
    };
    let cwd_nonce = match read_agent_child_cwd_nonce_from_env(
        &[control.0.as_raw_fd(), cwd_report.descriptor],
        &[control_identity, cwd_report.identity],
    ) {
        Ok(nonce) => nonce,
        Err(error) => {
            eprintln!("jsh: internal Agent child cwd nonce setup failed: {error}");
            return 2;
        }
    };
    close_inherited_descriptors_except(&[control.0.as_raw_fd(), cwd_report.descriptor]);
    let Some(state_dir) = std::env::var_os(AGENT_CHILD_STATE_DIR_ENV).map(PathBuf::from) else {
        eprintln!("jsh: internal Agent child is missing its state directory");
        return 2;
    };
    let Some(agent_cwd) = std::env::var_os(AGENT_CHILD_CWD_ENV).map(PathBuf::from) else {
        eprintln!("jsh: internal Agent child is missing its working directory");
        return 2;
    };
    for name in [
        AGENT_CHILD_STATE_DIR_ENV,
        AGENT_CHILD_CWD_ENV,
        AGENT_CHILD_COMMAND_ENV,
        AGENT_CHILD_CONTROL_FD_ENV,
        AGENT_CHILD_CWD_REPORT_FD_ENV,
        AGENT_CHILD_CWD_NONCE_FD_ENV,
    ] {
        std::env::remove_var(name);
    }

    let snapshot_path = state_dir.join(format!("{AGENT_CHILD_SESSION_ID}.json"));
    let claim_dir = state_dir.join(AGENT_CHILD_CLAIM_DIR);
    let claimed_snapshot = claim_dir.join(format!("{AGENT_CHILD_SESSION_ID}.json"));
    // `rename` within this private directory is the one-shot claim. Two child
    // processes may be launched with the same capability, but only one can
    // move the source name and therefore reach command execution.
    if let Err(error) = fs::rename(&snapshot_path, &claimed_snapshot) {
        eprintln!("jsh: internal Agent state claim failed: {error}");
        return 1;
    }
    let snapshot_result =
        crate::session::SessionSnapshot::load_from_dir(AGENT_CHILD_SESSION_ID, &claim_dir);
    // The command must not be able to discover or read the serialized shell
    // state through its inherited environment or either known snapshot name.
    let _ = fs::remove_file(&claimed_snapshot);
    let snapshot = match snapshot_result {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("jsh: internal Agent state load failed: {error}");
            return 1;
        }
    };

    crate::builtins::reset_exit_request();
    crate::signal::reset_pending_signals();
    crate::signal::install_noninteractive_signals();
    let mut state = ShellState::new(false);
    snapshot.apply(&mut state);
    for name in [
        AGENT_CHILD_STATE_DIR_ENV,
        AGENT_CHILD_CWD_ENV,
        AGENT_CHILD_COMMAND_ENV,
        AGENT_CHILD_CONTROL_FD_ENV,
        AGENT_CHILD_CWD_REPORT_FD_ENV,
        AGENT_CHILD_CWD_NONCE_FD_ENV,
    ] {
        state.unset_var(name);
        std::env::remove_var(name);
    }
    if let Err(error) = std::env::set_current_dir(&agent_cwd) {
        eprintln!("jsh: Agent cwd is unavailable: {error}");
        return 1;
    }
    state.export_var("PWD", &agent_cwd.to_string_lossy());

    // This is the exact execution boundary. Claim, decode, state restore, and
    // cwd setup have all succeeded; failures before this byte are never
    // reported to the Agent as a real shell exit. Consuming the writer closes
    // it before parsing or executing attacker-influenced command text.
    if let Err(error) = control.signal_ready() {
        eprintln!("jsh: Agent command readiness failed: {error}");
        return 1;
    }

    let code = match crate::parser::parse(command) {
        Ok(commands) => crate::executor::execute_program(&commands, &mut state),
        Err(error) => {
            eprintln!("jsh: parse error: {error}");
            2
        }
    };

    if let Ok(final_cwd) = std::env::current_dir() {
        if let Err(error) = cwd_report.finish(&final_cwd, &cwd_nonce) {
            eprintln!("jsh: Agent cwd report failed: {error}");
        }
    }
    code
}

fn agent_child_command(
    command: &str,
    transport: &AgentChildTransport,
    cwd: &Path,
    control_writer: &std::os::fd::OwnedFd,
    cwd_report_writer: &std::os::fd::OwnedFd,
    cwd_nonce_reader: &std::os::fd::OwnedFd,
) -> std::io::Result<std::process::Command> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let channel_identities = [
        agent_child_pipe_identity(control_writer.as_raw_fd())?,
        agent_child_pipe_identity(cwd_report_writer.as_raw_fd())?,
        agent_child_pipe_identity(cwd_nonce_reader.as_raw_fd())?,
    ];
    if !agent_child_pipe_identities_are_distinct(&channel_identities) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Agent child channels must use three distinct pipes",
        ));
    }

    let mut child = std::process::Command::new(std::env::current_exe()?);
    #[cfg(not(test))]
    child.args([INTERNAL_AGENT_CHILD_FLAG, command]);
    #[cfg(test)]
    child
        .args([
            "agent::tests::internal_agent_child_process",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env(AGENT_CHILD_COMMAND_ENV, command);
    child
        .env(AGENT_CHILD_STATE_DIR_ENV, &transport.dir)
        .env(AGENT_CHILD_CWD_ENV, cwd)
        .env(
            AGENT_CHILD_CONTROL_FD_ENV,
            control_writer.as_raw_fd().to_string(),
        )
        .env(
            AGENT_CHILD_CWD_REPORT_FD_ENV,
            cwd_report_writer.as_raw_fd().to_string(),
        )
        .env(
            AGENT_CHILD_CWD_NONCE_FD_ENV,
            cwd_nonce_reader.as_raw_fd().to_string(),
        );
    let control_descriptor = control_writer.as_raw_fd();
    let cwd_report_descriptor = cwd_report_writer.as_raw_fd();
    let cwd_nonce_descriptor = cwd_nonce_reader.as_raw_fd();
    // SAFETY: the closure performs only fcntl on already-open pipe
    // descriptors. Clearing CLOEXEC in this forked child does not change the
    // parent's descriptor table or leak through concurrent parent spawns.
    unsafe {
        child.pre_exec(move || {
            for descriptor in [
                control_descriptor,
                cwd_report_descriptor,
                cwd_nonce_descriptor,
            ] {
                let flags = nix::libc::fcntl(descriptor, nix::libc::F_GETFD);
                if flags < 0
                    || nix::libc::fcntl(
                        descriptor,
                        nix::libc::F_SETFD,
                        flags & !nix::libc::FD_CLOEXEC,
                    ) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(child)
}

/// Run one approved command through a fresh jsh process, teeing combined
/// stdout+stderr to the terminal while capturing a bounded observation.
/// Interactive/TTY-dependent programs see a pipe; the Agent protocol already
/// biases toward non-interactive commands. A private one-shot snapshot gives
/// the child the current aliases/functions/options without running Rust code
/// after `fork()` in the multi-threaded interactive process.
fn run_captured(
    command: &str,
    state: &mut ShellState,
    agent_cwd: &mut PathBuf,
) -> CapturedExecution {
    let mut stdout = std::io::stdout();
    run_captured_to(command, state, agent_cwd, &mut stdout)
}

fn run_captured_to(
    command: &str,
    state: &mut ShellState,
    agent_cwd: &mut PathBuf,
    terminal_output: &mut dyn Write,
) -> CapturedExecution {
    let shell_cwd = std::env::current_dir().unwrap_or_default();
    if *agent_cwd == shell_cwd {
        println!(
            "  {}",
            dim(&format!(
                "$ {}",
                terminal_safe_inline_text(command, MAX_AGENT_COMMAND_DISPLAY_BYTES)
            ))
        );
    } else {
        println!(
            "  {}",
            dim(&format!(
                "$ {}   (in {})",
                terminal_safe_inline_text(command, MAX_AGENT_COMMAND_DISPLAY_BYTES),
                terminal_safe_inline_text(
                    &agent_cwd.display().to_string(),
                    MAX_AGENT_DISPLAY_BYTES
                )
            ))
        );
    }
    let transport = match AgentChildTransport::new(state, agent_cwd) {
        Ok(transport) => transport,
        Err(error) => {
            return CapturedExecution::failed(
                CommandExecutionFailure::FailedToStart,
                format!("[jsh: Agent state snapshot failed: {error}]"),
            )
        }
    };
    run_captured_with_transport(command, &transport, agent_cwd, terminal_output)
}

fn run_captured_with_transport(
    command: &str,
    transport: &AgentChildTransport,
    agent_cwd: &mut PathBuf,
    terminal_output: &mut dyn Write,
) -> CapturedExecution {
    use nix::unistd::{close, read};
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, IntoRawFd};
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    let (control_reader, control_writer) = match capture_pipe_cloexec() {
        Ok(pipe) => pipe,
        Err(error) => {
            return CapturedExecution::failed(
                CommandExecutionFailure::FailedToStart,
                format!("[jsh: Agent readiness pipe failed: {error}]"),
            )
        }
    };
    let (cwd_report_reader, cwd_report_writer) = match capture_pipe_cloexec() {
        Ok(pipe) => pipe,
        Err(error) => {
            return CapturedExecution::failed(
                CommandExecutionFailure::FailedToStart,
                format!("[jsh: Agent cwd report pipe failed: {error}]"),
            )
        }
    };
    let (cwd_nonce, cwd_nonce_reader) = match agent_child_cwd_nonce_channel() {
        Ok(channel) => channel,
        Err(error) => {
            return CapturedExecution::failed(
                CommandExecutionFailure::FailedToStart,
                format!("[jsh: Agent cwd nonce channel failed: {error}]"),
            )
        }
    };
    // Neither end may leak through the exec that starts the one-shot jsh.
    // `stdout_file` is duplicated onto fd 1 by `Command`; dup2 clears
    // FD_CLOEXEC on that destination, while the original writer and, most
    // importantly, the reader disappear at exec. Without this, a background
    // command inherits both ends of its own capture pipe. Closing `r` here
    // then cannot deliver SIGPIPE to a continuously writing descendant, which
    // leaves the descendant and its waiting jsh parent orphaned indefinitely.
    let (r, w) = match capture_pipe_cloexec() {
        Ok(fds) => (fds.0.into_raw_fd(), fds.1),
        Err(error) => {
            return CapturedExecution::failed(
                CommandExecutionFailure::FailedToStart,
                format!("[jsh: pipe failed: {error}]"),
            )
        }
    };
    let stdout_file = File::from(w);
    let stderr_file = match stdout_file.try_clone() {
        Ok(file) => file,
        Err(error) => {
            close(r).ok();
            return CapturedExecution::failed(
                CommandExecutionFailure::FailedToStart,
                format!("[jsh: pipe clone failed: {error}]"),
            );
        }
    };
    let mut child_command = match agent_child_command(
        command,
        transport,
        agent_cwd,
        &control_writer,
        &cwd_report_writer,
        &cwd_nonce_reader,
    ) {
        Ok(command) => command,
        Err(error) => {
            close(r).ok();
            return CapturedExecution::failed(
                CommandExecutionFailure::FailedToStart,
                format!("[jsh: Agent child setup failed: {error}]"),
            );
        }
    };
    child_command
        .stdin(Stdio::inherit())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    let mut child = match child_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            close(r).ok();
            return CapturedExecution::failed(
                CommandExecutionFailure::FailedToStart,
                format!("[jsh: Agent child spawn failed: {error}]"),
            );
        }
    };
    // The forked child owns the only remaining writer. EOF without its READY
    // byte is therefore authoritative evidence that user command execution
    // was never entered.
    drop(control_writer);
    // The internal child owns the sole report writer. It restores CLOEXEC as
    // soon as it takes the descriptor, so approved external processes cannot
    // delay EOF or synthesize a cwd frame.
    drop(cwd_report_writer);
    // The child consumes and closes the pre-filled nonce reader before it
    // restores or executes shell state; the parent keeps only its own copy.
    drop(cwd_nonce_reader);
    let child_pid = i32::try_from(child.id()).ok();
    drop(child_command);

    let flags = unsafe { nix::libc::fcntl(r, nix::libc::F_GETFL) };
    let control_flags = unsafe { nix::libc::fcntl(control_reader.as_raw_fd(), nix::libc::F_GETFL) };
    let cwd_report_flags =
        unsafe { nix::libc::fcntl(cwd_report_reader.as_raw_fd(), nix::libc::F_GETFL) };
    if flags < 0
        || unsafe { nix::libc::fcntl(r, nix::libc::F_SETFL, flags | nix::libc::O_NONBLOCK) } < 0
        || control_flags < 0
        || unsafe {
            nix::libc::fcntl(
                control_reader.as_raw_fd(),
                nix::libc::F_SETFL,
                control_flags | nix::libc::O_NONBLOCK,
            )
        } < 0
        || cwd_report_flags < 0
        || unsafe {
            nix::libc::fcntl(
                cwd_report_reader.as_raw_fd(),
                nix::libc::F_SETFL,
                cwd_report_flags | nix::libc::O_NONBLOCK,
            )
        } < 0
    {
        let _ = child.kill();
        let _ = child.wait();
        close(r).ok();
        return CapturedExecution::failed(
            CommandExecutionFailure::Cancelled,
            "[jsh: capture setup failed after the Agent child started]",
        );
    }
    let mut cwd_report_reader = Some(cwd_report_reader);

    let mut captured: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 4096];
    let mut child_status: Option<Result<i32, String>> = None;
    let mut output_open = true;
    let mut post_exit_bytes = 0usize;
    let mut forwarded_signal = None;
    let mut readiness = AgentChildReadiness::default();
    let mut cwd_report = AgentChildCwdFrameBuffer::default();
    let mut refresh_child_status = |status: &mut Option<Result<i32, String>>| {
        if status.is_some() {
            return;
        }
        match child.try_wait() {
            Ok(Some(exit)) => {
                *status = Some(match (exit.code(), exit.signal()) {
                    (Some(code), _) => Ok(code),
                    (None, Some(signal)) => Err(format!(
                        "[jsh: Agent command boundary terminated by signal {signal}]"
                    )),
                    (None, None) => {
                        Err("[jsh: Agent command boundary ended without a status]".into())
                    }
                });
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                *status = Some(Err(format!(
                    "[jsh: could not observe Agent command completion: {error}]"
                )));
            }
        }
    };
    'capture: loop {
        drain_agent_child_readiness(control_reader.as_fd(), &mut readiness);
        drain_agent_child_cwd_reader(&mut cwd_report_reader, &mut cwd_report, &cwd_nonce);
        if let Some(status) = crate::signal::pending_status() {
            let signal = status.saturating_sub(128);
            if signal > 0 && forwarded_signal != Some(signal) {
                // The interactive shell received the terminal signal. Relay
                // it to the one-shot jsh child, whose noninteractive handler
                // forwards it again to any foreground command process group.
                if let Some(child_pid) = child_pid {
                    unsafe {
                        nix::libc::kill(child_pid, signal);
                    }
                }
                forwarded_signal = Some(signal);
            }
        }
        refresh_child_status(&mut child_status);

        while output_open {
            match unsafe { read(BorrowedFd::borrow_raw(r), &mut buffer) } {
                Ok(0) => {
                    output_open = false;
                    break;
                }
                Ok(count) => {
                    let chunk = &buffer[..count];
                    let _ = terminal_output.write_all(chunk);
                    let _ = terminal_output.flush();
                    if captured.len() < MAX_CAPTURED_OUTPUT_BYTES {
                        let room = MAX_CAPTURED_OUTPUT_BYTES - captured.len();
                        captured.extend_from_slice(&chunk[..count.min(room)]);
                        if room <= count {
                            truncated = true;
                        }
                    } else {
                        truncated = true;
                    }
                    // A continuously writing detached descendant can keep the
                    // pipe readable forever. Check the direct jsh child
                    // between chunks rather than waiting for EAGAIN.
                    refresh_child_status(&mut child_status);
                    if child_status.is_some() {
                        post_exit_bytes = post_exit_bytes.saturating_add(count);
                        if post_exit_bytes >= MAX_POST_EXIT_DRAIN_BYTES {
                            truncated = true;
                            break 'capture;
                        }
                    }
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(nix::errno::Errno::EAGAIN) => break,
                Err(_) => {
                    output_open = false;
                    truncated = true;
                    break;
                }
            }
        }

        drain_agent_child_readiness(control_reader.as_fd(), &mut readiness);
        drain_agent_child_cwd_reader(&mut cwd_report_reader, &mut cwd_report, &cwd_nonce);

        if child_status.is_some() {
            // A READY written before exit is ordered before the writer closes;
            // one last drain observes it even when the process was very short.
            drain_agent_child_readiness(control_reader.as_fd(), &mut readiness);
            drain_agent_child_cwd_reader(&mut cwd_report_reader, &mut cwd_report, &cwd_nonce);
            break;
        }
        let mut descriptors = [
            nix::libc::pollfd {
                fd: if output_open { r } else { -1 },
                events: nix::libc::POLLIN | nix::libc::POLLHUP | nix::libc::POLLERR,
                revents: 0,
            },
            nix::libc::pollfd {
                fd: cwd_report_reader
                    .as_ref()
                    .map_or(-1, std::os::fd::AsRawFd::as_raw_fd),
                events: nix::libc::POLLIN | nix::libc::POLLHUP | nix::libc::POLLERR,
                revents: 0,
            },
        ];
        unsafe {
            nix::libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, 100);
        }
    }
    close(r).ok();
    drain_agent_child_cwd_reader(&mut cwd_report_reader, &mut cwd_report, &cwd_nonce);
    let child_status = child_status.unwrap_or_else(|| {
        Err("[jsh: Agent command boundary ended without an observable status]".into())
    });
    if readiness.authenticated() {
        if let Ok(reported) = cwd_report.decode(&cwd_nonce) {
            if reported != *agent_cwd && reported.is_dir() {
                println!(
                    "  {}",
                    dim(&format!(
                        "cwd → {}",
                        terminal_safe_inline_text(
                            &reported.as_os_str().to_string_lossy(),
                            MAX_AGENT_DISPLAY_BYTES
                        )
                    ))
                );
                *agent_cwd = reported;
            }
        }
    }
    let mut output = String::from_utf8_lossy(&captured).to_string();
    if truncated {
        output.push_str("\n[jsh: further output not captured]");
    }
    if readiness.invalid() || !readiness.closed {
        let mut detail = String::from(
            "[jsh: Agent readiness channel failed; command execution result is unknown]",
        );
        if !output.trim().is_empty() {
            detail.push('\n');
            detail.push_str(&output);
        }
        return CapturedExecution::failed(CommandExecutionFailure::Cancelled, detail);
    }
    if !readiness.ready {
        let mut detail =
            String::from("[jsh: Agent child failed before entering user command execution]");
        if !output.trim().is_empty() {
            detail.push('\n');
            detail.push_str(&output);
        }
        return CapturedExecution::failed(CommandExecutionFailure::FailedToStart, detail);
    }

    match child_status {
        Ok(exit_code) => CapturedExecution::exited(exit_code, output),
        Err(mut detail) => {
            if !output.trim().is_empty() {
                detail.push('\n');
                detail.push_str(&output);
            }
            CapturedExecution::failed(CommandExecutionFailure::Cancelled, detail)
        }
    }
}

fn max_turns() -> u32 {
    let configured = std::env::var("JSH_AGENT_MAX_TURNS").ok();
    configured_max_turns(configured.as_deref())
}

fn configured_max_turns(value: Option<&str>) -> u32 {
    value
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|turns| *turns > 0)
        .unwrap_or(DEFAULT_MAX_TURNS)
        .min(MAX_AGENT_SESSION_TURNS)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentProtocolConfigError {
    InvalidProtocol,
    InvalidPeer(CapabilityError),
    UnsupportedSelection(AgentProtocol),
}

impl std::fmt::Display for AgentProtocolConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProtocol => write!(
                formatter,
                "{AGENT_PROTOCOL_ENV} must be 'text' or 'native-tools' (text is the compatible default)"
            ),
            Self::InvalidPeer(CapabilityError::TooLarge) => write!(
                formatter,
                "{AGENT_PEER_CAPABILITIES_ENV} exceeds the bounded capability-token limit"
            ),
            Self::InvalidPeer(CapabilityError::Malformed) => write!(
                formatter,
                "{AGENT_PEER_CAPABILITIES_ENV} is not a canonical agent capability token"
            ),
            Self::InvalidPeer(CapabilityError::UnsupportedVersion(_)) => write!(
                formatter,
                "{AGENT_PEER_CAPABILITIES_ENV} uses an unsupported capability version"
            ),
            Self::UnsupportedSelection(protocol) => write!(
                formatter,
                "agent protocol '{}' is not supported by both the configured provider and peer for complete delivery",
                protocol.as_wire_name()
            ),
        }
    }
}

fn requested_agent_protocol(
    value: Option<&str>,
) -> Result<AgentProtocol, AgentProtocolConfigError> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("text") => Ok(AgentProtocol::Text),
        Some("native-tools" | "native_tools" | "tools") => Ok(AgentProtocol::NativeTools),
        Some(_) => Err(AgentProtocolConfigError::InvalidProtocol),
    }
}

fn configured_agent_protocol(
    provider: Provider,
    protocol_value: Option<&str>,
    peer_value: Option<&str>,
) -> Result<AgentProtocol, AgentProtocolConfigError> {
    let protocol = requested_agent_protocol(protocol_value)?;
    let peer = AgentCapabilities::from_wire(peer_value.unwrap_or(LEGACY_AGENT_PEER_CAPABILITIES))
        .map_err(AgentProtocolConfigError::InvalidPeer)?;
    // Reply in the peer's schema version so an exact-pair v2 capability set
    // remains exact if a provider's matrix becomes asymmetric in the future.
    // Compatibility-first v1 peers still receive the legacy Cartesian form.
    let local = agent_capabilities_for_peer(provider, peer);
    local
        .negotiate_with(peer, &[protocol], AgentDelivery::Complete)
        .ok_or(AgentProtocolConfigError::UnsupportedSelection(protocol))
}

pub(crate) fn configured_agent_protocol_from_env(
    provider: Provider,
) -> Result<AgentProtocol, AgentProtocolConfigError> {
    let protocol = match std::env::var(AGENT_PROTOCOL_ENV) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(AgentProtocolConfigError::InvalidProtocol)
        }
    };
    let peer = match std::env::var(AGENT_PEER_CAPABILITIES_ENV) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(AgentProtocolConfigError::InvalidPeer(
                CapabilityError::Malformed,
            ))
        }
    };
    configured_agent_protocol(provider, protocol.as_deref(), peer.as_deref())
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn read_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    loop {
        if crate::signal::pending_status().is_some() {
            return None;
        }
        let mut descriptor = nix::libc::pollfd {
            fd: nix::libc::STDIN_FILENO,
            events: nix::libc::POLLIN | nix::libc::POLLHUP | nix::libc::POLLERR,
            revents: 0,
        };
        let result = unsafe { nix::libc::poll(&mut descriptor, 1, 100) };
        if result > 0 {
            break;
        }
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
    if crate::signal::pending_status().is_some() {
        return None;
    }
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
    }
}

fn confirm(prompt: &str) -> bool {
    matches!(
        read_line(prompt).as_deref().map(str::trim),
        Some("y" | "yes")
    )
}

fn status_line(text: &str) {
    println!("{}", dim(&format!("[agent] {text}")));
}

fn styled(code: &str, text: &str) -> String {
    if std::io::stdout().is_terminal() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn emphasize(text: &str) -> String {
    styled("1", text)
}

fn warn(text: &str) -> String {
    styled("31", text)
}

fn dim(text: &str) -> String {
    styled("2", text)
}

/// Unicode controls that can reorder, conceal, or annotate visible terminal
/// text without using a C0/C1 control byte. These are especially dangerous in
/// an exact-command approval card.
/// Render untrusted model, provider, or filesystem text without letting a
/// control sequence or invisible Unicode formatting instruction reach the
/// user's terminal. The rendered result is byte-bounded on UTF-8 boundaries.
fn render_terminal_safe_text(
    value: &str,
    max_bytes: usize,
    initial_prefix: &str,
    continuation_prefix: Option<&str>,
    preserve_newlines: bool,
) -> String {
    let mut output = initial_prefix
        .get(..max_bytes.min(initial_prefix.len()))
        .unwrap_or("")
        .to_string();
    for ch in value.chars() {
        let rendered = match ch {
            '\n' if continuation_prefix.is_some() => {
                format!("\n{}", continuation_prefix.unwrap_or_default())
            }
            '\n' if preserve_newlines => "\n".to_string(),
            '\n' => "\\n".to_string(),
            '\t' => "\\t".to_string(),
            '\r' => "\\r".to_string(),
            ch if crate::terminal_text::is_terminal_ambiguous(ch) && u32::from(ch) <= 0x7f => {
                format!("\\x{:02x}", u32::from(ch))
            }
            ch if crate::terminal_text::is_terminal_ambiguous(ch) => {
                format!("\\u{{{:x}}}", u32::from(ch))
            }
            ch => ch.to_string(),
        };
        if output.len().saturating_add(rendered.len()) > max_bytes {
            if output.len().saturating_add('…'.len_utf8()) <= max_bytes {
                output.push('…');
            }
            break;
        }
        output.push_str(&rendered);
    }
    output
}

fn terminal_safe_text(value: &str, max_bytes: usize) -> String {
    render_terminal_safe_text(value, max_bytes, "", None, true)
}

fn terminal_safe_inline_text(value: &str, max_bytes: usize) -> String {
    render_terminal_safe_text(value, max_bytes, "", None, false)
}

fn terminal_safe_message(prefix: &str, value: &str, max_bytes: usize) -> String {
    render_terminal_safe_text(value, max_bytes, prefix, Some(prefix), true)
}

#[cfg(test)]
mod tests {
    use super::{
        agent_child_command, agent_child_cwd_nonce_channel, agent_http_client, bounded_git_stdout,
        capture_pipe_cloexec, configured_agent_protocol, configured_max_turns,
        decode_agent_child_cwd_frame, drain_agent_child_readiness, encode_agent_child_cwd_frame,
        git_meta, git_worktree_dirty, read_model_response_bounded, run_captured, run_captured_to,
        run_captured_with_transport, run_internal_agent_child, run_internal_model_request,
        terminal_safe_inline_text, terminal_safe_message, terminal_safe_text, AgentChildReadiness,
        AgentChildTransport, AgentProtocolConfigError, CapturedExecution, AGENT_CHILD_COMMAND_ENV,
        AGENT_CHILD_READY, MAX_AGENT_CHILD_CWD_BYTES, MAX_AGENT_CHILD_CWD_FRAME_BYTES,
        MAX_AGENT_COMMAND_DISPLAY_BYTES, MAX_AGENT_DISPLAY_BYTES, MAX_AGENT_RESPONSE_BYTES,
        MAX_AGENT_SESSION_TURNS,
    };
    use super::{decode_model_request, encode_model_request};
    use crate::environment::ShellState;
    use jagent::provider::{ChatConfig, HttpRequest, Message, Provider, Role};
    use jagent::{
        prepare_agent_request as prepare_request, AgentProtocol, AgentRequestSpec as RequestSpec,
        AgentSession, CommandExecutionFailure, ModelOutcome, Turn,
    };
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    #[test]
    fn cwd_report_frame_is_absolute_bounded_and_exactly_one_message() {
        use std::os::unix::ffi::OsStringExt;

        let cwd = PathBuf::from("/tmp/agent-cwd");
        let nonce = [b'n'; 32];
        let frame = encode_agent_child_cwd_frame(&cwd, &nonce).unwrap();
        assert_eq!(decode_agent_child_cwd_frame(&frame, &nonce).unwrap(), cwd);
        assert!(decode_agent_child_cwd_frame(&frame, &[b'x'; 32]).is_err());

        for invalid in [
            Vec::new(),
            frame[..7].to_vec(),
            frame[..frame.len() - 1].to_vec(),
            {
                let mut extra = frame.clone();
                extra.push(b'x');
                extra
            },
        ] {
            assert!(
                decode_agent_child_cwd_frame(&invalid, &nonce).is_err(),
                "accepted malformed cwd frame {invalid:?}"
            );
        }
        assert!(encode_agent_child_cwd_frame(Path::new("relative/path"), &nonce).is_err());
        assert!(encode_agent_child_cwd_frame(
            &PathBuf::from(format!("/{}", "x".repeat(MAX_AGENT_CHILD_CWD_BYTES))),
            &nonce,
        )
        .is_err());

        let nul_path = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/nul\0path".to_vec()));
        assert!(encode_agent_child_cwd_frame(&nul_path, &nonce).is_err());
        let mut nul_frame = frame;
        *nul_frame.last_mut().unwrap() = 0;
        assert!(decode_agent_child_cwd_frame(&nul_frame, &nonce).is_err());
    }

    #[test]
    fn inherited_descriptor_spelling_is_canonical_decimal() {
        use std::ffi::OsStr;

        assert_eq!(
            super::parse_agent_child_descriptor(OsStr::new("37")),
            Some(37)
        );
        for invalid in [
            "",
            "+37",
            " 37",
            "37 ",
            "037",
            "-37",
            "٣٧",
            "2147483648",
            "99999999999",
        ] {
            assert_eq!(
                super::parse_agent_child_descriptor(OsStr::new(invalid)),
                None,
                "accepted descriptor spelling {invalid:?}"
            );
        }
    }

    #[test]
    fn cwd_nonce_comparison_requires_an_exact_full_match() {
        let expected = [0x5a; 32];
        assert!(super::agent_child_nonce_matches(&expected, &expected));
        for index in [0, 15, 31] {
            let mut different = expected;
            different[index] ^= 1;
            assert!(!super::agent_child_nonce_matches(&different, &expected));
        }
        assert!(!super::agent_child_nonce_matches(
            &expected[..31],
            &expected
        ));
    }

    #[test]
    fn cwd_nonce_channel_requires_exact_length_and_eof() {
        let nonce = [0x7a; 32];
        let mut exact = nonce.as_slice();
        assert_eq!(
            super::read_exact_agent_child_cwd_nonce(&mut exact).unwrap(),
            nonce
        );
        let mut short = &nonce[..31];
        assert!(super::read_exact_agent_child_cwd_nonce(&mut short).is_err());
        let mut extra = nonce.to_vec();
        extra.push(0xff);
        let mut extra = extra.as_slice();
        assert!(super::read_exact_agent_child_cwd_nonce(&mut extra).is_err());
    }

    #[test]
    fn a_model_request_envelope_round_trips() {
        let request = HttpRequest {
            url: "https://api.example.test/v1/messages".to_string(),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-api-key".to_string(), "sk-secret".to_string()),
            ],
            body: r#"{"model":"m","messages":[]}"#.to_string(),
        };
        for provider in [
            Provider::Anthropic,
            Provider::OpenAiCompatible,
            Provider::Ollama,
        ] {
            let encoded = encode_model_request(&request, provider);
            let (decoded, decoded_provider) = decode_model_request(&encoded).expect("round trip");
            assert_eq!(decoded, request);
            assert_eq!(decoded_provider, provider);
        }
    }

    #[test]
    fn decoded_model_response_has_an_outer_allocation_bound() {
        let exact = vec![b'x'; MAX_AGENT_RESPONSE_BYTES as usize];
        assert_eq!(
            read_model_response_bounded(std::io::Cursor::new(exact))
                .unwrap()
                .len(),
            MAX_AGENT_RESPONSE_BYTES as usize
        );

        let expanded = vec![b'x'; MAX_AGENT_RESPONSE_BYTES as usize + 1];
        let error = read_model_response_bounded(std::io::Cursor::new(expanded)).unwrap_err();
        assert!(error.contains("decoded limit"), "{error}");
    }

    #[test]
    fn a_malformed_envelope_is_refused_rather_than_guessed() {
        // The envelope carries a URL and an API key into a request this process
        // is about to make. Every field is required; none is defaulted.
        for envelope in [
            "",
            "not json",
            r#"{"v":2,"provider":"ollama","url":"http://x","headers":[],"body":"{}"}"#,
            r#"{"v":1,"provider":"unknown","url":"http://x","headers":[],"body":"{}"}"#,
            r#"{"v":1,"provider":"ollama","headers":[],"body":"{}"}"#,
            r#"{"v":1,"provider":"ollama","url":"http://x","body":"{}"}"#,
            r#"{"v":1,"provider":"ollama","url":"http://x","headers":[],"body":1}"#,
            r#"{"v":1,"provider":"ollama","url":"http://x","headers":[["only-one"]],"body":"{}"}"#,
            r#"{"v":1,"provider":"ollama","url":"http://x","headers":[[1,2]],"body":"{}"}"#,
            r#"{"v":1,"v":1,"provider":"ollama","url":"http://x","headers":[],"body":"{}"}"#,
            r#"{"v":1,"provider":"ollama","provider":"ollama","url":"http://x","headers":[],"body":"{}"}"#,
            r#"{"v":1,"provider":"ollama","url":"http://x","url":"http://x","headers":[],"body":"{}"}"#,
            r#"{"v":1,"provider":"ollama","url":"http://x","headers":[],"headers":[],"body":"{}"}"#,
            r#"{"v":1,"provider":"ollama","url":"http://x","headers":[],"body":"{}","body":"{}"}"#,
            r#"{"v":1,"provider":"ollama","url":"http://x","headers":[],"body":"{}","future":true}"#,
            r#"{"v":1,"provider":"ollama","url":"http://x","headers":[],"body":"{}"} trailing"#,
        ] {
            assert!(
                decode_model_request(envelope).is_err(),
                "accepted {envelope:?}"
            );
        }
    }

    #[test]
    fn the_transport_child_only_answers_to_its_own_flag() {
        let args = |values: &[&str]| -> Vec<std::ffi::OsString> {
            values.iter().map(std::ffi::OsString::from).collect()
        };
        assert_eq!(run_internal_model_request(&args(&["jsh"])), None);
        assert_eq!(
            run_internal_model_request(&args(&["jsh", "-c", "echo"])),
            None
        );
        assert_eq!(
            run_internal_model_request(&args(&["jsh", "--jsh-internal-agent-child", "x"])),
            None
        );
        // Right flag, extra operands: refused rather than ignored, so a stray
        // argument can never be read as part of the request.
        assert_eq!(
            run_internal_model_request(&args(&["jsh", "--jsh-internal-model-request", "extra"])),
            Some(2)
        );
    }

    #[test]
    fn agent_provider_credentials_never_cross_redirects() {
        assert_eq!(agent_http_client(false).config().max_redirects(), 0);
        assert!(agent_http_client(true).config().proxy().is_none());
    }

    #[test]
    #[ignore = "one-shot helper process for Agent command capture tests"]
    fn internal_agent_child_process() {
        use std::io::Write as _;

        let command = std::env::var(AGENT_CHILD_COMMAND_ENV).expect("child command");
        let status = run_internal_agent_child(&command);
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        std::process::exit(status);
    }

    #[test]
    fn one_shot_snapshot_is_atomically_claimed_by_only_one_process() {
        use std::io::Read as _;
        use std::process::Stdio;

        let temp = tempfile::tempdir().expect("tempdir");
        let marker = temp.path().join("executions");
        let state = ShellState::new(false);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let transport = AgentChildTransport::new(&state, &cwd).expect("Agent transport");
        // Use an external writer so Rust's test-harness output capture cannot
        // intercept the builtin's stdout before the shell redirection.
        let command = format!("/usr/bin/printf x >> '{}'", marker.display());

        let spawn = || {
            let (control_reader, control_writer) =
                capture_pipe_cloexec().expect("child control pipe");
            let (cwd_reader, cwd_writer) = capture_pipe_cloexec().expect("child cwd pipe");
            let (cwd_nonce, cwd_nonce_reader) =
                agent_child_cwd_nonce_channel().expect("child cwd nonce");
            let mut command = agent_child_command(
                &command,
                &transport,
                &cwd,
                &control_writer,
                &cwd_writer,
                &cwd_nonce_reader,
            )
            .expect("child command");
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let child = command.spawn().expect("spawn Agent child");
            drop(control_writer);
            drop(cwd_writer);
            drop(cwd_nonce_reader);
            (child, control_reader, cwd_reader, cwd_nonce)
        };
        let (mut first, first_control, first_cwd, first_nonce) = spawn();
        let (mut second, second_control, second_cwd, second_nonce) = spawn();
        let read_control = |reader: std::os::fd::OwnedFd| {
            let mut bytes = Vec::new();
            std::fs::File::from(reader)
                .read_to_end(&mut bytes)
                .expect("read child control");
            bytes
        };
        let read_cwd = |reader: std::os::fd::OwnedFd| {
            let mut bytes = Vec::new();
            std::fs::File::from(reader)
                .read_to_end(&mut bytes)
                .expect("read child cwd");
            bytes
        };
        let mut outcomes = [
            (
                first.wait().expect("first Agent child").code(),
                read_control(first_control),
                read_cwd(first_cwd),
                first_nonce,
            ),
            (
                second.wait().expect("second Agent child").code(),
                read_control(second_control),
                read_cwd(second_cwd),
                second_nonce,
            ),
        ];
        outcomes.sort_by_key(|(status, _, _, _)| *status);

        assert_eq!(outcomes[0].0, Some(0));
        assert_eq!(outcomes[0].1, vec![AGENT_CHILD_READY]);
        assert_eq!(
            decode_agent_child_cwd_frame(&outcomes[0].2, &outcomes[0].3).unwrap(),
            cwd
        );
        assert_eq!(outcomes[1].0, Some(1));
        assert!(outcomes[1].1.is_empty());
        assert!(outcomes[1].2.is_empty());
        assert_eq!(
            std::fs::read_to_string(marker).expect("execution marker"),
            "x"
        );
    }

    #[test]
    fn pre_spawn_failure_has_no_fake_exit_status_and_restores_as_failure() {
        let mut state = ShellState::new(false);
        // Force the private child snapshot over its 4 MiB encoded ceiling.
        // This deterministically fails before Command::spawn, exercising the
        // production setup-failure route rather than only constructing the
        // enum directly in the test.
        state
            .env_vars
            .insert("AGENT_OVERSIZED_STATE".into(), "x".repeat(5 * 1024 * 1024));
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut terminal_output = std::io::sink();
        let execution: CapturedExecution = run_captured_to(
            "/usr/bin/printf must-not-run",
            &mut state,
            &mut cwd,
            &mut terminal_output,
        );
        assert_eq!(execution.exit_code(), None);
        assert_eq!(
            execution.failure(),
            Some(CommandExecutionFailure::FailedToStart)
        );
        assert!(execution.evidence().contains("state snapshot failed"));

        let mut session = AgentSession::new(4);
        session.submit_user("inspect").unwrap();
        let ModelOutcome::Proposal { id, .. } = session
            .accept_model_reply(r#"{"action":"run","command":"check"}"#)
            .unwrap()
        else {
            panic!("expected proposal")
        };
        let _approved = session.approve(id).unwrap();
        session.observe_execution(id, execution).unwrap();
        assert!(matches!(
            session.transcript().last(),
            Some(Turn::ProtocolError(message))
                if message.contains("failed to start")
                    && message.contains("no normal exit status")
                    && !message.contains("Output (exit=1)")
        ));
        let restored = AgentSession::restore(session.snapshot().unwrap()).unwrap();
        assert_eq!(restored.state(), jagent::AgentState::AwaitingModel);
    }

    #[test]
    fn child_exit_one_before_ready_is_an_execution_failure() {
        let state = ShellState::new(false);
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let transport = AgentChildTransport::new(&state, &cwd).expect("Agent transport");
        // Make the already-created child capability unusable after the parent
        // setup phase. The spawned child will fail its one-shot claim and exit
        // 1, exactly the path that used to look like a real command failure.
        std::fs::remove_file(&transport.snapshot).expect("remove public snapshot");
        let temp = tempfile::tempdir().expect("execution marker directory");
        let marker = temp.path().join("must-not-exist");
        let command = format!("/usr/bin/printf ran > '{}'", marker.display());
        let mut terminal_output = std::io::sink();
        let execution =
            run_captured_with_transport(&command, &transport, &mut cwd, &mut terminal_output);
        assert_eq!(execution.exit_code(), None);
        assert_eq!(
            execution.failure(),
            Some(CommandExecutionFailure::FailedToStart)
        );
        assert!(execution
            .evidence()
            .contains("failed before entering user command execution"));
        assert!(
            !marker.exists(),
            "pre-ready child executed the user command"
        );

        let mut session = AgentSession::new(4);
        session.submit_user("inspect").unwrap();
        let ModelOutcome::Proposal { id, .. } = session
            .accept_model_reply(r#"{"action":"run","command":"check"}"#)
            .unwrap()
        else {
            panic!("expected proposal")
        };
        let _approved = session.approve(id).unwrap();
        session.observe_execution(id, execution).unwrap();
        assert!(matches!(
            session.transcript().last(),
            Some(Turn::ProtocolError(message))
                if message.contains("failed to start")
                    && message.contains("no normal exit status")
        ));
        assert!(AgentSession::restore(session.snapshot().unwrap()).is_ok());
    }

    #[test]
    fn real_exit_one_remains_a_normal_observation() {
        let mut state = ShellState::new(false);
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut terminal_output = std::io::sink();
        let execution = run_captured_to("false", &mut state, &mut cwd, &mut terminal_output);
        assert_eq!(execution.exit_code(), Some(1));
        assert_eq!(execution.failure(), None);

        let mut session = AgentSession::new(4);
        session.submit_user("inspect").unwrap();
        let ModelOutcome::Proposal { id, .. } = session
            .accept_model_reply(r#"{"action":"run","command":"false"}"#)
            .unwrap()
        else {
            panic!("expected proposal")
        };
        let _approved = session.approve(id).unwrap();
        session.observe_execution(id, execution).unwrap();
        assert!(matches!(
            session.transcript().last(),
            Some(Turn::Observation { exit_code: 1, .. })
        ));
        assert!(AgentSession::restore(session.snapshot().unwrap()).is_ok());
    }

    #[test]
    fn detached_descendant_cannot_hold_the_capture_loop_open() {
        let mut state = ShellState::new(false);
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let started = Instant::now();

        let execution = run_captured("sleep 1 &", &mut state, &mut cwd);

        assert_eq!(execution.exit_code(), Some(0));
        assert!(
            started.elapsed() < Duration::from_millis(750),
            "background stdout descriptor delayed Agent completion for {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn continuously_writing_detached_descendant_cannot_pin_capture() {
        let mut state = ShellState::new(false);
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut terminal_output = std::io::sink();
        let started = Instant::now();
        let temp = tempfile::tempdir().expect("pid directory");
        let pid_file = temp.path().join("background.pid");
        let quoted_pid_file = pid_file.display().to_string().replace('\'', "'\\''");
        let command = format!(
            "/bin/sh -c 'printf %s \"$$\" > \"$1\"; exec yes agent-background-output' \
             agent-writer '{quoted_pid_file}' & sleep 0.1"
        );

        let execution = run_captured_to(&command, &mut state, &mut cwd, &mut terminal_output);

        assert_eq!(execution.exit_code(), Some(0));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "continuous background output delayed Agent completion for {:?}",
            started.elapsed()
        );
        assert!(execution.evidence().contains("further output not captured"));

        // The capture reader is the only thing keeping the writer alive. Once
        // run_captured_to closes it, the writer must receive SIGPIPE and the
        // background shell waiting for it must disappear as well. The old
        // plain `pipe()` leaked the read end through exec, so this exact test
        // returned quickly while leaving both processes behind forever.
        let background_pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("background pid report")
            .parse()
            .expect("numeric background pid");
        assert!(background_pid > 1, "refuse to probe an unsafe pid");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut alive = true;
        while Instant::now() < deadline {
            alive = unsafe { nix::libc::kill(background_pid, 0) } == 0;
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if alive {
            // Keep a failed regression test from recreating the leak it is
            // meant to detect. The PID came from this test's own child.
            unsafe {
                nix::libc::kill(background_pid, nix::libc::SIGKILL);
            }
        }
        assert!(!alive, "background Agent writer survived capture teardown");
    }

    #[test]
    fn capture_pipe_marks_both_original_descriptors_close_on_exec() {
        use nix::fcntl::{fcntl, FcntlArg, FdFlag};

        let (reader, writer) = capture_pipe_cloexec().expect("capture pipe");
        for descriptor in [&reader, &writer] {
            let flags = fcntl(descriptor, FcntlArg::F_GETFD).expect("descriptor flags");
            assert!(
                FdFlag::from_bits_truncate(flags).contains(FdFlag::FD_CLOEXEC),
                "capture descriptor must not survive exec"
            );
        }
    }

    #[test]
    fn readiness_channel_accepts_exactly_one_private_marker() {
        use std::io::Write as _;
        use std::os::fd::AsFd;

        let inspect = |bytes: &[u8]| {
            let (reader, writer) = capture_pipe_cloexec().expect("readiness pipe");
            let mut writer = std::fs::File::from(writer);
            writer.write_all(bytes).expect("write readiness fixture");
            drop(writer);
            let mut readiness = AgentChildReadiness::default();
            drain_agent_child_readiness(reader.as_fd(), &mut readiness);
            readiness
        };

        let valid = inspect(&[AGENT_CHILD_READY]);
        assert!(valid.ready);
        assert!(valid.closed);
        assert!(!valid.invalid());
        assert!(valid.authenticated());

        let incomplete = AgentChildReadiness {
            ready: true,
            closed: false,
            received_bytes: 1,
            issue: None,
        };
        assert!(!incomplete.authenticated());

        for invalid in [vec![b'+'], vec![AGENT_CHILD_READY, AGENT_CHILD_READY]] {
            let readiness = inspect(&invalid);
            assert!(readiness.invalid(), "accepted control bytes {invalid:?}");
        }
    }

    #[test]
    fn readiness_drain_stops_at_the_first_typed_protocol_issue() {
        use std::io::Write as _;
        use std::os::fd::AsFd;

        let (reader, writer) = capture_pipe_cloexec().expect("readiness pipe");
        let mut writer = std::fs::File::from(writer);
        writer
            .write_all(&[AGENT_CHILD_READY; 64])
            .expect("write duplicate markers");
        drop(writer);
        let mut readiness = AgentChildReadiness::default();
        drain_agent_child_readiness(reader.as_fd(), &mut readiness);
        assert_eq!(
            readiness.issue,
            Some(super::AgentChildReadinessIssue::DuplicateMarker)
        );
        assert!(readiness.received_bytes <= 16);
        assert!(!readiness.authenticated());
    }

    #[test]
    fn cwd_frame_rejects_bad_prefix_length_nonce_and_extra_bytes_early() {
        let nonce = [0x5a; 32];

        let mut bad_magic = super::AgentChildCwdFrameBuffer::default();
        bad_magic.observe(b"X", &nonce);
        assert_eq!(
            bad_magic.issue,
            Some(super::AgentChildCwdFrameIssue::InvalidMagic)
        );

        let mut bad_length = super::AgentChildCwdFrameBuffer::default();
        bad_length.observe(b"JCW1\0\0\0\0", &nonce);
        assert_eq!(
            bad_length.issue,
            Some(super::AgentChildCwdFrameIssue::InvalidLength)
        );

        let frame = encode_agent_child_cwd_frame(Path::new("/tmp/agent-cwd"), &nonce).unwrap();
        let mut wrong_nonce = super::AgentChildCwdFrameBuffer::default();
        wrong_nonce.observe(&frame, &[0xa5; 32]);
        assert_eq!(
            wrong_nonce.issue,
            Some(super::AgentChildCwdFrameIssue::NonceMismatch)
        );

        let mut extra = super::AgentChildCwdFrameBuffer::default();
        extra.observe(&frame, &nonce);
        extra.observe(b"x", &nonce);
        assert_eq!(
            extra.issue,
            Some(super::AgentChildCwdFrameIssue::ExtraBytes)
        );

        let mut truncated = super::AgentChildCwdFrameBuffer::default();
        truncated.observe(&frame[..8], &nonce);
        truncated.finish_eof();
        assert_eq!(
            truncated.issue,
            Some(super::AgentChildCwdFrameIssue::TruncatedFrame)
        );
    }

    #[test]
    fn maximum_cwd_frame_crosses_one_drain_budget_and_still_authenticates() {
        use nix::fcntl::{fcntl, FcntlArg, OFlag};
        use std::io::Write as _;

        let nonce = [0x3c; 32];
        let cwd = PathBuf::from(format!("/{}", "x".repeat(MAX_AGENT_CHILD_CWD_BYTES - 1)));
        let frame = encode_agent_child_cwd_frame(&cwd, &nonce).unwrap();
        assert_eq!(frame.len(), MAX_AGENT_CHILD_CWD_FRAME_BYTES);
        let (reader, writer) = capture_pipe_cloexec().expect("cwd frame pipe");
        let flags = fcntl(&reader, FcntlArg::F_GETFL).expect("reader flags");
        fcntl(
            &reader,
            FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
        )
        .expect("nonblocking reader");
        let writer_thread = std::thread::spawn(move || {
            let mut writer = std::fs::File::from(writer);
            writer.write_all(&frame)
        });

        let mut reader = Some(reader);
        let mut report = super::AgentChildCwdFrameBuffer::default();
        let deadline = Instant::now() + Duration::from_secs(2);
        while reader.is_some() && Instant::now() < deadline {
            super::drain_agent_child_cwd_reader(&mut reader, &mut report, &nonce);
            std::thread::yield_now();
        }
        assert!(reader.is_none(), "maximum frame did not reach EOF");
        writer_thread.join().unwrap().unwrap();
        assert_eq!(report.received_bytes, MAX_AGENT_CHILD_CWD_FRAME_BYTES);
        assert_eq!(report.decode(&nonce).unwrap(), cwd);

        // Debug/status accounting must never reveal the path bytes.
        let debug = format!("{report:?}");
        assert!(!debug.contains("xxxxxxxx"));
        assert!(debug.contains("received_bytes"));
    }

    #[test]
    fn invalid_cwd_frame_closes_the_reader_instead_of_draining_a_flood() {
        use nix::fcntl::{fcntl, FcntlArg, OFlag};
        use std::io::Write as _;

        let nonce = [0x42; 32];
        let (reader, writer) = capture_pipe_cloexec().expect("cwd frame pipe");
        let flags = fcntl(&reader, FcntlArg::F_GETFL).expect("reader flags");
        fcntl(
            &reader,
            FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
        )
        .expect("nonblocking reader");
        let mut writer = std::fs::File::from(writer);
        writer.write_all(b"not-a-frame").unwrap();
        let mut reader = Some(reader);
        let mut report = super::AgentChildCwdFrameBuffer::default();
        super::drain_agent_child_cwd_reader(&mut reader, &mut report, &nonce);
        assert!(reader.is_none());
        assert_eq!(
            report.issue,
            Some(super::AgentChildCwdFrameIssue::InvalidMagic)
        );
        assert!(writer.write_all(b"continued flood").is_err());
    }

    #[test]
    fn all_three_agent_channels_require_distinct_pipe_identities() {
        use std::os::fd::AsRawFd;

        let (_reader_a, writer_a) = capture_pipe_cloexec().unwrap();
        let (_reader_b, writer_b) = capture_pipe_cloexec().unwrap();
        let (_reader_c, writer_c) = capture_pipe_cloexec().unwrap();
        let writer_a = std::fs::File::from(writer_a);
        let duplicate_a = writer_a.try_clone().unwrap();
        let identity_a = super::agent_child_pipe_identity(writer_a.as_raw_fd()).unwrap();
        let duplicate_identity = super::agent_child_pipe_identity(duplicate_a.as_raw_fd()).unwrap();
        let identity_b = super::agent_child_pipe_identity(writer_b.as_raw_fd()).unwrap();
        let identity_c = super::agent_child_pipe_identity(writer_c.as_raw_fd()).unwrap();

        assert!(super::agent_child_pipe_identities_are_distinct(&[
            identity_a, identity_b, identity_c,
        ]));
        assert!(!super::agent_child_pipe_identities_are_distinct(&[
            identity_a,
            duplicate_identity,
            identity_c,
        ]));
    }

    #[test]
    fn cwd_report_preserves_significant_trailing_whitespace() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("directory with trailing space ");
        std::fs::create_dir(&target).expect("target directory");
        let mut state = ShellState::new(false);
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut terminal_output = std::io::sink();
        let command = format!("cd '{}'", target.display());

        let execution = run_captured_to(&command, &mut state, &mut cwd, &mut terminal_output);

        assert_eq!(execution.exit_code(), Some(0));
        assert_eq!(cwd, target);
    }

    #[test]
    fn enumerable_files_cannot_forge_the_private_cwd_report_pipe() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("cwd fixtures");
        let forged = root.path().join("forged");
        let reported = root.path().join("reported");
        std::fs::create_dir(&forged).unwrap();
        std::fs::create_dir(&reported).unwrap();

        let state = ShellState::new(false);
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let transport = AgentChildTransport::new(&state, &cwd).expect("Agent transport");
        // This was the old discoverable protocol name. Its contents and
        // permissions are now irrelevant because the parent owns an unnamed
        // pipe reader created after the private snapshot.
        let obsolete_report = transport.dir.join("cwd-report");
        std::fs::write(&obsolete_report, forged.as_os_str().as_encoded_bytes()).unwrap();
        std::fs::set_permissions(&obsolete_report, std::fs::Permissions::from_mode(0o666)).unwrap();
        let mut terminal_output = std::io::sink();
        let command = format!("cd '{}'", reported.display());

        let execution =
            run_captured_with_transport(&command, &transport, &mut cwd, &mut terminal_output);

        assert_eq!(execution.exit_code(), Some(0));
        assert_eq!(cwd, reported);
        std::fs::remove_file(obsolete_report).unwrap();
    }

    #[test]
    fn same_process_fd_tampering_cannot_authenticate_a_forged_cwd_frame() {
        let root = tempfile::tempdir().expect("cwd fixtures");
        let forged = root.path().join("forged");
        let actual = root.path().join("actual");
        std::fs::create_dir(&forged).unwrap();
        std::fs::create_dir(&actual).unwrap();

        let wrong_nonce = [0xa5; 32];
        let forged_frame = encode_agent_child_cwd_frame(&forged, &wrong_nonce).unwrap();
        let escaped_frame = forged_frame
            .iter()
            .map(|byte| format!("\\{:03o}", byte))
            .collect::<Vec<_>>()
            .join("");
        // An external child can reopen same-user descriptors through
        // /proc/$PPID/fd even though CLOEXEC kept them out of its own table.
        // It writes a structurally valid frame with a guessed nonce to every
        // writable parent descriptor. Quoted `exec` arguments then exercise
        // jsh's persistent same-process close path over every plausible fd,
        // preventing the genuine final frame from being appended.
        let attack = format!(
            "/bin/sh -c 'payload=\"{escaped_frame}\"; for target in /proc/$PPID/fd/*; do fd=${{target##*/}}; test \"$fd\" -ge 3 2>/dev/null || continue; /usr/bin/printf \"%b\" \"$payload\" 2>/dev/null > \"$target\" || :; done'"
        );
        let closes = (3..=1024)
            .map(|descriptor| format!("exec '{descriptor}>&-'"))
            .collect::<Vec<_>>()
            .join("; ");
        let command = format!("{attack}; {closes}; cd '{}'", actual.display());

        let mut state = ShellState::new(false);
        let original = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut cwd = original.clone();
        let mut terminal_output = std::io::sink();
        let execution = run_captured_to(&command, &mut state, &mut cwd, &mut terminal_output);

        assert_eq!(execution.exit_code(), Some(0), "{}", execution.evidence());
        // The attack also closes the internal child's stderr, so the expected
        // fail-closed report diagnostic is intentionally not observable here.
        // A clean exit proves that closing the raw report descriptor no longer
        // trips Rust's owned-fd abort; refusing both candidate cwd values proves
        // that neither the guessed frame nor a post-tamper frame was accepted.
        assert_eq!(cwd, original, "forged cwd frame was accepted");
        assert_ne!(cwd, forged);
        assert_ne!(cwd, actual);
    }

    #[test]
    fn approved_external_process_inherits_neither_report_env_nor_writer() {
        let mut state = ShellState::new(false);
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut terminal_output = std::io::sink();
        let command = "/bin/sh -c 'test -z \"${JSH_AGENT_CHILD_CWD_REPORT_FD+x}\" || exit 90; fd=3; while test $fd -le 255; do test ! -e /proc/self/fd/$fd || { printf inherited-fd-%s $fd; exit 91; }; fd=$((fd + 1)); done'";

        let execution = run_captured_to(command, &mut state, &mut cwd, &mut terminal_output);

        assert_eq!(execution.exit_code(), Some(0), "{}", execution.evidence());
        assert!(!execution.evidence().contains("inherited-fd"));
    }

    #[test]
    fn approved_child_inherits_shell_state_without_mutating_parent() {
        let mut state = ShellState::new(false);
        state
            .aliases
            .insert("agent_alias".into(), "/usr/bin/printf alias-ok".into());
        state.set_var("AGENT_SNAPSHOT_VALUE", "parent-value");
        state.shell_opts.nounset = true;
        let definitions = crate::parser::parse("agent_fn() { /usr/bin/printf ':function-ok'; }")
            .expect("function parse");
        assert_eq!(
            crate::executor::execute_program(&definitions, &mut state),
            0
        );

        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut terminal_output = std::io::sink();
        let command = "agent_alias; agent_fn; /usr/bin/printf ':%s:%s' \"$AGENT_SNAPSHOT_VALUE\" \"$-\"; AGENT_SNAPSHOT_VALUE=child-value; alias agent_alias='/usr/bin/printf child-alias'";

        let execution = run_captured_to(command, &mut state, &mut cwd, &mut terminal_output);

        assert_eq!(execution.exit_code(), Some(0));
        let observation = execution.evidence();
        assert!(
            observation.contains("alias-ok:function-ok:parent-value:"),
            "restored Agent state produced unexpected output: {observation:?}"
        );
        assert!(
            observation
                .split("alias-ok:function-ok:parent-value:")
                .nth(1)
                .is_some_and(|flags| flags.contains('u')),
            "restored option flags missing nounset: {observation:?}"
        );
        assert_eq!(state.get_var("AGENT_SNAPSHOT_VALUE"), Some("parent-value"));
        assert_eq!(
            state.aliases.get("agent_alias").map(String::as_str),
            Some("/usr/bin/printf alias-ok")
        );
        assert!(state.functions.contains_key("agent_fn"));
    }

    #[test]
    fn git_dirty_probe_reports_clean_and_untracked_worktrees() {
        let temp = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(temp.path())
                .status()
                .expect("run git fixture command");
            assert!(status.success(), "git fixture command failed: {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(temp.path().join("tracked"), "initial\n").expect("tracked fixture");
        git(&["add", "tracked"]);
        git(&[
            "-c",
            "user.name=jsh test",
            "-c",
            "user.email=jsh@example.invalid",
            "commit",
            "-qm",
            "initial",
        ]);

        let branch = bounded_git_stdout(
            temp.path(),
            &["rev-parse", "--abbrev-ref", "HEAD"],
            16 * 1024,
        )
        .expect("bounded branch probe");
        assert!(!branch.is_empty());
        assert!(
            bounded_git_stdout(temp.path(), &["rev-parse", "--abbrev-ref", "HEAD"], 1).is_none(),
            "oversized Git probe output must fail closed"
        );
        assert_eq!(git_worktree_dirty(temp.path()), Some(false));
        assert_eq!(git_meta(temp.path()).map(|meta| meta.dirty), Some(false));
        std::fs::write(temp.path().join("untracked"), "new\n").expect("untracked fixture");
        assert_eq!(git_worktree_dirty(temp.path()), Some(true));
        assert_eq!(git_meta(temp.path()).map(|meta| meta.dirty), Some(true));
    }

    #[test]
    fn untrusted_agent_text_cannot_emit_terminal_controls_or_grow_unbounded() {
        let hostile = format!(
            "before\x1b]52;c;Y2xpcGJvYXJk\x07after\rnext\u{0085}\u{202e}\u{200b}{}",
            "界".repeat(MAX_AGENT_DISPLAY_BYTES)
        );

        let rendered = terminal_safe_text(&hostile, MAX_AGENT_DISPLAY_BYTES);

        assert!(rendered.len() <= MAX_AGENT_DISPLAY_BYTES);
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
        assert!(!rendered.contains('\r'));
        assert!(!rendered.contains('\u{0085}'));
        assert!(rendered.contains("\\x1b]52"));
        assert!(rendered.contains("\\r"));
        assert!(rendered.contains("\\u{85}"));
        assert!(rendered.contains("\\u{202e}"));
        assert!(rendered.contains("\\u{200b}"));
        assert!(rendered.is_char_boundary(rendered.len()));

        let inline = terminal_safe_inline_text("first\nsecond", MAX_AGENT_DISPLAY_BYTES);
        assert_eq!(inline, "first\\nsecond");
        let message = terminal_safe_message("agent: ", "first\nsecond", MAX_AGENT_DISPLAY_BYTES);
        assert_eq!(message, "agent: first\nagent: second");

        // The maximum-size command remains fully reviewable even when every
        // character expands into an explicit Unicode escape.
        let ambiguous = "\u{00ad}".repeat((16 * 1024) / '\u{00ad}'.len_utf8());
        let command_display =
            terminal_safe_inline_text(&ambiguous, MAX_AGENT_COMMAND_DISPLAY_BYTES);
        assert!(!command_display.ends_with('…'));
        assert_eq!(
            command_display.matches("\\u{ad}").count(),
            ambiguous.chars().count()
        );
    }

    #[test]
    fn jagent_boundaries_and_protocol_selection_are_shared() {
        assert_eq!(configured_max_turns(None), 16);
        assert_eq!(configured_max_turns(Some("0")), 16);
        assert_eq!(
            configured_max_turns(Some("4294967295")),
            MAX_AGENT_SESSION_TURNS
        );
        assert_eq!(
            configured_agent_protocol(Provider::Ollama, None, None),
            Ok(jagent::AgentProtocol::Text)
        );
        assert_eq!(
            configured_agent_protocol(
                Provider::Ollama,
                Some("native-tools"),
                Some(jagent::AGENT_CAPABILITIES_V1_WIRE),
            ),
            Ok(jagent::AgentProtocol::NativeTools)
        );
        assert_eq!(
            configured_agent_protocol(Provider::Ollama, Some("guess"), None),
            Err(AgentProtocolConfigError::InvalidProtocol)
        );
        for command in [
            "hostname build-node",
            "date --set=tomorrow",
            "truncate -s 0 database",
            "git restore src/main.rs",
            "git checkout -- src/main.rs",
            "git branch -D work",
            "git stash clear",
            "docker volume rm database",
            "kubectl delete namespace prod",
            "terraform destroy -auto-approve",
        ] {
            assert!(jagent::is_dangerous(command).is_some(), "missed {command}");
        }
    }

    #[test]
    fn complete_peer_negotiation_is_strict_and_default_text_is_stable_for_every_provider() {
        const TEXT_COMPLETE: &str = "jagent-agent/1;protocols=text;delivery=complete";
        const NATIVE_COMPLETE: &str = "jagent-agent/1;protocols=native-tools;delivery=complete";
        const STREAMING_ONLY: &str =
            "jagent-agent/1;protocols=text,native-tools;delivery=streaming";

        for provider in [
            Provider::Anthropic,
            Provider::OpenAiCompatible,
            Provider::Ollama,
        ] {
            assert_eq!(
                configured_agent_protocol(provider, None, None),
                Ok(AgentProtocol::Text),
                "legacy default drifted for {provider:?}"
            );
            assert_eq!(
                configured_agent_protocol(provider, None, Some(jagent::AGENT_CAPABILITIES_V1_WIRE),),
                Ok(AgentProtocol::Text),
                "capability discovery must not silently opt {provider:?} into native tools"
            );
            assert_eq!(
                configured_agent_protocol(provider, Some("text"), Some(TEXT_COMPLETE)),
                Ok(AgentProtocol::Text)
            );
            assert_eq!(
                configured_agent_protocol(provider, Some("native-tools"), Some(NATIVE_COMPLETE),),
                Ok(AgentProtocol::NativeTools)
            );
            assert_eq!(
                configured_agent_protocol(provider, Some("native-tools"), Some(TEXT_COMPLETE)),
                Err(AgentProtocolConfigError::UnsupportedSelection(
                    AgentProtocol::NativeTools
                ))
            );
            assert_eq!(
                configured_agent_protocol(provider, None, Some(NATIVE_COMPLETE)),
                Err(AgentProtocolConfigError::UnsupportedSelection(
                    AgentProtocol::Text
                ))
            );
            assert_eq!(
                configured_agent_protocol(provider, Some("text"), Some(STREAMING_ONLY)),
                Err(AgentProtocolConfigError::UnsupportedSelection(
                    AgentProtocol::Text
                )),
                "jsh's complete-only transport must reject streaming-only {provider:?} peers"
            );
        }
    }

    #[test]
    fn malformed_peer_capabilities_are_bounded_and_never_echoed() {
        let secret = "not-a-token-jsh-peer-secret";
        let error =
            configured_agent_protocol(Provider::OpenAiCompatible, Some("text"), Some(secret))
                .unwrap_err();
        assert_eq!(
            error,
            AgentProtocolConfigError::InvalidPeer(jagent::CapabilityError::Malformed)
        );
        assert!(!error.to_string().contains(secret));

        let oversized = "x".repeat(jagent::MAX_AGENT_CAPABILITIES_WIRE_BYTES + 1);
        let error = configured_agent_protocol(Provider::Anthropic, Some("text"), Some(&oversized))
            .unwrap_err();
        assert_eq!(
            error,
            AgentProtocolConfigError::InvalidPeer(jagent::CapabilityError::TooLarge)
        );
        assert!(!error.to_string().contains(&oversized));
    }

    #[test]
    fn native_tools_provider_envelopes_follow_the_prepared_request_into_session() {
        let fixtures: [(Provider, &[u8]); 3] = [
            (
                Provider::Anthropic,
                br#"{"content":[{"type":"tool_use","id":"toolu_1","name":"run","input":{"command":"pwd"}}],"stop_reason":"tool_use"}"#,
            ),
            (
                Provider::OpenAiCompatible,
                br#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"run","arguments":"{\"command\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            ),
            (
                Provider::Ollama,
                br#"{"message":{"content":"","tool_calls":[{"function":{"name":"run","arguments":{"command":"pwd"}}}]},"done":true,"done_reason":"stop"}"#,
            ),
        ];

        for (provider, body) in fixtures {
            let history = [Message {
                role: Role::User,
                text: "inspect".into(),
            }];
            let config = ChatConfig {
                provider,
                api_key: (provider == Provider::Anthropic).then(|| "test-key".into()),
                model: "test-model".into(),
                base_url: provider.default_base_url().into(),
                max_tokens: 128,
                temperature: Some(0.0),
            };
            let prepared = prepare_request(
                &config,
                RequestSpec::new(&history, AgentProtocol::NativeTools),
            )
            .unwrap();
            assert_eq!(prepared.protocol(), AgentProtocol::NativeTools);
            assert!(prepared.request.body.contains("\"tools\""));

            let response = prepared.parse_response(body).unwrap();
            let mut session = AgentSession::new(4);
            session.submit_user("inspect").unwrap();
            let ModelOutcome::Proposal { command, .. } =
                session.accept_agent_response(&response).unwrap()
            else {
                panic!("{provider:?} did not produce a proposal")
            };
            assert_eq!(command, "pwd", "{provider:?}");
        }
    }

    #[test]
    fn duplicate_native_tool_arguments_fail_before_becoming_a_proposal() {
        let history = [Message {
            role: Role::User,
            text: "inspect".into(),
        }];
        let config = ChatConfig {
            provider: Provider::OpenAiCompatible,
            api_key: None,
            model: "test-model".into(),
            base_url: Provider::OpenAiCompatible.default_base_url().into(),
            max_tokens: 128,
            temperature: Some(0.0),
        };
        let prepared = prepare_request(
            &config,
            RequestSpec::new(&history, AgentProtocol::NativeTools),
        )
        .unwrap();
        let duplicate = br#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"run","arguments":"{\"command\":\"first\",\"command\":\"second\"}"}}]},"finish_reason":"tool_calls"}]}"#;

        let response = prepared.parse_response(duplicate).unwrap();
        let mut session = AgentSession::new(4);
        session.submit_user("inspect").unwrap();
        assert!(session.accept_agent_response(&response).is_err());
    }
}
