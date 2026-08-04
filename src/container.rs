//! Entering a container without installing anything inside it.
//!
//! `docker run -it ubuntu bash` and `docker exec -it web bash` are how people
//! actually enter a container, and both land in whatever shell the image
//! happens to carry. jsh can be that shell without being installed there.
//!
//! A container's `/dev` is a tmpfs the runtime creates, and — unlike
//! `/dev/shm`, which is mounted `noexec` — a binary can be executed from it.
//! So for `docker run` the shell is a **read-only bind mount** of a static jsh
//! over a path in that tmpfs: nothing is copied, nothing is written to the
//! image's layers, and `docker diff` on the container stays empty. For a
//! container that is already running there is no way to add a mount, so the
//! binary is streamed into that same tmpfs instead — still nothing in the
//! writable layer, and it is gone when the container stops.
//!
//! Rewriting a command a person typed is a serious thing to do, so the rules
//! are deliberately narrow and every one of them fails closed:
//!
//!   * only `run` and `exec`, only with a terminal requested, and only when
//!     the command is a bare shell (or, for `run`, when the image's own
//!     default command is one);
//!   * only against a local daemon — with a remote `DOCKER_HOST` a `-v` would
//!     mount a path on *that* machine, which is someone else's filesystem;
//!   * only when every flag between the subcommand and the operand is one this
//!     module knows, because an unrecognised flag means the operand cannot be
//!     located with certainty;
//!   * never when the user already said `--entrypoint`, or asked for a
//!     platform this host's binary would not run on.
//!
//! Anything outside that runs exactly as typed. `command docker …` bypasses
//! this entirely (it goes through `spawn_external`), and
//! `JSH_CONTAINER_SHELL=off` turns it off for good.

use std::path::{Path, PathBuf};

use crate::environment::ShellState;

/// Where the injected shell lives inside the container. In `/dev` because that
/// is a runtime tmpfs: the mount point costs the image layer nothing, and a
/// copy written there disappears with the container.
const CONTAINER_PATH: &str = "/dev/.jsh";

/// Shells a person types when they mean "give me a prompt in there".
const SHELL_NAMES: &[&str] = &["bash", "sh", "zsh", "ash", "dash", "busybox"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Engine {
    Docker,
    Podman,
    Nerdctl,
}

impl Engine {
    fn from_program(program: &str) -> Option<Self> {
        let name = Path::new(program).file_name()?.to_str()?;
        match name {
            "docker" => Some(Self::Docker),
            "podman" => Some(Self::Podman),
            "nerdctl" => Some(Self::Nerdctl),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// A container that does not exist yet: the shell can be bind-mounted in.
    Run,
    /// A container that is already running: the shell has to be streamed into
    /// its tmpfs first.
    Exec,
}

/// What to do to an argv that enters a container, decided without running
/// anything. Kept separate from the doing so it can be asserted directly.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Plan {
    mode: Mode,
    /// Index just after the subcommand, where inserted flags are valid for
    /// every engine regardless of what the rest of the line looks like.
    insert_at: usize,
    /// The image (`run`) or container (`exec`).
    operand: String,
    /// The shell token to replace, if the user named one. `None` means the
    /// command was left to the image, and the shell is appended instead.
    shell_at: Option<usize>,
}

/// Flags that consume the next argument. An unknown flag stops the whole
/// analysis rather than being guessed at, so this list only has to cover what
/// people actually type — a miss costs the rewrite, never the command.
const VALUE_FLAGS: &[&str] = &[
    "--add-host",
    "--annotation",
    "--attach",
    "--blkio-weight",
    "--cap-add",
    "--cap-drop",
    "--cgroup-parent",
    "--cgroupns",
    "--cidfile",
    "--cpu-period",
    "--cpu-quota",
    "--cpu-shares",
    "--cpus",
    "--cpuset-cpus",
    "--cpuset-mems",
    "--detach-keys",
    "--device",
    "--dns",
    "--dns-option",
    "--dns-search",
    "--domainname",
    "--entrypoint",
    "--env",
    "--env-file",
    "--expose",
    "--gpus",
    "--group-add",
    "--health-cmd",
    "--health-interval",
    "--health-retries",
    "--health-timeout",
    "--hostname",
    "--ip",
    "--ipc",
    "--label",
    "--label-file",
    "--link",
    "--log-driver",
    "--log-opt",
    "--mac-address",
    "--memory",
    "--memory-reservation",
    "--memory-swap",
    "--mount",
    "--name",
    "--network",
    "--net",
    "--network-alias",
    "--pid",
    "--platform",
    "--publish",
    "--pull",
    "--restart",
    "--runtime",
    "--security-opt",
    "--shm-size",
    "--stop-signal",
    "--stop-timeout",
    "--storage-opt",
    "--sysctl",
    "--tmpfs",
    "--ulimit",
    "--user",
    "--userns",
    "--uts",
    "--volume",
    "--volume-driver",
    "--volumes-from",
    "--workdir",
];

/// Long flags that stand alone.
const BOOL_FLAGS: &[&str] = &[
    "--detach",
    "--disable-content-trust",
    "--init",
    "--interactive",
    "--no-healthcheck",
    "--oom-kill-disable",
    "--privileged",
    "--publish-all",
    "--quiet",
    "--read-only",
    "--rm",
    "--tty",
];

/// Short flags that consume the next argument.
const SHORT_VALUE_FLAGS: &[char] = &[
    'a', 'c', 'e', 'h', 'l', 'm', 'p', 'u', 'v',
    'w', // attach, cpu-shares, env,
        // hostname, label, memory,
        // publish, user, volume,
        // workdir
];

/// Short flags that stand alone, including the two this module looks for.
const SHORT_BOOL_FLAGS: &[char] = &['d', 'i', 't', 'P', 'q'];

/// Rewrite an argv that enters a container so the shell inside it is jsh.
///
/// Returns `None` — leaving the command exactly as typed — for anything this
/// cannot do confidently, which is most things.
pub(crate) fn upgrade_entry(argv: &[String], state: &ShellState) -> Option<Vec<String>> {
    if !enabled(state) || !state.interactive {
        return None;
    }
    let engine = Engine::from_program(argv.first()?)?;
    let plan = plan(argv)?;
    if !daemon_is_local(argv) {
        return None;
    }
    let binary = static_jsh()?;

    match plan.mode {
        Mode::Run => {
            if plan.shell_at.is_none() && !image_default_is_a_shell(engine, &plan.operand) {
                // Its default command is a program someone wants to run, not a
                // prompt: `docker run -it postgres` means postgres.
                return None;
            }
            let mut argv = argv.to_vec();
            match plan.shell_at {
                Some(index) => argv[index] = CONTAINER_PATH.to_string(),
                None => argv.push(CONTAINER_PATH.to_string()),
            }
            let mut inserted = vec![
                "-v".to_string(),
                format!("{}:{CONTAINER_PATH}:ro", binary.display()),
            ];
            inserted.extend(terminal_env_flags());
            argv.splice(plan.insert_at..plan.insert_at, inserted);
            eprintln!("jsh: entering the container as jsh (read-only mount, nothing installed)");
            Some(argv)
        }
        Mode::Exec => {
            // A running container cannot be given a mount, so the binary has
            // to reach its tmpfs the only way left: over the exec's stdin.
            if !place_binary_in_container(engine, &plan.operand, &binary) {
                return None;
            }
            let mut argv = argv.to_vec();
            argv[plan.shell_at?] = CONTAINER_PATH.to_string();
            argv.splice(plan.insert_at..plan.insert_at, terminal_env_flags());
            eprintln!("jsh: entering the container as jsh (in its tmpfs, gone when it stops)");
            Some(argv)
        }
    }
}

fn enabled(state: &ShellState) -> bool {
    let setting = state
        .get_var("JSH_CONTAINER_SHELL")
        .map(str::to_string)
        .or_else(|| std::env::var("JSH_CONTAINER_SHELL").ok());
    !matches!(
        setting.as_deref().map(str::trim),
        Some("off" | "0" | "no" | "false")
    )
}

/// Decide what an argv is doing, using nothing but the argv.
fn plan(argv: &[String]) -> Option<Plan> {
    let mut index = 1;
    // Global flags sit before the subcommand. The ones that matter here name a
    // different daemon, and those are refused outright by `daemon_is_local`.
    while let Some(token) = argv.get(index) {
        if !token.starts_with('-') {
            break;
        }
        index += flag_width(token)?;
    }
    let mode = match argv.get(index)?.as_str() {
        "run" => Mode::Run,
        "exec" => Mode::Exec,
        _ => return None,
    };
    let insert_at = index + 1;

    let mut wants_terminal = false;
    let mut index = insert_at;
    while let Some(token) = argv.get(index) {
        if !token.starts_with('-') || token == "-" {
            break;
        }
        if token == "--entrypoint" || token.starts_with("--entrypoint=") {
            // The user has already said what to run.
            return None;
        }
        if token == "--platform" || token.starts_with("--platform=") {
            let value = match token.split_once('=') {
                Some((_, value)) => value,
                None => argv.get(index + 1)?.as_str(),
            };
            if !platform_matches_host(value) {
                return None;
            }
        }
        if requests_terminal(token) {
            wants_terminal = true;
        }
        index += flag_width(token)?;
    }
    if !wants_terminal {
        // No terminal means this is a pipeline or a script step, not someone
        // going in to look around.
        return None;
    }

    let operand = argv.get(index)?.clone();
    if operand.starts_with('-') {
        return None;
    }
    let rest = &argv[index + 1..];
    let shell_at = match rest.len() {
        0 => None,
        1 if is_shell_name(&rest[0]) => Some(index + 1),
        // `bash -lc …` is a command, not a prompt.
        _ => return None,
    };
    if mode == Mode::Exec && shell_at.is_none() {
        return None;
    }
    Some(Plan {
        mode,
        insert_at,
        operand,
        shell_at,
    })
}

/// How many argv slots a flag occupies, or `None` when it is not one this
/// module recognises — which stops the analysis, because an unknown flag may
/// or may not swallow the token after it.
fn flag_width(token: &str) -> Option<usize> {
    if token.starts_with("--") {
        if token.contains('=') {
            return Some(1);
        }
        if VALUE_FLAGS.contains(&token) {
            return Some(2);
        }
        if BOOL_FLAGS.contains(&token) {
            return Some(1);
        }
        return None;
    }
    // A short cluster: every letter but the last must stand alone, and the
    // last decides whether the next token belongs to it (`-itv /a:/b`).
    let letters: Vec<char> = token.strip_prefix('-')?.chars().collect();
    if letters.is_empty() {
        return None;
    }
    let (last, leading) = letters.split_last()?;
    if leading.iter().any(|c| !SHORT_BOOL_FLAGS.contains(c)) {
        return None;
    }
    if SHORT_BOOL_FLAGS.contains(last) {
        Some(1)
    } else if SHORT_VALUE_FLAGS.contains(last) {
        Some(2)
    } else {
        None
    }
}

fn requests_terminal(token: &str) -> bool {
    if token == "--tty" {
        return true;
    }
    token.starts_with('-')
        && !token.starts_with("--")
        && token.chars().skip(1).any(|c| c == 't')
        && flag_width(token).is_some()
}

fn is_shell_name(token: &str) -> bool {
    Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SHELL_NAMES.contains(&name))
}

/// A `-v` names a path on the machine the daemon runs on. When that is not
/// this machine, the mount would come from a filesystem we know nothing about.
fn daemon_is_local(argv: &[String]) -> bool {
    let names_another_daemon = argv
        .iter()
        .take_while(|token| *token != "run")
        .any(|token| {
            matches!(token.as_str(), "-H" | "--host" | "-c" | "--context")
                || token.starts_with("--host=")
                || token.starts_with("--context=")
        });
    if names_another_daemon {
        return false;
    }
    match std::env::var("DOCKER_HOST") {
        Ok(host) if !host.is_empty() => host.starts_with("unix://"),
        _ => std::env::var("DOCKER_CONTEXT").is_err(),
    }
}

fn platform_matches_host(platform: &str) -> bool {
    let arch = platform.rsplit('/').next().unwrap_or(platform);
    let host = std::env::consts::ARCH;
    matches!(
        (arch, host),
        ("amd64" | "x86_64", "x86_64") | ("arm64" | "aarch64", "aarch64")
    )
}

/// `TERM` and `COLORTERM` decide whether everything in the session draws in
/// 256 colours or 8, and neither `docker run` nor `docker exec` inherits them:
/// the daemon substitutes its own default. Only a bare terminal name is
/// forwarded, because these become arguments.
fn terminal_env_flags() -> Vec<String> {
    let mut flags = Vec::new();
    for name in ["TERM", "COLORTERM"] {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        if value.is_empty()
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            continue;
        }
        flags.push("-e".to_string());
        flags.push(format!("{name}={value}"));
    }
    flags
}

/// A jsh that will run in any container: statically linked, so it needs
/// nothing from the image — not even a libc.
pub(crate) fn static_jsh() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("JSH_CONTAINER_BINARY") {
        let path = PathBuf::from(configured);
        return is_static_elf(&path)
            .then(|| readable_inside(&path))
            .flatten();
    }
    // This shell, when it is itself static. Nothing to configure, nothing to
    // build, and the version inside always matches the version outside.
    if let Ok(own) = std::fs::read_link("/proc/self/exe") {
        if is_static_elf(&own) {
            return readable_inside(&own);
        }
    }
    // Otherwise the artifact `jsh-remote.sh` stages for the same purpose.
    let staged = dirs::cache_dir()?
        .join("jsh/remote")
        .join(format!("v{}", env!("CARGO_PKG_VERSION")))
        .join(format!("{}-unknown-linux-musl", std::env::consts::ARCH))
        .join("jsh");
    is_static_elf(&staged)
        .then(|| readable_inside(&staged))
        .flatten()
}

/// The mount is read by whoever the container runs as, and that is often not
/// root: an image with its own `USER`, or a `--user` on the command line, sees
/// the host file's permissions and nothing else. A private artifact — the
/// staged one is 0700 — would make the container fail to start at all, so it
/// is republished once as a readable copy rather than being handed over as is.
fn readable_inside(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let source = std::fs::metadata(path).ok()?;
    if source.mode() & 0o005 == 0o005 {
        return Some(path.to_path_buf());
    }
    let copy = dirs::cache_dir()?.join("jsh/container/jsh");
    if std::fs::metadata(&copy).is_ok_and(|existing| {
        existing.len() == source.len() && existing.modified().ok() == source.modified().ok()
    }) {
        return Some(copy);
    }
    let parent = copy.parent()?;
    std::fs::create_dir_all(parent).ok()?;
    let staging = parent.join(format!("jsh.{}", std::process::id()));
    std::fs::copy(path, &staging).ok()?;
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755)).ok()?;
    // Same mtime, so the check above recognises this copy as current.
    if let Ok(source_time) = source.modified() {
        let _ = std::fs::File::open(&staging)
            .and_then(|file| file.set_times(std::fs::FileTimes::new().set_modified(source_time)));
    }
    match std::fs::rename(&staging, &copy) {
        Ok(()) => Some(copy),
        Err(_) => {
            let _ = std::fs::remove_file(&staging);
            None
        }
    }
}

/// A dynamically linked binary carries a hard-coded interpreter path that the
/// image almost certainly does not have, and fails with a "no such file or
/// directory" that names a file which is plainly there. Read the ELF program
/// headers instead of finding that out inside the container.
fn is_static_elf(path: &Path) -> bool {
    let Ok(bytes) = crate::io_guard::read_regular_file_prefix(path, 64 * 1024) else {
        return false;
    };
    if bytes.len() < 64 || &bytes[0..4] != b"\x7fELF" || bytes[4] != 2 {
        return false; // not a 64-bit ELF
    }
    let word = |offset: usize| -> u64 {
        let mut value = [0u8; 8];
        value.copy_from_slice(&bytes[offset..offset + 8]);
        u64::from_le_bytes(value)
    };
    let half = |offset: usize| -> u16 { u16::from_le_bytes([bytes[offset], bytes[offset + 1]]) };
    let program_headers = word(0x20) as usize;
    let entry_size = half(0x36) as usize;
    let count = half(0x38) as usize;
    if entry_size < 8 || program_headers == 0 {
        return false;
    }
    for index in 0..count {
        let offset = program_headers + index * entry_size;
        if offset + 4 > bytes.len() {
            return false;
        }
        let kind = u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]);
        if kind == 3 {
            return false; // PT_INTERP: needs a dynamic loader from the image
        }
    }
    true
}

/// Put the binary in the running container's tmpfs, unless the one already
/// there is this same jsh. Streamed over the exec's stdin rather than
/// `docker cp`, which would write into the container's filesystem.
fn place_binary_in_container(engine: Engine, container: &str, binary: &Path) -> bool {
    let program = engine_program(engine);
    let banner = format!("jsh {}", env!("CARGO_PKG_VERSION"));
    let mut probe = std::process::Command::new(program);
    probe.args(["exec", container, CONTAINER_PATH, "--version"]);
    if let Ok(output) = crate::io_guard::bounded_command_output(
        &mut probe,
        4 * 1024,
        4 * 1024,
        std::time::Duration::from_secs(10),
    ) {
        if String::from_utf8_lossy(&output.stdout).starts_with(&banner) {
            return true;
        }
    }

    let Ok(bytes) = crate::io_guard::read_regular_file(binary, 64 * 1024 * 1024) else {
        return false;
    };
    // Written to a temporary name and renamed, so a session that starts while
    // this one is copying never executes half a binary.
    let script = format!(
        "cat > {CONTAINER_PATH}.incoming && chmod 755 {CONTAINER_PATH}.incoming \
         && mv -f {CONTAINER_PATH}.incoming {CONTAINER_PATH}"
    );
    // The push runs as the image's default user, and /dev belongs to root: an
    // image that says `USER ubuntu` gets its write refused. Anyone allowed to
    // talk to the daemon already has root inside the container, so a refusal
    // is retried once as uid 0 — the binary lands 0755 in the tmpfs, and the
    // session itself still runs as whoever the person asked for.
    for user in [None, Some("0")] {
        let mut push = std::process::Command::new(program);
        push.arg("exec");
        if let Some(uid) = user {
            push.args(["--user", uid]);
        }
        push.args(["-i", container, "/bin/sh", "-c", &script]);
        let pushed = crate::io_guard::bounded_command_session(
            &mut push,
            crate::io_guard::BoundedCommand {
                stdout_limit: 4 * 1024,
                stderr_limit: 4 * 1024,
                timeout: std::time::Duration::from_secs(60),
                stdin: Some(&bytes),
                cancel: None,
                die_with_parent: true,
            },
        )
        .is_ok_and(|output| output.status.success());
        if pushed {
            return true;
        }
    }
    false
}

/// Does this image start a shell when nobody says otherwise? Only then can the
/// command be left off and still mean "give me a prompt".
fn image_default_is_a_shell(engine: Engine, image: &str) -> bool {
    let mut command = std::process::Command::new(engine_program(engine));
    command.args([
        "image",
        "inspect",
        "--format",
        "{{json .Config.Entrypoint}} {{json .Config.Cmd}}",
        image,
    ]);
    let Ok(output) = crate::io_guard::bounded_command_output(
        &mut command,
        16 * 1024,
        16 * 1024,
        std::time::Duration::from_secs(10),
    ) else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some((entrypoint, cmd)) = text.trim().split_once(' ') else {
        return false;
    };
    // An entrypoint takes the command as its arguments, so replacing the
    // command would not replace the program.
    if entrypoint != "null" && entrypoint != "[]" {
        return false;
    }
    cmd.trim_matches(|c| c == '[' || c == ']' || c == '"')
        .split(&['"', ','][..])
        .rfind(|part| !part.trim().is_empty())
        .is_some_and(is_shell_name)
}

fn engine_program(engine: Engine) -> &'static str {
    match engine {
        Engine::Docker => "docker",
        Engine::Podman => "podman",
        Engine::Nerdctl => "nerdctl",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    fn planned(line: &str) -> Option<Plan> {
        plan(&argv(line))
    }

    #[test]
    fn a_typed_shell_is_the_one_that_gets_replaced() {
        let plan = planned("docker run -it ubuntu bash").expect("plan");
        assert_eq!(plan.mode, Mode::Run);
        assert_eq!(plan.operand, "ubuntu");
        // Index 4 is `bash`; index 2 is where inserted flags are always legal.
        assert_eq!(plan.shell_at, Some(4));
        assert_eq!(plan.insert_at, 2);

        let plan = planned("docker exec -it web bash").expect("plan");
        assert_eq!(plan.mode, Mode::Exec);
        assert_eq!(plan.operand, "web");
        assert_eq!(plan.shell_at, Some(4));
    }

    #[test]
    fn flags_that_swallow_their_value_do_not_shift_the_operand() {
        let plan = planned("docker run -it -v /a:/b -e X=1 --name box ubuntu bash").expect("plan");
        assert_eq!(plan.operand, "ubuntu");
        assert_eq!(plan.shell_at, Some(10));

        // Clustered shorts, with the value-taking letter last.
        let plan = planned("docker run -itv /a:/b ubuntu bash").expect("plan");
        assert_eq!(plan.operand, "ubuntu");

        // `--flag=value` is unambiguous on its own.
        let plan = planned("docker run -it --name=box ubuntu bash").expect("plan");
        assert_eq!(plan.operand, "ubuntu");
    }

    #[test]
    fn an_image_named_like_a_shell_is_still_the_image() {
        // `docker run -it bash` runs the image called bash. Treating the last
        // token as a command would have replaced the image itself.
        let plan = planned("docker run -it bash").expect("plan");
        assert_eq!(plan.operand, "bash");
        assert_eq!(plan.shell_at, None);
    }

    #[test]
    fn anything_that_is_not_someone_going_in_to_look_around_is_left_alone() {
        for line in [
            // No terminal: a pipeline or a script step.
            "docker run ubuntu bash",
            "docker exec web bash",
            // A command, not a prompt.
            "docker run -it ubuntu bash -c echo",
            "docker exec -it web ls /tmp",
            // The user already said what to run.
            "docker run -it --entrypoint sh ubuntu",
            // Not a subcommand that enters anything.
            "docker build -t x .",
            "docker ps",
            // exec has no image default to fall back on.
            "docker exec -it web",
            // An unknown flag could be hiding the operand behind its value.
            "docker run -it --frobnicate value ubuntu bash",
        ] {
            assert!(planned(line).is_none(), "should not have rewritten: {line}");
        }
    }

    #[test]
    fn another_daemon_owns_a_different_filesystem() {
        // `-v` would name a path on that machine, not this one.
        assert!(!daemon_is_local(&argv(
            "docker -H tcp://box:2375 run -it ubuntu bash"
        )));
        assert!(!daemon_is_local(&argv(
            "docker --context remote run -it ubuntu bash"
        )));
        assert!(daemon_is_local(&argv("docker run -it ubuntu bash")));
    }

    #[test]
    fn a_platform_this_binary_could_not_run_on_is_refused() {
        assert!(planned("docker run -it --platform linux/arm64 ubuntu bash").is_none());
        assert_eq!(
            planned(&format!(
                "docker run -it --platform linux/{} ubuntu bash",
                if std::env::consts::ARCH == "x86_64" {
                    "amd64"
                } else {
                    "arm64"
                }
            ))
            .map(|plan| plan.operand),
            Some("ubuntu".to_string())
        );
    }

    #[test]
    fn the_other_engines_use_the_same_command_line() {
        for program in ["podman", "nerdctl", "/usr/bin/docker"] {
            assert!(Engine::from_program(program).is_some(), "{program}");
            assert!(planned(&format!("{program} run -it fedora bash")).is_some());
        }
        assert!(Engine::from_program("dockerd").is_none());
        assert!(Engine::from_program("kubectl").is_none());
    }

    #[test]
    fn a_dynamically_linked_shell_is_not_offered_to_a_container() {
        // The running test binary is linked against this machine's libc, which
        // is exactly what must never be handed to an image.
        let own = std::fs::read_link("/proc/self/exe").expect("own path");
        assert!(!is_static_elf(&own) || cfg!(target_feature = "crt-static"));
        assert!(!is_static_elf(Path::new("/etc/hostname")));
        assert!(!is_static_elf(Path::new("/definitely/missing")));
    }
}
