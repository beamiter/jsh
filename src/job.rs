/// Job control: process groups, fg/bg, job table, async notifications.
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{tcsetpgrp, Pid};
use std::fmt;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Running,
    Stopped,
    Done(i32),
}

impl fmt::Display for JobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JobStatus::Running => write!(f, "Running"),
            JobStatus::Stopped => write!(f, "Stopped"),
            JobStatus::Done(code) => write!(f, "Done({})", code),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: usize,
    pub pid: Pid,
    pub command: String,
    pub status: JobStatus,
    pub start_time: Instant,
}

pub struct JobTable {
    pub jobs: Vec<Job>,
    next_id: usize,
}

impl Default for JobTable {
    fn default() -> Self {
        Self::new()
    }
}

impl JobTable {
    pub fn new() -> Self {
        JobTable {
            jobs: Vec::new(),
            next_id: 1,
        }
    }

    pub fn add(&mut self, pid: Pid, command: String) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.jobs.push(Job {
            id,
            pid,
            command,
            status: JobStatus::Running,
            start_time: Instant::now(),
        });
        id
    }

    pub fn get_by_id(&mut self, id: usize) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    pub fn get_last_stopped(&mut self) -> Option<&mut Job> {
        self.jobs
            .iter_mut()
            .rev()
            .find(|j| j.status == JobStatus::Stopped)
    }

    pub fn get_last(&mut self) -> Option<&mut Job> {
        self.jobs
            .iter_mut()
            .rev()
            .find(|j| j.status == JobStatus::Running || j.status == JobStatus::Stopped)
    }

    pub fn remove_done(&mut self) {
        self.jobs
            .retain(|j| !matches!(j.status, JobStatus::Done(_)));
    }

    pub fn notify_done(&mut self) {
        self.notify_done_with_notification(Duration::from_secs(u64::MAX));
    }

    pub fn notify_done_with_notification(&mut self, threshold: Duration) {
        for job in &self.jobs {
            if let JobStatus::Done(code) = job.status {
                let elapsed = job.start_time.elapsed();
                let dur = format_job_duration(elapsed);
                if code == 0 {
                    eprintln!("[{}]+  Done  ({})  {}", job.id, dur, job.command);
                } else {
                    eprintln!(
                        "[{}]+  Failed({})  ({})  {}",
                        job.id, code, dur, job.command
                    );
                }
                if elapsed > threshold {
                    send_notification(&job.command, code, elapsed);
                }
            }
        }
        self.remove_done();
    }

    pub fn check_background(&mut self) {
        for job in &mut self.jobs {
            if job.status == JobStatus::Running {
                match waitpid(job.pid, Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED)) {
                    Ok(WaitStatus::Exited(_, code)) => {
                        job.status = JobStatus::Done(code);
                    }
                    Ok(WaitStatus::Signaled(_, _, _)) => {
                        job.status = JobStatus::Done(128);
                    }
                    Ok(WaitStatus::Stopped(_, _)) => {
                        job.status = JobStatus::Stopped;
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn print_jobs(&self) {
        for job in &self.jobs {
            let elapsed = job.start_time.elapsed();
            println!(
                "[{}]+  {}  ({:.1}s)  {}",
                job.id,
                job.status,
                elapsed.as_secs_f64(),
                job.command
            );
        }
    }

    pub fn wait_fg(&mut self, pid: Pid) -> i32 {
        loop {
            match waitpid(pid, Some(WaitPidFlag::WUNTRACED)) {
                Ok(WaitStatus::Exited(_, code)) => return code,
                Ok(WaitStatus::Signaled(_, sig, _)) => return 128 + sig as i32,
                Ok(WaitStatus::Stopped(_, _)) => {
                    if let Some(job) = self.jobs.iter_mut().find(|j| j.pid == pid) {
                        job.status = JobStatus::Stopped;
                        eprintln!(
                            "\n[{}]+  Stopped                    {}",
                            job.id, job.command
                        );
                    }
                    return 148;
                }
                Err(_) => return 1,
                _ => continue,
            }
        }
    }

    pub fn continue_fg(&mut self, id: usize) -> i32 {
        if let Some(job) = self.get_by_id(id) {
            let pid = job.pid;
            job.status = JobStatus::Running;
            eprintln!("{}", job.command);
            let shell_pgid = nix::unistd::getpgrp();
            tcsetpgrp(std::io::stdin(), pid).ok();
            kill(pid, Signal::SIGCONT).ok();
            let code = self.wait_fg(pid);
            tcsetpgrp(std::io::stdin(), shell_pgid).ok();
            code
        } else {
            eprintln!("jsh: fg: {}: no such job", id);
            1
        }
    }

    pub fn continue_bg(&mut self, id: usize) -> i32 {
        if let Some(job) = self.get_by_id(id) {
            job.status = JobStatus::Running;
            eprintln!("[{}]+ {} &", job.id, job.command);
            kill(job.pid, Signal::SIGCONT).ok();
            0
        } else {
            eprintln!("jsh: bg: {}: no such job", id);
            1
        }
    }
}

fn send_notification(command: &str, exit_code: i32, elapsed: Duration) {
    let (summary, body) = notification_text(command, exit_code, elapsed);
    dispatch_notification(
        &summary,
        &body,
        crate::osc::notify_osc777,
        |summary, body| {
            std::process::Command::new("notify-send")
                .args([summary, body])
                .spawn()
                .ok();
        },
    );
}

fn notification_text(command: &str, exit_code: i32, elapsed: Duration) -> (String, String) {
    let dur = format_job_duration(elapsed);
    let summary = if exit_code == 0 {
        "Command completed".to_string()
    } else {
        format!("Command failed (exit {})", exit_code)
    };
    (summary, format!("{} ({})", command, dur))
}

/// Route one finished job to exactly ONE notification channel.
///
/// This used to fire three: OSC 777, then OSC 9, then a spawned notify-send.
/// The comments justified the two OSC forms by assuming they reach disjoint
/// terminals, which is false here — jterm_core's shared parser turns both into
/// the same `ParserEvent::Notification`, so jterm1/jterm4 raised two popups, and
/// notify-send made a third on any desktop where the terminal had already
/// notified.
///
/// Policy: prefer the in-band OSC channel whenever the mark sink is a terminal.
/// The terminal knows whether its window is focused and can suppress, route or
/// coalesce the popup, and the notification survives ssh with no D-Bus in
/// between. `notify-send` is the fallback for exactly one case: there is no
/// terminal listening (stderr redirected, jsh in a pipeline), so nobody else
/// will tell the user. OSC 777 is the primary in-band form because it carries
/// summary and body as separate fields; OSC 9 is deliberately not emitted (see
/// `osc::notify_osc9`).
///
/// The sinks are parameters so tests can count deliveries per channel.
fn dispatch_notification<InBand, Desktop>(
    summary: &str,
    body: &str,
    emit_in_band: InBand,
    notify_desktop: Desktop,
) where
    InBand: FnOnce(&str, &str) -> bool,
    Desktop: FnOnce(&str, &str),
{
    if !emit_in_band(summary, body) {
        notify_desktop(summary, body);
    }
}

fn format_job_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h{}m{}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m{:.0}s", secs / 60, secs % 60)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records what each channel was told, so "one event, one notification" is
    /// an assertion rather than a hope.
    #[derive(Default)]
    struct Deliveries {
        in_band: RefCell<Vec<(String, String)>>,
        desktop: RefCell<Vec<(String, String)>>,
    }

    fn deliver(summary: &str, body: &str, terminal_listening: bool) -> Deliveries {
        let log = Deliveries::default();
        dispatch_notification(
            summary,
            body,
            |summary, body| {
                log.in_band
                    .borrow_mut()
                    .push((summary.to_string(), body.to_string()));
                terminal_listening
            },
            |summary, body| {
                log.desktop
                    .borrow_mut()
                    .push((summary.to_string(), body.to_string()));
            },
        );
        log
    }

    #[test]
    fn a_finished_job_notifies_once_in_band_when_a_terminal_is_listening() {
        // Regression: this fired OSC 777 + OSC 9 + notify-send, and jterm_core
        // parses both OSC forms, so one job produced three notifications.
        let log = deliver("Command completed", "sleep 30 (30.0s)", true);

        assert_eq!(
            log.in_band.borrow().as_slice(),
            [(
                "Command completed".to_string(),
                "sleep 30 (30.0s)".to_string()
            )]
        );
        assert!(log.desktop.borrow().is_empty(), "notify-send would be #2");
    }

    #[test]
    fn notify_send_is_the_fallback_only_when_no_terminal_took_it() {
        let log = deliver("Command completed", "sleep 30 (30.0s)", false);

        assert_eq!(log.in_band.borrow().len(), 1);
        assert_eq!(
            log.desktop.borrow().as_slice(),
            [(
                "Command completed".to_string(),
                "sleep 30 (30.0s)".to_string()
            )]
        );
    }

    #[test]
    fn notification_text_reports_command_duration_and_failure() {
        assert_eq!(
            notification_text("cargo build", 0, Duration::from_secs(90)),
            (
                "Command completed".to_string(),
                "cargo build (1m30s)".to_string()
            )
        );
        assert_eq!(
            notification_text("cargo build", 101, Duration::from_millis(1500)),
            (
                "Command failed (exit 101)".to_string(),
                "cargo build (1.5s)".to_string()
            )
        );
    }
}
