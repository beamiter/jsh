//! Bounded Unix file/process I/O used at persistence and helper-process
//! boundaries. These helpers are intentionally small and policy-free: callers
//! choose limits, while this module guarantees no symlink/FIFO reads, no
//! partial in-place persistence writes, and no unbounded `Command::output`.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SAFE_READ_FLAGS: i32 = nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Resolve helpers that jsh may start automatically without consulting the
/// mutable shell `PATH`. Candidates are fixed system locations and neither the
/// executable nor its containing namespace may be replaceable by anyone but
/// the party this shell already trusts — see [`replaceable_by_others`].
pub(crate) fn automatic_system_helper(name: &str) -> Option<&'static Path> {
    let candidates: &[&'static str] = match name {
        "bash" => &["/usr/bin/bash", "/bin/bash"],
        "git" => &["/usr/bin/git", "/bin/git", "/usr/local/bin/git"],
        "docker" => &["/usr/bin/docker", "/bin/docker", "/usr/local/bin/docker"],
        "systemctl" => &["/usr/bin/systemctl", "/bin/systemctl"],
        "notify-send" => &["/usr/bin/notify-send", "/bin/notify-send"],
        _ => return None,
    };
    candidates.iter().find_map(|candidate| {
        let path = Path::new(candidate);
        let metadata = path.metadata().ok()?;
        if !metadata.is_file() || replaceable_by_others(&metadata) || metadata.mode() & 0o111 == 0 {
            return None;
        }
        let parent = path.parent()?.metadata().ok()?;
        (parent.is_dir() && !replaceable_by_others(&parent)).then_some(path)
    })
}

fn replaceable_by_others(metadata: &fs::Metadata) -> bool {
    replaceable(metadata.mode(), metadata.uid(), unsafe {
        nix::libc::geteuid()
    })
}

/// Could someone other than the party this shell already trusts put a different
/// binary at this path before it is executed?
///
/// Group- and world-writable is always yes: anyone in the group, or anyone at
/// all, can write through it.
///
/// For an unprivileged shell, so is a path this user owns and can write. That
/// is not about the user attacking themselves — it is that such a path is
/// reachable by anything already running as them, which a system location is
/// supposed not to be.
///
/// Root is the case that rule cannot express. Root can write every file on the
/// system, so asking "can the current user write it" answers yes for
/// `/usr/bin/git` and `/usr/bin/bash` on every root shell — and a container is
/// a root shell by default, which is where this was found: Git completion, the
/// Git prompt and the `.bashrc` import all disappeared inside `docker exec`
/// while working locally. For euid 0 the meaningful question is instead whether
/// some *other* user owns the path, which is the same rule
/// [`trusted_path_component`] applies to an explicitly configured helper.
///
/// Split from [`replaceable_by_others`] so the euid-0 branch can be asserted by
/// a test suite that does not run as root.
fn replaceable(mode: u32, owner: u32, euid: u32) -> bool {
    if mode & 0o022 != 0 {
        return true;
    }
    if euid == 0 {
        return owner != 0;
    }
    owner == euid && mode & 0o200 != 0
}

/// Validate an explicitly configured executable without falling back to PATH.
pub(crate) fn explicit_absolute_executable(path: &Path) -> bool {
    trusted_explicit_executable(path)
}

/// Why [`executable_named_by_startup`] would not run a path.
pub(crate) enum StartupExecutable {
    Ok,
    /// Not absolute, missing, not a file, or not executable.
    Unusable,
    /// Some third account owns it or a directory above it.
    ForeignOwner,
}

/// Validate an executable that the user's own startup file named, such as
/// `$CONDA_EXE`.
///
/// This is deliberately weaker than [`trusted_explicit_executable`], and the
/// reason is that the stronger rule was protecting nothing here. jsh reaches
/// `$CONDA_EXE` only by first *sourcing the startup file that set it* — a file
/// which, in the container images where this keeps coming up, is itself mode
/// 0777 in a mode 0777 home, and which has already run that very binary
/// (`conda activate base` is a line in it). A shell that executes the config
/// and then refuses the one path the config named has not closed the hole; it
/// has only lost the shell hook, silently, which is how `conda activate` came
/// to answer "run 'conda init' first" in a shell whose `conda init` block has
/// been in place for years.
///
/// So the writability bits are not consulted. Ownership still is: a path owned
/// by some *third* account is a different machine-level claim from a loose mode
/// on the user's own tree, and nothing in the startup file justifies running
/// it. That case is reported rather than ignored — see the caller.
pub(crate) fn executable_named_by_startup(path: &Path) -> StartupExecutable {
    if !path.is_absolute() {
        return StartupExecutable::Unusable;
    }
    let Ok(resolved) = path.canonicalize() else {
        return StartupExecutable::Unusable;
    };
    let Ok(metadata) = resolved.metadata() else {
        return StartupExecutable::Unusable;
    };
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return StartupExecutable::Unusable;
    }
    let euid = unsafe { nix::libc::geteuid() };
    let mut component = Some(resolved.as_path());
    while let Some(current) = component {
        let Ok(metadata) = current.metadata() else {
            return StartupExecutable::Unusable;
        };
        if metadata.uid() != 0 && metadata.uid() != euid {
            return StartupExecutable::ForeignOwner;
        }
        component = current.parent();
    }
    StartupExecutable::Ok
}

/// Environment variable that names an explicit absolute path for a helper.
/// `notify-send` becomes `JSH_HELPER_NOTIFY_SEND`.
fn helper_path_variable(name: &str) -> String {
    let mut variable = String::from("JSH_HELPER_");
    for byte in name.bytes() {
        variable.push(match byte {
            b'-' => '_',
            other => other.to_ascii_uppercase() as char,
        });
    }
    variable
}

/// The helper jsh should start for `name`, or `None` when there is no
/// trustworthy one.
///
/// The fixed candidate list in [`automatic_system_helper`] is deliberately
/// short and absolute, which costs nothing on a distribution that puts `git` in
/// `/usr/bin` and everything on one that does not: Nix, Homebrew-style prefixes
/// and immutable-root images all lose the Git prompt, `.bashrc` import, and
/// desktop notifications with no way to say where those tools actually are.
///
/// `JSH_HELPER_<NAME>` says where. It is not a return to PATH lookup: PATH is
/// mutable shell state that any script can rewrite, while this is one explicit
/// absolute path that must still survive [`trusted_explicit_executable`].
///
/// A configured path that fails those checks yields no helper at all rather
/// than quietly falling back to the automatic candidate. Silently starting a
/// *different* binary than the one that was named is worse than the feature
/// being missing, and the missing feature is visible.
pub(crate) fn trusted_helper(name: &str) -> Option<std::path::PathBuf> {
    if let Some(configured) = std::env::var_os(helper_path_variable(name)) {
        if !configured.is_empty() {
            let path = std::path::PathBuf::from(configured);
            if trusted_explicit_executable(&path) {
                return Some(path);
            }
            warn_once_about_helper(name, &path);
            return None;
        }
    }
    automatic_system_helper(name).map(Path::to_path_buf)
}

/// Resolve a helper without emitting the normal once-per-process warning.
///
/// Diagnostics use this path because a valid `--json` request must produce one
/// self-contained document. The caller reports an invalid override as a
/// structured check instead of letting resolution write a side-channel line.
pub(crate) fn trusted_helper_quiet(name: &str) -> Option<std::path::PathBuf> {
    if let Some(configured) = std::env::var_os(helper_path_variable(name)) {
        if !configured.is_empty() {
            let path = std::path::PathBuf::from(configured);
            return trusted_explicit_executable(&path).then_some(path);
        }
    }
    automatic_system_helper(name).map(Path::to_path_buf)
}

/// One line per helper per process. These resolve from a prompt callback and a
/// notification thread, so an unconditional warning would repeat on every
/// prompt draw.
fn warn_once_about_helper(name: &str, path: &Path) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static WARNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let Ok(mut warned) = WARNED.lock() else {
        return;
    };
    if warned
        .get_or_insert_with(HashSet::new)
        .insert(name.to_string())
    {
        eprintln!(
            "jsh: {} names {}, which is not a trusted executable; \
             {name} integration is disabled",
            helper_path_variable(name),
            path.display()
        );
    }
}

/// Is this an absolute path to an executable that no third party can replace?
///
/// Symlinks are followed rather than refused: `/run/current-system/sw/bin/git`
/// on Nix and `/etc/alternatives`-style indirection everywhere else are normal,
/// and what matters is the trustworthiness of what they resolve to.
///
/// The check walks the whole resolved directory chain, not just the file. A
/// binary in a safe directory under a world-writable parent is not safe: the
/// parent can be renamed out of the way and replaced wholesale, so validating
/// only the leaf proves nothing about what will be there at exec time.
fn trusted_explicit_executable(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    // canonicalize resolves every symlink, so the chain walked below is the
    // chain the kernel will walk, rather than the one the caller typed.
    let Ok(resolved) = path.canonicalize() else {
        return false;
    };
    let Ok(metadata) = resolved.metadata() else {
        return false;
    };
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 || !trusted_path_component(&metadata) {
        return false;
    }
    let mut directory = resolved.parent();
    while let Some(current) = directory {
        let Ok(metadata) = current.metadata() else {
            return false;
        };
        if !metadata.is_dir() || !trusted_path_component(&metadata) {
            return false;
        }
        directory = current.parent();
    }
    true
}

/// Trusted means "only root or this user can change it". World-writable is
/// refused outright — that covers the sticky-bit temporary directories, where
/// anyone can plant a name — and so is ownership by some third user, who could
/// replace the file between this check and the exec.
///
/// Group-writable is refused only when the group actually contains somebody
/// else; see [`group_write_reaches_others`].
fn trusted_path_component(metadata: &fs::Metadata) -> bool {
    if metadata.mode() & 0o002 != 0 {
        return false;
    }
    if metadata.mode() & 0o020 != 0 && group_write_reaches_others(metadata.gid(), metadata.uid()) {
        return false;
    }
    metadata.uid() == 0 || metadata.uid() == unsafe { nix::libc::geteuid() }
}

/// Does the group bit on a path hand write access to anyone beyond its owner?
///
/// Debian, Ubuntu and Fedora all give each user a group of their own and ship
/// umask 002, so *everything* a user installs under their home is mode 0775
/// with a group only that user is in. Reading the bit alone as "someone else
/// can write here" therefore refuses a whole `~/miniconda3`, `~/.local` or
/// `~/.cargo` — every helper a person actually installs — while proving
/// nothing: the group is them. That is how `conda activate` lost its shell
/// hook, silently, on a stock Ubuntu.
///
/// So look at who is in the group instead of at the bit. Private means the
/// owner's primary group with no other members, which grants exactly the
/// access the owner bit already grants. Anything else — a shared `staff`,
/// `wheel`, or a group the owner merely belongs to — stays untrusted.
///
/// Users whose *primary* group is this one do not appear in the member list,
/// and finding them would mean enumerating passwd, which is unbounded and
/// often impossible under LDAP or SSSD. A second account sharing a first
/// account's private group is a hand-made configuration; a shell's helper
/// lookup is not where it gets caught.
fn group_write_reaches_others(gid: u32, owner: u32) -> bool {
    let Ok(Some(group)) = nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(gid)) else {
        return true;
    };
    let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(owner)) else {
        return true;
    };
    group_reaches_others(gid, user.gid.as_raw(), &user.name, &group.mem)
}

/// Split from [`group_write_reaches_others`] so the membership rule can be
/// asserted without a test suite that owns /etc/group.
fn group_reaches_others(gid: u32, owner_primary_gid: u32, owner: &str, members: &[String]) -> bool {
    gid != owner_primary_gid || members.iter().any(|member| member != owner)
}

/// Read the first bytes of a regular file, with the same refusals as
/// [`read_regular_file`] but without its size limit.
///
/// The limit in the others means "this file is not allowed to be bigger than
/// that". Reading a header is the opposite question: an ELF is megabytes long
/// and only its first hundred bytes are being asked about, so a cap that
/// rejects the file would answer "no" for every real binary.
pub(crate) fn read_regular_file_prefix(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(SAFE_READ_FLAGS)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "refusing to read a non-regular file",
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(max_bytes as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Read a regular file with a byte cap. Symlinks and special files are refused
/// so a startup/completion probe cannot block on a FIFO or silently traverse a
/// link to unrelated data.
pub(crate) fn read_regular_file(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    read_regular_file_with_policy(path, max_bytes, false, true)
}

/// Read a regular file through an explicitly supplied symlink. This is for
/// command/script operands where symlinks are normal shell semantics; special
/// files remain non-blocking and are rejected after open.
pub(crate) fn read_regular_file_following(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    read_regular_file_with_policy(path, max_bytes, false, false)
}

/// As [`read_regular_file`], additionally requiring a single-link file owned
/// by the effective user. Use this for private shell persistence.
pub(crate) fn read_private_file(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    read_regular_file_with_policy(path, max_bytes, true, true)
}

pub(crate) fn read_regular_text(path: &Path, max_bytes: usize) -> io::Result<String> {
    String::from_utf8(read_regular_file(path, max_bytes)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn read_regular_text_following(path: &Path, max_bytes: usize) -> io::Result<String> {
    String::from_utf8(read_regular_file_following(path, max_bytes)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn read_private_text(path: &Path, max_bytes: usize) -> io::Result<String> {
    String::from_utf8(read_private_file(path, max_bytes)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn read_to_end_bounded(mut reader: impl Read, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("input exceeds the {max_bytes} byte limit"),
        ));
    }
    Ok(bytes)
}

fn read_regular_file_with_policy(
    path: &Path,
    max_bytes: usize,
    private: bool,
    nofollow: bool,
) -> io::Result<Vec<u8>> {
    let flags = if nofollow {
        SAFE_READ_FLAGS
    } else {
        nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC
    };
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "refusing to read a non-regular file",
        ));
    }
    if private && metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private data file has multiple hard links",
        ));
    }
    if private && metadata.uid() != unsafe { nix::libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private data file is not owned by the current user",
        ));
    }
    if private && metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private data file is writable by another user",
        ));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file exceeds the {max_bytes} byte limit"),
        ));
    }

    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(max_bytes));
    Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file exceeds the {max_bytes} byte limit"),
        ));
    }
    Ok(bytes)
}

/// Atomically replace one private persistence file. Data is written to a
/// create-new 0600 sibling, synced, renamed, and followed by a directory sync.
/// Replacing a symlink changes the link itself and never follows its target.
pub(crate) fn write_private_file_atomic(
    path: &Path,
    bytes: &[u8],
    max_bytes: usize,
) -> io::Result<()> {
    if bytes.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("data exceeds the {max_bytes} byte limit"),
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(parent)?;
    // Ownership, and not the mode bits. What has to be true of the result is
    // that jsh wrote a private file that jsh owns, and that is established
    // directly, on the descriptor: the temporary sibling below is created
    // `O_EXCL` at 0600, so a name planted by somebody else makes the create
    // fail rather than be adopted, and `rename` replaces a symlink rather than
    // following it. A loose mode on the directory lets another account unlink
    // the file; it does not let them read it, and it never makes jsh write
    // through something it did not create.
    //
    // Reading the bit as a refusal cost the whole feature in the place people
    // actually hit it: container images ship `$HOME` at 0777, so a shell in one
    // had no history and no z-jump, and said so at every start. A directory
    // owned by a *different* account is still refused — see `/tmp`.
    let directory_metadata = directory.metadata()?;
    if !directory_metadata.is_dir() || directory_metadata.uid() != unsafe { nix::libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "persistence directory is not owned by the current user \
                 (owner {}, effective user {})",
                directory_metadata.uid(),
                unsafe { nix::libc::geteuid() },
            ),
        ));
    }

    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let mut last_collision = None;
    for _ in 0..32 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".{}.jsh-tmp-{}-{counter}",
            name.to_string_lossy(),
            std::process::id()
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .mode(0o600)
            .open(&temp)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let result = (|| {
            file.write_all(bytes)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.sync_all()?;
            fs::rename(&temp, path)?;
            directory.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        return result;
    }
    Err(last_collision.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate persistence temporary file",
        )
    }))
}

/// Run a helper process while concurrently draining both output pipes. Either
/// stream exceeding its independent cap, or the deadline expiring, kills and
/// reaps the direct child and returns an error.
pub(crate) fn bounded_command_output(
    command: &mut Command,
    stdout_limit: usize,
    stderr_limit: usize,
    timeout: Duration,
) -> io::Result<Output> {
    bounded_command_session(
        command,
        BoundedCommand {
            stdout_limit,
            stderr_limit,
            timeout,
            stdin: None,
            cancel: None,
            die_with_parent: false,
            new_session: false,
        },
    )
}

/// [`bounded_command_output`] for a helper that must not be able to touch the
/// terminal jsh is drawing on.
///
/// An interactive bash is the one helper that goes looking. Job control is part
/// of what `-i` means, so before it runs a line it opens `/dev/tty`, reads the
/// foreground process group and tries to make itself that group. From a child
/// in a background process group `tcsetpgrp` raises `SIGTTOU`, whose default
/// action is to *stop* the process — so the helper never exits, jsh drains an
/// idle pipe until the deadline, and the startup file it was sourcing is
/// reported as a timeout. Detaching the helper into its own session leaves it
/// with no controlling terminal to find: bash says it has no job control, on
/// stderr, and gets on with the file.
pub(crate) fn bounded_command_output_detached(
    command: &mut Command,
    stdout_limit: usize,
    stderr_limit: usize,
    timeout: Duration,
) -> io::Result<Output> {
    bounded_command_session(
        command,
        BoundedCommand {
            stdout_limit,
            stderr_limit,
            timeout,
            stdin: None,
            cancel: None,
            die_with_parent: false,
            new_session: true,
        },
    )
}

/// Everything [`bounded_command_session`] needs beyond the command itself.
#[derive(Default)]
pub(crate) struct BoundedCommand<'a> {
    pub stdout_limit: usize,
    pub stderr_limit: usize,
    pub timeout: Duration,
    /// Bytes to hand the child on stdin. Written from a helper thread, because
    /// a payload larger than the pipe buffer would otherwise deadlock against
    /// our own drain loop: the child cannot read while it is blocked writing
    /// output nobody is consuming.
    pub stdin: Option<&'a [u8]>,
    /// Polled once per drain iteration — at most 100 ms apart. Returning true
    /// kills the process group and reports `Interrupted`.
    ///
    /// This is what makes a helper genuinely cancellable rather than merely
    /// abandoned. Dropping a channel and walking away leaves the work running,
    /// still holding whatever single-flight slot it was given, until its own
    /// timeout expires; killing the process group ends it now.
    pub cancel: Option<&'a (dyn Fn() -> bool + Sync)>,
    /// Ask the kernel to `SIGKILL` the child if this process dies.
    ///
    /// Every kill path here needs a live parent to run it, so a shell that is
    /// itself `SIGKILL`ed would otherwise orphan a helper that keeps holding a
    /// connection open. Work that used to run on a thread died with the process
    /// for free; a child has to be told. There is an unavoidable race — the
    /// parent can die between fork and `prctl` — so a helper still needs its own
    /// timeout as the backstop, but the window shrinks from minutes to
    /// microseconds.
    pub die_with_parent: bool,
    /// Put the child in a session of its own rather than merely a process group
    /// of its own, so that it has no controlling terminal.
    ///
    /// `setsid` makes the child a group leader too, so the timeout and cancel
    /// paths below still reach a whole helper tree through `kill(-pid)`. See
    /// [`bounded_command_output_detached`] for why anything wants this.
    pub new_session: bool,
}

/// [`bounded_command_output`] with a stdin payload and a cancellation predicate.
pub(crate) fn bounded_command_session(
    command: &mut Command,
    options: BoundedCommand<'_>,
) -> io::Result<Output> {
    let BoundedCommand {
        stdout_limit,
        stderr_limit,
        timeout,
        stdin: stdin_payload,
        cancel,
        die_with_parent,
        new_session,
    } = options;
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin_payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    if new_session {
        // SAFETY: runs between fork and exec in the child. setsid is a single
        // async-signal-safe syscall and touches no allocator or lock.
        //
        // It replaces `process_group(0)` rather than joining it: setsid fails
        // with EPERM on a process that is already a group leader, which is
        // exactly what `process_group(0)` would have made this one.
        unsafe {
            command.pre_exec(|| {
                if nix::libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    } else {
        // A helper that forks must not escape timeout cleanup merely by
        // leaving a descendant holding one of the capture pipes open.
        command.process_group(0);
    }
    if die_with_parent {
        // SAFETY: runs between fork and exec in the child. prctl is a single
        // async-signal-safe syscall and touches no allocator or lock, which is
        // the whole requirement for pre_exec.
        unsafe {
            command.pre_exec(|| {
                if nix::libc::prctl(nix::libc::PR_SET_PDEATHSIG, nix::libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command.spawn()?;

    // Scoped so the writer is joined on every exit path, including the kills
    // below: a detached writer would outlive the child holding a copy of
    // whatever secret the payload carries.
    let mut stdin_writer = None;
    if let Some(payload) = stdin_payload {
        match child.stdin.take() {
            Some(mut pipe) => {
                let bytes = payload.to_vec();
                stdin_writer = std::thread::Builder::new()
                    .name("jsh-helper-stdin".to_string())
                    .spawn(move || {
                        // EPIPE is the expected outcome when the child is
                        // killed mid-write, and is not worth reporting.
                        let _ = pipe.write_all(&bytes);
                    })
                    .ok();
            }
            None => {
                kill_and_reap(&mut child);
                return Err(io::Error::other("helper stdin pipe was not created"));
            }
        }
    }
    let result = bounded_command_drain(&mut child, stdout_limit, stderr_limit, timeout, cancel);
    if let Some(writer) = stdin_writer {
        let _ = writer.join();
    }
    result
}

fn bounded_command_drain(
    child: &mut Child,
    stdout_limit: usize,
    stderr_limit: usize,
    timeout: Duration,
    cancel: Option<&(dyn Fn() -> bool + Sync)>,
) -> io::Result<Output> {
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            kill_and_reap(child);
            return Err(io::Error::other("helper stdout pipe was not created"));
        }
    };
    let mut stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            kill_and_reap(child);
            return Err(io::Error::other("helper stderr pipe was not created"));
        }
    };
    if let Err(error) =
        set_nonblocking(stdout.as_raw_fd()).and_then(|()| set_nonblocking(stderr.as_raw_fd()))
    {
        kill_and_reap(child);
        return Err(error);
    }

    let deadline = Instant::now() + timeout;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_closed = false;
    let mut stderr_closed = false;
    let mut status = None;

    loop {
        let result = (|| {
            drain_pipe(
                &mut stdout,
                &mut stdout_bytes,
                stdout_limit,
                &mut stdout_closed,
            )?;
            drain_pipe(
                &mut stderr,
                &mut stderr_bytes,
                stderr_limit,
                &mut stderr_closed,
            )
        })();
        if let Err(error) = result {
            kill_and_reap(child);
            return Err(error);
        }

        if status.is_none() {
            status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    kill_and_reap(child);
                    return Err(error);
                }
            };
        }
        if let Some(status) = status {
            if stdout_closed && stderr_closed {
                return Ok(Output {
                    status,
                    stdout: stdout_bytes,
                    stderr: stderr_bytes,
                });
            }
        }

        if cancel.is_some_and(|predicate| predicate()) {
            kill_and_reap(child);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "helper process was cancelled",
            ));
        }

        if Instant::now() >= deadline {
            kill_and_reap(child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "helper process exceeded its time limit",
            ));
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = remaining.as_millis().min(100).try_into().unwrap_or(100);
        let mut descriptors = Vec::with_capacity(2);
        if !stdout_closed {
            descriptors.push(nix::libc::pollfd {
                fd: stdout.as_raw_fd(),
                events: nix::libc::POLLIN | nix::libc::POLLHUP | nix::libc::POLLERR,
                revents: 0,
            });
        }
        if !stderr_closed {
            descriptors.push(nix::libc::pollfd {
                fd: stderr.as_raw_fd(),
                events: nix::libc::POLLIN | nix::libc::POLLHUP | nix::libc::POLLERR,
                revents: 0,
            });
        }
        let polled = unsafe {
            nix::libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len().try_into().unwrap_or(0),
                timeout_ms,
            )
        };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                kill_and_reap(child);
                return Err(error);
            }
        }
    }
}

fn kill_and_reap(child: &mut Child) {
    if let Ok(process_group) = i32::try_from(child.id()) {
        // The child is the leader of the process group configured above.
        let _ = unsafe { nix::libc::kill(-process_group, nix::libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn set_nonblocking(fd: i32) -> io::Result<()> {
    let flags = unsafe { nix::libc::fcntl(fd, nix::libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { nix::libc::fcntl(fd, nix::libc::F_SETFL, flags | nix::libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_pipe(
    reader: &mut impl Read,
    output: &mut Vec<u8>,
    limit: usize,
    closed: &mut bool,
) -> io::Result<()> {
    if *closed {
        return Ok(());
    }
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                *closed = true;
                return Ok(());
            }
            Ok(read) => {
                if output.len().saturating_add(read) > limit {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("helper output exceeds the {limit} byte limit"),
                    ));
                }
                output.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn a_root_shell_still_trusts_the_system_helpers_it_could_write() {
        const ROOT: u32 = 0;
        const USER: u32 = 1000;

        // What a distribution actually ships. Under the old rule root matched
        // "owner, and owner can write", so every automatic helper was refused
        // in a container and Git completion silently went missing.
        assert!(!replaceable(0o755, ROOT, ROOT), "root-owned /usr/bin/git");
        assert!(!replaceable(0o755, ROOT, USER), "the same path as a user");

        // Root gains nothing else: a helper some other account owns is still
        // that account's to replace, and group/world-writable is still refused
        // no matter who is asking.
        assert!(replaceable(0o755, USER, ROOT), "owned by another user");
        assert!(replaceable(0o775, ROOT, ROOT), "group-writable");
        assert!(replaceable(0o757, ROOT, ROOT), "world-writable");

        // Unprivileged behaviour is unchanged: a path this user owns and can
        // write is reachable by anything already running as them.
        assert!(replaceable(0o755, USER, USER), "self-owned and writable");
        assert!(!replaceable(0o555, USER, USER), "self-owned, read-only");
    }

    #[test]
    fn private_persistence_is_bounded_atomic_and_does_not_follow_symlinks() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
            .expect("private tempdir");
        let path = temp.path().join("state");
        write_private_file_atomic(&path, b"first", 32).expect("initial write");
        assert_eq!(read_private_file(&path, 32).unwrap(), b"first");
        assert!(read_private_file(&path, 4).is_err());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o622)).expect("unsafe mode");
        assert!(read_private_file(&path, 32).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("private mode");

        let victim = temp.path().join("victim");
        fs::write(&victim, "keep").expect("victim");
        fs::remove_file(&path).expect("remove state");
        symlink(&victim, &path).expect("state symlink");
        assert!(read_private_file(&path, 32).is_err());
        write_private_file_atomic(&path, b"second", 32).expect("replace link");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(read_private_file(&path, 32).unwrap(), b"second");
    }

    #[test]
    fn a_cancelled_helper_dies_promptly_instead_of_running_to_its_deadline() {
        // The point of the predicate: without it the only way out of a helper
        // that has stopped being wanted is its own timeout, which for a model
        // request is measured in minutes.
        let started = Instant::now();
        let cancel = AtomicBool::new(false);
        let predicate = move || {
            // False once, so the helper is genuinely running when it is killed
            // rather than being refused before it starts.
            !cancel.swap(true, Ordering::SeqCst)
        };
        let error = bounded_command_session(
            Command::new("sleep").arg("120"),
            BoundedCommand {
                stdout_limit: 64,
                stderr_limit: 64,
                timeout: Duration::from_secs(120),
                stdin: None,
                cancel: Some(&predicate),
                die_with_parent: false,
                new_session: false,
            },
        )
        .expect_err("cancelled helper must not succeed");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cancellation waited for the deadline: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn an_uncancelled_helper_still_runs_to_completion() {
        let never = || false;
        let output = bounded_command_session(
            Command::new("printf").arg("done"),
            BoundedCommand {
                stdout_limit: 16,
                stderr_limit: 16,
                timeout: Duration::from_secs(10),
                stdin: None,
                cancel: Some(&never),
                die_with_parent: false,
                new_session: false,
            },
        )
        .expect("helper");
        assert_eq!(output.stdout, b"done");
    }

    /// Field 6 of `/proc/PID/stat` is the session id. `comm` is field 2 and can
    /// contain spaces, but not for the two commands used here.
    #[cfg(target_os = "linux")]
    fn session_of_helper(detached: bool) -> u32 {
        let mut command = Command::new("sh");
        command.args(["-c", "cut -d' ' -f6 /proc/self/stat"]);
        let output = if detached {
            bounded_command_output_detached(&mut command, 64, 64, Duration::from_secs(10))
        } else {
            bounded_command_output(&mut command, 64, 64, Duration::from_secs(10))
        }
        .expect("helper");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("session id")
    }

    /// A `$HOME` that anyone can write is the normal state of a container
    /// image, and it is not a reason to drop the user's data on the floor. The
    /// file is still created `O_EXCL` at 0600 under a name only this process
    /// knows, and still arrives by `rename`, so it is still private and still
    /// jsh's — none of which the directory's mode had any part in.
    #[test]
    fn a_world_writable_home_still_persists() {
        let home = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o777)).expect("0777 home");

        let path = home.path().join(".jsh_z");
        write_private_file_atomic(&path, b"/p|1.5|10\n", 4096).expect("persist under a 0777 home");

        assert_eq!(fs::read(&path).expect("read back"), b"/p|1.5|10\n");
        assert_eq!(
            fs::metadata(&path).expect("metadata").mode() & 0o7777,
            0o600,
            "the file itself must still be private"
        );
    }

    /// The half of the rule that stays: somebody else's directory is somebody
    /// else's. `/tmp` is the one every system has — root-owned and 1777.
    #[test]
    fn a_directory_owned_by_another_user_is_still_refused() {
        if unsafe { nix::libc::geteuid() } == 0 {
            return; // root owns /tmp, so there is no third party to test against
        }
        let error = write_private_file_atomic(Path::new("/tmp/.jsh-guard-probe"), b"x", 16)
            .expect_err("a root-owned directory must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains("not owned by the current user"),
            "{error}"
        );
    }

    /// The property that keeps an interactive helper bash from stopping itself
    /// on `SIGTTOU`: with no session of its own it shares jsh's controlling
    /// terminal, opens `/dev/tty`, and tries to take the foreground.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_detached_helper_leaves_the_terminal_alone() {
        let ours = unsafe { nix::libc::getsid(0) } as u32;
        assert_eq!(
            session_of_helper(false),
            ours,
            "an ordinary helper should stay in this session"
        );
        assert_ne!(
            session_of_helper(true),
            ours,
            "a detached helper still shares jsh's session, so it still shares \
             jsh's controlling terminal"
        );
    }

    #[test]
    fn a_stdin_payload_larger_than_the_pipe_buffer_does_not_deadlock() {
        // 1 MiB is far past any pipe buffer, so a child that reads its input
        // only while we are draining its output is the whole test: writing the
        // payload inline from this thread would wedge both sides forever.
        let payload = vec![b'x'; 1024 * 1024];
        let output = bounded_command_session(
            Command::new("wc").arg("-c"),
            BoundedCommand {
                stdout_limit: 64,
                stderr_limit: 64,
                timeout: Duration::from_secs(30),
                stdin: Some(&payload),
                cancel: None,
                die_with_parent: false,
                new_session: false,
            },
        )
        .expect("helper");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            payload.len().to_string()
        );
    }

    #[test]
    fn killing_a_helper_mid_payload_does_not_wedge_the_writer() {
        // The child exits without reading, so the writer thread meets EPIPE.
        // The session must still return rather than blocking on the join.
        let payload = vec![b'x'; 4 * 1024 * 1024];
        let started = Instant::now();
        let output = bounded_command_session(
            &mut Command::new("true"),
            BoundedCommand {
                stdout_limit: 64,
                stderr_limit: 64,
                timeout: Duration::from_secs(30),
                stdin: Some(&payload),
                cancel: None,
                die_with_parent: false,
                new_session: false,
            },
        )
        .expect("helper");
        assert!(output.status.success());
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "writer join blocked: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn helper_output_is_bounded() {
        let output = bounded_command_output(
            Command::new("printf").arg("hello"),
            5,
            0,
            Duration::from_secs(2),
        )
        .expect("bounded helper");
        assert_eq!(output.stdout, b"hello");

        assert!(bounded_command_output(
            Command::new("printf").arg("too-large"),
            4,
            0,
            Duration::from_secs(2),
        )
        .is_err());

        let started = Instant::now();
        assert!(bounded_command_output(
            Command::new("/bin/sh").args(["-c", "sleep 5 & exit 0"]),
            16,
            16,
            Duration::from_millis(50),
        )
        .is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn automatic_helpers_never_resolve_through_path() {
        assert!(automatic_system_helper("not-a-helper").is_none());
        if let Some(git) = automatic_system_helper("git") {
            assert!(git.is_absolute());
            assert_ne!(git, Path::new("git"));
        }
        assert!(!explicit_absolute_executable(Path::new("relative-helper")));
    }

    #[test]
    fn an_explicit_helper_must_be_trustworthy_all_the_way_down() {
        // A real system binary: root-owned file under a root-owned chain.
        for candidate in ["/bin/sh", "/usr/bin/env", "/bin/cat"] {
            let path = Path::new(candidate);
            if path.exists() {
                assert!(
                    explicit_absolute_executable(path),
                    "{candidate} should be trusted"
                );
            }
        }

        assert!(!explicit_absolute_executable(Path::new("relative")));
        assert!(!explicit_absolute_executable(Path::new("/no/such/helper")));

        // The interesting case, and the reason the whole directory chain is
        // walked: the file itself is private and the directory holding it is
        // 0700, but it sits under a world-writable /tmp, which can be renamed
        // out of the way and replaced wholesale. Validating only the leaf would
        // accept this.
        let temp = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).expect("private dir");
        let configured = temp.path().join("configured-helper");
        fs::write(&configured, "#!/bin/sh\n").expect("helper fixture");
        fs::set_permissions(&configured, fs::Permissions::from_mode(0o700))
            .expect("executable mode");
        let under_world_writable = temp.path().ancestors().any(|ancestor| {
            fs::metadata(ancestor).is_ok_and(|metadata| metadata.mode() & 0o022 != 0)
        });
        assert_eq!(
            explicit_absolute_executable(&configured),
            !under_world_writable,
            "chain trust disagreed with the ancestors of {}",
            configured.display()
        );

        // A regular file with no execute bit is not a helper.
        let data = temp.path().join("not-executable");
        fs::write(&data, "").expect("data fixture");
        fs::set_permissions(&data, fs::Permissions::from_mode(0o600)).expect("data mode");
        assert!(!explicit_absolute_executable(&data));
    }

    #[test]
    fn a_group_of_one_is_not_somebody_else() {
        let nobody: [String; 0] = [];
        // The stock Ubuntu shape: user-private group, umask 002, so every
        // helper under $HOME is 0775 and the group is the owner alone.
        assert!(!group_reaches_others(1000, 1000, "ubuntu", &nobody));
        // A second member of that group can write through the bit.
        assert!(group_reaches_others(
            1000,
            1000,
            "ubuntu",
            &["deploy".to_string()]
        ));
        // Owner listed in its own group changes nothing.
        assert!(!group_reaches_others(
            1000,
            1000,
            "ubuntu",
            &["ubuntu".to_string()]
        ));
        // A shared group the owner merely belongs to is not private, even
        // while empty: its membership is not the owner's to keep at one.
        assert!(group_reaches_others(50, 1000, "ubuntu", &nobody));
    }

    #[test]
    fn a_helper_under_a_user_private_group_stays_usable() {
        // What a `pip install --user` or a conda prefix looks like on a
        // distribution shipping umask 002.
        let temp = tempfile::tempdir().expect("tempdir");
        let helper = temp.path().join("group-writable-helper");
        fs::write(&helper, "#!/bin/sh\n").expect("helper fixture");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o775)).expect("group-writable");
        let metadata = fs::metadata(&helper).expect("helper metadata");
        assert_eq!(
            trusted_path_component(&metadata),
            !group_write_reaches_others(metadata.gid(), metadata.uid()),
            "0775 was judged by the bit rather than by who is in the group"
        );

        // World-writable is still refused with no lookup at all.
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o777)).expect("world-writable");
        assert!(!trusted_path_component(
            &fs::metadata(&helper).expect("helper metadata")
        ));
    }

    #[test]
    fn helper_variable_names_are_derived_predictably() {
        assert_eq!(helper_path_variable("git"), "JSH_HELPER_GIT");
        assert_eq!(helper_path_variable("bash"), "JSH_HELPER_BASH");
        assert_eq!(
            helper_path_variable("notify-send"),
            "JSH_HELPER_NOTIFY_SEND"
        );
    }

    #[test]
    fn an_unset_helper_variable_leaves_the_automatic_candidate_alone() {
        // No override configured in this process, so resolution must be exactly
        // what the fixed candidate list already produced.
        assert_eq!(
            trusted_helper("git"),
            automatic_system_helper("git").map(Path::to_path_buf)
        );
        assert!(trusted_helper("not-a-helper").is_none());
    }
}
