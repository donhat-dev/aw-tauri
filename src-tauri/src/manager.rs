//! A process manager for ActivityWatch
//!
//! Used to start, stop and manage the lifecycle modules like aw-watcher-afk and aw-watcher-window.
//! A module is a process that runs in the background and sends events to the ActivityWatch server.
//!
//! The manager is responsible for starting and stopping the modules, and for keeping track of
//! their state.
//!
//! If a module crashes, the manager will notify the user and ask if they want to restart it.

#[cfg(unix)]
use {
    nix::sys::signal::{self, Signal},
    nix::unistd::{close, pipe, read, Pid},
    std::os::unix::fs::PermissionsExt,
    std::os::unix::io::IntoRawFd,
};
#[cfg(windows)]
use {
    std::os::windows::process::CommandExt,
    std::ptr::null_mut,
    winapi::shared::minwindef::{DWORD, FALSE},
    winapi::um::handleapi::CloseHandle,
    winapi::um::jobapi2::{AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject},
    winapi::um::processthreadsapi::{OpenProcess, TerminateProcess},
    winapi::um::winbase::CREATE_NO_WINDOW,
    winapi::um::winnt::{
        JobObjectExtendedLimitInformation, HANDLE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, PROCESS_TERMINATE,
    },
};

use lazy_static::lazy_static;
use log::{debug, error, info, trace};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::process::{ChildStdin, Command};
use std::sync::{
    mpsc::{channel, Receiver, Sender},
    Arc, Mutex,
};
use std::time::Duration;
use std::{env, fs, thread};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, SubmenuBuilder};
use tauri::{webview::WebviewWindowBuilder, Manager, WebviewUrl};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use crate::{get_app_handle, get_config, get_tray_id, HANDLE_CONDVAR};
use tauri_plugin_notification::NotificationExt;

lazy_static! {
    static ref ODOO_SYNC_STDIN: Mutex<Option<ChildStdin>> = Mutex::new(None);
}

#[derive(Debug)]
enum ModuleMessage {
    Started {
        name: String,
        pid: u32,
        args: Option<Vec<String>>,
    },
    Stopped {
        name: String,
        output: std::process::Output,
    },
    Init {},
}

#[derive(Debug)]
pub struct ManagerState {
    tx: Sender<ModuleMessage>,
    pub modules_running: BTreeMap<String, bool>,
    pub modules_discovered: BTreeMap<String, PathBuf>,
    pub modules_pid: HashMap<String, u32>,
    pub modules_restart_count: HashMap<String, u32>,
    pub modules_pending_shutdown: HashMap<String, bool>,
    pub modules_args: HashMap<String, Option<Vec<String>>>,
    pub modules_menu_set: bool,
}

impl ManagerState {
    fn new(tx: Sender<ModuleMessage>) -> ManagerState {
        ManagerState {
            tx,
            //TODO: merge some of these maps into a single struct
            modules_running: BTreeMap::new(),
            modules_discovered: discover_modules(),
            modules_pid: HashMap::new(),
            modules_restart_count: HashMap::new(),
            modules_pending_shutdown: HashMap::new(),
            modules_args: HashMap::new(),
            modules_menu_set: false,
        }
    }
    fn started_module(&mut self, name: &str, pid: u32, args: Option<Vec<String>>) {
        info!("Started module: {name}");
        self.modules_running.insert(name.to_string(), true);
        self.modules_pid.insert(name.to_string(), pid);
        self.modules_args.insert(name.to_string(), args);
        self.modules_pending_shutdown.remove(name);
        debug!("Running modules: {:?}", self.modules_running);
    }
    fn stopped_module(&mut self, name: &str) {
        info!("Stopped module: {name}");
        self.modules_running.insert(name.to_string(), false);
        self.modules_pid.remove(name);
    }

    pub fn start_module(&self, name: &str, args: Option<&Vec<String>>) {
        if !self.is_module_running(name) {
            if let Some(path) = self.modules_discovered.get(name) {
                start_module_thread(
                    name.to_string(),
                    path.clone(),
                    args.cloned(),
                    self.tx.clone(),
                );
            } else {
                error!("Module {name} not found in PATH");
            }
        }
    }
    pub fn stop_module(&mut self, name: &str) {
        if let Some(pid) = self.modules_pid.get(name) {
            // add to pending shutdown to prevent restart
            self.modules_pending_shutdown.insert(name.to_string(), true);
            if let Err(e) = send_sigterm(*pid) {
                error!("Failed to send SIGTERM to module {name}: {e}");
            } else {
                debug!("Sent SIGTERM to module: {name}");
            }
        }
    }
    pub fn stop_modules(&mut self) {
        let module_names: Vec<String> = self.modules_pid.keys().cloned().collect();
        for name in module_names {
            self.stop_module(&name);
        }
    }
    pub fn handle_system_click(&mut self, name: &str) {
        if self.is_module_running(name) {
            self.stop_module(name);
        } else {
            self.start_module(name, None);
        }
    }
    fn is_module_running(&self, name: &str) -> bool {
        *self.modules_running.get(name).unwrap_or(&false)
    }
}

fn update_tray_menu(
    modules_running: &BTreeMap<String, bool>,
    modules_discovered: &BTreeMap<String, PathBuf>,
) {
    let (lock, cvar) = &*HANDLE_CONDVAR;
    let mut state = lock.lock().expect("Failed to acquire manager_state lock");

    debug!("Attempting to get app handle");
    while !*state {
        state = cvar
            .wait(state)
            .expect("Failed to wait on condition variable");
    }
    debug!("Condition variable set");
    let app = &*get_app_handle().lock().expect("Failed to get app handle");
    debug!("App handle acquired");

    let open = MenuItem::with_id(app, "open", "Open Dashboard", true, None::<&str>)
        .expect("failed to create open menu item");
    let quit = MenuItem::with_id(app, "quit", "Quit ActivityWatch", true, None::<&str>)
        .expect("failed to create quit menu item");

    let mut modules_submenu_builder = SubmenuBuilder::new(app, "Modules");
    for (module, running) in modules_running.iter() {
        let label = module;
        let module_menu = CheckMenuItem::with_id(app, module, label, true, *running, None::<&str>)
            .expect("Failed to create module menu item");
        modules_submenu_builder = modules_submenu_builder.item(&module_menu);
    }

    for module_name in modules_discovered.keys() {
        if !modules_running.contains_key(module_name) {
            let module_menu = MenuItem::with_id(app, module_name, module_name, true, None::<&str>)
                .expect("Failed to create module menu item");
            modules_submenu_builder = modules_submenu_builder.item(&module_menu);
        }
    }

    let module_submenu = modules_submenu_builder
        .build()
        .expect("Failed to create module submenu");
    let config_folder = MenuItem::with_id(
        app,
        "config_folder",
        "Open config folder",
        true,
        None::<&str>,
    )
    .expect("Failed to create config folder menu item");

    let log_folder = MenuItem::with_id(app, "log_folder", "Open log folder", true, None::<&str>)
        .expect("Failed to create log folder menu item");
    let separator = PredefinedMenuItem::separator(app).expect("Failed to create separator");
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &separator,
            &module_submenu,
            &separator,
            &config_folder,
            &log_folder,
            &separator,
            &quit,
        ],
    )
    .expect("Failed to create tray menu");

    let tray_id = get_tray_id();
    app.tray_by_id(tray_id)
        .expect("Failed to get tray by id")
        .set_menu(Some(menu))
        .expect("Failed to set tray menu");
    trace!("set tray menu");
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> Result<(), nix::Error> {
    let pid = Pid::from_raw(pid as i32);
    let res = signal::kill(pid, Signal::SIGTERM);
    if let Err(e) = res {
        Err(e)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn send_sigkill(pid: u32) -> Result<(), nix::Error> {
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL)
}

#[cfg(unix)]
#[derive(Debug)]
struct RunningProcess {
    pid: u32,
    ppid: u32,
    command: String,
}

#[cfg(unix)]
fn split_first_field(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    let split_at = input.find(|c: char| c.is_whitespace())?;
    Some((&input[..split_at], &input[split_at..]))
}

#[cfg(unix)]
fn list_running_processes() -> Vec<RunningProcess> {
    let output = match Command::new("ps")
        .args(["axww", "-o", "pid=,ppid=,command="])
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            error!("Failed to list running processes: {e}");
            return Vec::new();
        }
    };

    if !output.status.success() {
        error!("ps exited with status {}", output.status);
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (pid_str, rest) = split_first_field(line)?;
            let (ppid_str, command) = split_first_field(rest)?;
            Some(RunningProcess {
                pid: pid_str.parse().ok()?,
                ppid: ppid_str.parse().ok()?,
                command: command.trim_start().to_string(),
            })
        })
        .collect()
}

#[cfg(unix)]
fn command_starts_with_path(command: &str, path: &str) -> bool {
    command == path
        || command
            .strip_prefix(path)
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c.is_whitespace())
}

#[cfg(unix)]
fn process_command_matches_path(command: &str, path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    if command_starts_with_path(command, path_str.as_ref()) {
        return true;
    }

    if let Ok(canonical_path) = fs::canonicalize(path) {
        let canonical_path = canonical_path.to_string_lossy();
        return command_starts_with_path(command, canonical_path.as_ref());
    }

    false
}

#[cfg(unix)]
fn find_orphaned_module_processes(modules: &BTreeMap<String, PathBuf>) -> Vec<(String, u32)> {
    let current_pid = std::process::id();
    let mut matches = Vec::new();

    for process in list_running_processes() {
        if process.pid == current_pid || process.ppid != 1 {
            continue;
        }

        for (name, path) in modules {
            if process_command_matches_path(&process.command, path) {
                matches.push((name.clone(), process.pid));
                break;
            }
        }
    }

    matches.sort();
    matches.dedup();
    matches
}

#[cfg(unix)]
fn terminate_orphaned_module_processes(modules: &BTreeMap<String, PathBuf>) {
    let orphaned = find_orphaned_module_processes(modules);
    if orphaned.is_empty() {
        return;
    }

    for (name, pid) in &orphaned {
        info!("Terminating orphaned module process before startup: {name} pid={pid}");
        if let Err(e) = send_sigterm(*pid) {
            error!("Failed to send SIGTERM to orphaned module {name} pid={pid}: {e}");
        }
    }

    for _ in 0..20 {
        thread::sleep(Duration::from_millis(100));
        if find_orphaned_module_processes(modules).is_empty() {
            return;
        }
    }

    for (name, pid) in find_orphaned_module_processes(modules) {
        error!("Orphaned module {name} pid={pid} did not exit after SIGTERM; sending SIGKILL");
        if let Err(e) = send_sigkill(pid) {
            error!("Failed to send SIGKILL to orphaned module {name} pid={pid}: {e}");
        }
    }
}

#[cfg(windows)]
fn send_sigterm(pid: u32) -> Result<(), std::io::Error> {
    let pid = pid as DWORD;

    // Open the process with terminate permission
    let process_handle = unsafe { OpenProcess(PROCESS_TERMINATE, FALSE, pid) };

    if process_handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }

    // Terminate the process with exit code 1
    let result = unsafe { TerminateProcess(process_handle, 1) };

    // Close the process handle
    unsafe { CloseHandle(process_handle) };

    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn create_job_object() -> Result<HANDLE, std::io::Error> {
    unsafe {
        // Create a new job object
        let job_handle = CreateJobObjectW(null_mut(), null_mut());
        if job_handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        // Set job object to kill all associated processes when it's closed
        let mut job_info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        job_info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        let result = SetInformationJobObject(
            job_handle,
            JobObjectExtendedLimitInformation,
            &mut job_info as *mut _ as *mut _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as DWORD,
        );

        if result == 0 {
            CloseHandle(job_handle);
            return Err(std::io::Error::last_os_error());
        }

        Ok(job_handle)
    }
}

#[cfg(unix)]
fn monitor_parent_process(child_pid: u32, read_fd: i32) {
    thread::spawn(move || {
        // Read from the pipe - when parent dies, the write end is closed by the OS
        // and we'll get EOF (read returns 0)
        let mut buf = [0u8; 1];
        loop {
            match read(read_fd, &mut buf) {
                Ok(0) => {
                    // EOF means parent died (write end of pipe closed)
                    info!(
                        "Parent process died (pipe closed), terminating child {}",
                        child_pid
                    );

                    // Close our read end of the pipe
                    let _ = close(read_fd);

                    // Send SIGTERM to the child process
                    if let Err(e) = send_sigterm(child_pid) {
                        error!("Failed to terminate child process {}: {}", child_pid, e);
                    } else {
                        debug!("Successfully sent SIGTERM to child process {}", child_pid);
                    }
                    break;
                }
                Ok(_) => {
                    // Should never receive data, but if we do, just continue monitoring
                    // This handles spurious wake-ups gracefully
                }
                Err(e) => {
                    // Error reading from pipe - parent likely died
                    error!("Error reading from parent monitor pipe: {}", e);
                    let _ = close(read_fd);

                    if let Err(e) = send_sigterm(child_pid) {
                        error!("Failed to terminate child process {}: {}", child_pid, e);
                    } else {
                        debug!("Successfully sent SIGTERM to child process {}", child_pid);
                    }
                    break;
                }
            }
        }
    });
}

pub fn start_manager() -> Arc<Mutex<ManagerState>> {
    let (tx, rx) = channel();
    let state = Arc::new(Mutex::new(ManagerState::new(tx.clone())));

    #[cfg(unix)]
    {
        let modules = state
            .lock()
            .expect("Failed to acquire manager_state lock")
            .modules_discovered
            .clone();
        terminate_orphaned_module_processes(&modules);
    }

    // Start the modules
    let config = get_config();
    for module_entry in config.autostart.modules.iter() {
        let name = module_entry.name();
        let args_str = module_entry.args();

        let args = if args_str.is_empty() {
            None
        } else {
            // Split args string on whitespace, preserving quoted arguments
            Some(shell_words::split(args_str).unwrap_or_default())
        };
        state
            .lock()
            .expect("Failed to acquire manager_state lock")
            .start_module(name, args.as_ref());
    }

    // populate the tray menu if not yet already done
    let modules_menu_set = state
        .lock()
        .expect("Failed to acquire manager_state lock")
        .modules_menu_set;
    if !modules_menu_set {
        tx.send(ModuleMessage::Init {})
            .expect("Failed to send \"Module Init\" message");
    }

    let state_clone = Arc::clone(&state);
    thread::spawn(move || {
        handle(rx, state_clone);
    });
    state
}

fn handle(rx: Receiver<ModuleMessage>, state: Arc<Mutex<ManagerState>>) {
    loop {
        let msg = rx.recv().expect("Failed to receive Module message");
        let state_clone = Arc::clone(&state);

        let (modules_running, modules_discovered) = {
            let mut state_guard = state.lock().expect("Failed to acquire manager_state lock");
            match msg {
                ModuleMessage::Started { name, pid, args } => {
                    state_guard.started_module(&name, pid, args);
                    (
                        state_guard.modules_running.clone(),
                        state_guard.modules_discovered.clone(),
                    )
                }
                ModuleMessage::Stopped { name, output } => {
                    state_guard.stopped_module(&name);
                    let data = (
                        state_guard.modules_running.clone(),
                        state_guard.modules_discovered.clone(),
                    );
                    let name_clone = name.clone();
                    if output.status.success() {
                        info!("Module {name} exited successfully");
                    } else {
                        error!("Module {name} exited with error status");
                        thread::spawn(move || {
                            let (should_restart, restart_info) = {
                                let state_guard = &mut state_clone
                                    .lock()
                                    .expect("Failed to acquire manager_state lock");
                                let restart_count = state_guard
                                    .modules_restart_count
                                    .get(&name_clone)
                                    .unwrap_or(&0);

                                let pending_shutdown = state_guard
                                    .modules_pending_shutdown
                                    .get(&name_clone)
                                    .unwrap_or(&false);

                                // If shutdown is pending, exit early
                                if *pending_shutdown {
                                    return; // Exit the entire thread
                                }

                                if *restart_count < 3 {
                                    // Exponential backoff: 2^(restart_count + 1) seconds
                                    // restart_count 0 -> 2 seconds, 1 -> 4 seconds, 2 -> 8 seconds
                                    let delay_secs = 2u64.pow(*restart_count + 1);
                                    info!(
                                        "Module {name_clone} will restart in {delay_secs} seconds (attempt {} of 3)",
                                        *restart_count + 1
                                    );
                                    (true, Some((delay_secs, *restart_count)))
                                } else {
                                    (false, None)
                                }
                            };

                            if should_restart {
                                if let Some((secs, restart_count)) = restart_info {
                                    {
                                        // Show dialog BEFORE sleeping
                                        let app = &*get_app_handle()
                                            .lock()
                                            .expect("Failed to get app handle");
                                        app.dialog()
                                            .message(format!("{name_clone} crashed. Restarting..."))
                                            .kind(MessageDialogKind::Warning)
                                            .title("Warning")
                                            .show(|_| {});
                                    }
                                    error!("Module {name_clone} crashed and will be restarted");

                                    thread::sleep(Duration::from_secs(secs));

                                    let state_guard = &mut state_clone
                                        .lock()
                                        .expect("Failed to acquire manager_state lock");

                                    state_guard
                                        .modules_restart_count
                                        .insert(name_clone.clone(), restart_count + 1);
                                    // Get the stored arguments for this module
                                    let stored_args = state_guard
                                        .modules_args
                                        .get(&name_clone)
                                        .cloned()
                                        .flatten();
                                    state_guard.start_module(&name_clone, stored_args.as_ref());
                                }
                            } else {
                                // Restart limit reached
                                let state_guard = &mut state_clone
                                    .lock()
                                    .expect("Failed to acquire manager_state lock");
                                state_guard
                                    .modules_pending_shutdown
                                    .insert(name_clone.clone(), true);

                                let app =
                                    &*get_app_handle().lock().expect("Failed to get app handle");
                                app.dialog()
                                    .message(format!(
                                        "{name_clone} keeps on crashing. Restart limit reached."
                                    ))
                                    .kind(MessageDialogKind::Warning)
                                    .title("Warning")
                                    .show(|_| {});
                                error!("Module {name_clone} exceeded crash restart limit");
                            }
                        });

                        let stdout = String::from_utf8_lossy(&output.stdout);
                        if !stdout.trim().is_empty() {
                            info!("Module {name} stdout: {}", stdout);
                        }
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        if !stderr.trim().is_empty() {
                            error!("Module {name} stderr: {}", stderr);
                        }
                    }
                    data
                }
                ModuleMessage::Init {} => (
                    state_guard.modules_running.clone(),
                    state_guard.modules_discovered.clone(),
                ),
            }
        };
        update_tray_menu(&modules_running, &modules_discovered);
    }
}

fn start_module_thread(
    name: String,
    path: PathBuf,
    custom_args: Option<Vec<String>>,
    tx: Sender<ModuleMessage>,
) {
    // Special handling for aw-notify module
    if name == "aw-notify" {
        info!("Using special aw-notify handler for module: {name}");
        start_notify_module_thread(name, path, custom_args, tx);
        return;
    }
    if name == "aw-odoo-sync" {
        info!("Using special aw-odoo-sync handler for module: {name}");
        start_odoo_sync_module_thread(name, path, custom_args, tx);
        return;
    }

    start_generic_module_thread(name, path, custom_args, tx);
}

fn start_generic_module_thread(
    name: String,
    path: PathBuf,
    custom_args: Option<Vec<String>>,
    tx: Sender<ModuleMessage>,
) {
    thread::spawn(move || {
        // Create job object on Windows to ensure child dies with parent
        #[cfg(windows)]
        let job_handle = match create_job_object() {
            Ok(handle) => Some(handle),
            Err(e) => {
                error!("Failed to create job object for {name}: {e}");
                None
            }
        };

        // Create pipe for Unix parent death detection
        #[cfg(unix)]
        let (pipe_read_fd, _pipe_write_keeper) = match pipe() {
            Ok((read_fd, write_fd)) => {
                // read_fd is read end, write_fd stays open in parent and auto-closes when parent dies
                (read_fd.into_raw_fd(), Some(std::fs::File::from(write_fd)))
            }
            Err(e) => {
                error!("Failed to create pipe for parent monitoring: {}", e);
                (-1, None)
            }
        };

        // Start the child process
        let mut command = Command::new(&path);
        apply_module_environment(&mut command, &name);

        // Use custom args if provided, otherwise only pass port arg if it's not the default (5600)
        if let Some(ref args) = custom_args {
            command.args(args);
        } else if get_config().port != 5600 {
            command.args(["--port", get_config().port.to_string().as_str()]);
        }

        // Set creation flags on Windows to hide console window
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let child = command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        let child = match child {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to start module {name}: {e}");
                #[cfg(windows)]
                if let Some(handle) = job_handle {
                    unsafe {
                        CloseHandle(handle);
                    }
                }
                #[cfg(unix)]
                if pipe_read_fd >= 0 {
                    let _ = close(pipe_read_fd);
                }
                return;
            }
        };

        let child_pid = child.id();

        // On Windows, assign child to job object
        #[cfg(windows)]
        if let Some(handle) = job_handle {
            use std::os::windows::io::AsRawHandle;
            let child_handle = child.as_raw_handle() as HANDLE;
            unsafe {
                if AssignProcessToJobObject(handle, child_handle) == 0 {
                    error!(
                        "Failed to assign child process to job object: {:?}",
                        std::io::Error::last_os_error()
                    );
                }
            }
        }

        // On Unix, start parent process monitor with pipe
        #[cfg(unix)]
        if pipe_read_fd >= 0 {
            monitor_parent_process(child_pid, pipe_read_fd);
        }

        // Send a message to the manager that the module has started
        tx.send(ModuleMessage::Started {
            name: name.to_string(),
            pid: child_pid,
            args: custom_args,
        })
        .expect("Failed to send Module Started message");

        // Wait for the child to exit
        let output = child
            .wait_with_output()
            .expect("Failed to wait on child process");

        // Clean up job handle on Windows
        #[cfg(windows)]
        if let Some(handle) = job_handle {
            unsafe {
                CloseHandle(handle);
            }
        }

        // Send the process output to the manager
        tx.send(ModuleMessage::Stopped {
            name: name.to_string(),
            output,
        })
        .expect("Failed to send module stopped message");
    });
}

fn start_odoo_sync_module_thread(
    name: String,
    path: PathBuf,
    custom_args: Option<Vec<String>>,
    tx: Sender<ModuleMessage>,
) {
    thread::spawn(move || {
        #[cfg(windows)]
        let job_handle = match create_job_object() {
            Ok(handle) => Some(handle),
            Err(e) => {
                error!("Failed to create job object for {name}: {e}");
                None
            }
        };

        #[cfg(unix)]
        let (pipe_read_fd, _pipe_write_keeper) = match pipe() {
            Ok((read_fd, write_fd)) => (read_fd.into_raw_fd(), Some(std::fs::File::from(write_fd))),
            Err(e) => {
                error!("Failed to create pipe for parent monitoring: {}", e);
                (-1, None)
            }
        };

        let mut command = Command::new(&path);
        apply_module_environment(&mut command, &name);

        if let Some(ref args) = custom_args {
            command.args(args);
        } else if get_config().port != 5600 {
            command.args(["--port", get_config().port.to_string().as_str()]);
        }

        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let mut child = match command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                error!("Failed to start module {name}: {e}");
                #[cfg(windows)]
                if let Some(handle) = job_handle {
                    unsafe {
                        CloseHandle(handle);
                    }
                }
                #[cfg(unix)]
                if pipe_read_fd >= 0 {
                    let _ = close(pipe_read_fd);
                }
                return;
            }
        };

        let child_pid = child.id();
        if let Ok(mut stdin_guard) = ODOO_SYNC_STDIN.lock() {
            *stdin_guard = child.stdin.take();
        } else {
            error!("Failed to store aw-odoo-sync stdin pipe");
        }

        #[cfg(windows)]
        if let Some(handle) = job_handle {
            use std::os::windows::io::AsRawHandle;
            let child_handle = child.as_raw_handle() as HANDLE;
            unsafe {
                if AssignProcessToJobObject(handle, child_handle) == 0 {
                    error!(
                        "Failed to assign child process to job object: {:?}",
                        std::io::Error::last_os_error()
                    );
                }
            }
        }

        #[cfg(unix)]
        if pipe_read_fd >= 0 {
            monitor_parent_process(child_pid, pipe_read_fd);
        }

        tx.send(ModuleMessage::Started {
            name: name.to_string(),
            pid: child_pid,
            args: custom_args.clone(),
        })
        .expect("Failed to send Module Started message");

        if let Some(stderr) = child.stderr.take() {
            let stderr_name = name.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(line_str) => {
                            if !line_str.trim().is_empty() {
                                info!("{stderr_name} stderr: {line_str}");
                            }
                        }
                        Err(e) => error!("Failed to read stderr from {stderr_name}: {e}"),
                    }
                }
            });
        }

        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line_str) => {
                        info!("aw-odoo-sync output: {}", line_str);
                        handle_odoo_sync_output_line(&line_str);
                    }
                    Err(e) => error!("Failed to read line from aw-odoo-sync: {}", e),
                }
            }
        }

        let status = child.wait().expect("Failed to wait on child process");
        if let Ok(mut stdin_guard) = ODOO_SYNC_STDIN.lock() {
            *stdin_guard = None;
        }

        #[cfg(windows)]
        if let Some(handle) = job_handle {
            unsafe {
                CloseHandle(handle);
            }
        }

        tx.send(ModuleMessage::Stopped {
            name: name.to_string(),
            output: std::process::Output {
                status,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        })
        .expect("Failed to send module stopped message");
    });
}

fn apply_module_environment(command: &mut Command, name: &str) {
    if let Ok(log_root) = crate::dirs::get_log_root_dir() {
        let module_log_dir = log_root.join(name);
        command.env("AW_LOG_ROOT", &log_root);
        command.env("AW_LOG_DIR", module_log_dir);
    }
}

fn start_notify_module_thread(
    name: String,
    path: PathBuf,
    custom_args: Option<Vec<String>>,
    tx: Sender<ModuleMessage>,
) {
    thread::spawn(move || {
        // Create job object on Windows to ensure child dies with parent
        #[cfg(windows)]
        let job_handle = match create_job_object() {
            Ok(handle) => Some(handle),
            Err(e) => {
                error!("Failed to create job object for {name}: {e}");
                None
            }
        };

        // Create pipe for Unix parent death detection
        // Create pipe for Unix parent death detection
        #[cfg(unix)]
        let (pipe_read_fd, _pipe_write_keeper) = match pipe() {
            Ok((read_fd, write_fd)) => {
                // read_fd is read end, write_fd stays open in parent and auto-closes when parent dies
                (read_fd.into_raw_fd(), Some(std::fs::File::from(write_fd)))
            }
            Err(e) => {
                error!("Failed to create pipe for parent monitoring: {}", e);
                (-1, None)
            }
        };

        // Start the child process with --output-only flag
        let mut command = Command::new(&path);
        apply_module_environment(&mut command, &name);

        // Always add --output-only flag for aw-notify
        let mut args = vec!["--output-only".to_string()];

        // Add port argument if not default (5600)
        if get_config().port != 5600 {
            args.push("--port".to_string());
            args.push(get_config().port.to_string());
        }

        // Add any custom args
        if let Some(ref custom) = custom_args {
            args.extend_from_slice(custom);
        }

        command.args(&args);

        // Set creation flags on Windows to hide console window
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let mut child = match command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                let error_msg = e.to_string();
                if error_msg.contains("No such option: --output-only") {
                    info!("aw-notify module doesn't support --output-only, falling back to default behavior");
                    // Clean up job handle before fallback
                    #[cfg(windows)]
                    if let Some(handle) = job_handle {
                        unsafe {
                            CloseHandle(handle);
                        }
                    }
                    #[cfg(unix)]
                    if pipe_read_fd >= 0 {
                        let _ = close(pipe_read_fd);
                    }
                    // Fallback to generic module handler to avoid recursion
                    start_generic_module_thread(name, path, custom_args, tx);
                    return;
                } else {
                    error!("Failed to start module {name}: {e}");
                    #[cfg(windows)]
                    if let Some(handle) = job_handle {
                        unsafe {
                            CloseHandle(handle);
                        }
                    }
                    #[cfg(unix)]
                    if pipe_read_fd >= 0 {
                        let _ = close(pipe_read_fd);
                    }
                    return;
                }
            }
        };

        let child_pid = child.id();

        // On Windows, assign child to job object
        #[cfg(windows)]
        if let Some(handle) = job_handle {
            use std::os::windows::io::AsRawHandle;
            let child_handle = child.as_raw_handle() as HANDLE;
            unsafe {
                if AssignProcessToJobObject(handle, child_handle) == 0 {
                    error!(
                        "Failed to assign child process to job object: {:?}",
                        std::io::Error::last_os_error()
                    );
                }
            }
        }

        // On Unix, start parent process monitor with pipe
        #[cfg(unix)]
        if pipe_read_fd >= 0 {
            monitor_parent_process(child_pid, pipe_read_fd);
        }

        // Send a message to the manager that the module has started
        tx.send(ModuleMessage::Started {
            name: name.to_string(),
            pid: child_pid,
            args: Some(args),
        })
        .expect("Failed to send module started message");

        let stdout = child.stdout.take().expect("Failed to get stdout");
        let reader = BufReader::new(stdout);

        for line in reader.lines() {
            match line {
                Ok(line_str) => {
                    info!("aw-notify output: {}", line_str);
                    if line_str.starts_with("{") {
                        if let Ok(notification) =
                            serde_json::from_str::<serde_json::Value>(&line_str)
                        {
                            info!("aw-notify notification: {}", notification);
                            if let (Some(title), Some(message)) = (
                                notification.get("title").and_then(|t| t.as_str()),
                                notification.get("message").and_then(|m| m.as_str()),
                            ) {
                                send_notification(title, message);
                                info!(
                                    "Parsed JSON notification: title='{}', message length={}",
                                    title,
                                    message.len()
                                );
                            } else {
                                debug!("JSON notification missing title or message fields");
                            }
                        } else {
                            debug!("Failed to parse JSON line: {}", line_str);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to read line from aw-notify: {}", e);
                }
            }
        }

        // Wait for the child to exit
        let output = child.wait_with_output().expect("Failed to wait on child");

        // Check if the process failed due to unsupported --output-only flag
        // Exit code 2 is commonly used by clap/click for argument errors
        if output.status.code() == Some(2) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("No such option: --output-only") {
                info!("aw-notify module doesn't support --output-only, falling back to default behavior");

                // Clean up job handle before fallback
                #[cfg(windows)]
                if let Some(handle) = job_handle {
                    unsafe {
                        CloseHandle(handle);
                    }
                }
                #[cfg(unix)]
                if pipe_read_fd >= 0 {
                    let _ = close(pipe_read_fd);
                }

                // Fallback to generic module handler
                start_generic_module_thread(name, path, custom_args, tx);
                return;
            }
        }

        // Clean up job handle on Windows
        #[cfg(windows)]
        if let Some(handle) = job_handle {
            unsafe {
                CloseHandle(handle);
            }
        }

        // Send the process output to the manager
        tx.send(ModuleMessage::Stopped {
            name: name.to_string(),
            output,
        })
        .expect("Failed to send module stopped message");
    });
}

fn send_notification(title: &str, message: &str) {
    // Get app handle and send notification
    if let Ok(app_handle_guard) = get_app_handle().lock() {
        let app_handle = &*app_handle_guard;
        let result = app_handle
            .notification()
            .builder()
            .title(title)
            .body(message)
            .show();

        match result {
            Ok(_) => {
                trace!(
                    "Sent notification: title='{}', message preview='{}'",
                    title,
                    message.lines().next().unwrap_or("")
                );
            }
            Err(e) => {
                error!("Failed to send notification: {}", e);
            }
        }
    } else {
        error!("Failed to get app handle lock for notification");
    }
}

pub fn send_odoo_sync_command(payload: serde_json::Value) -> Result<(), String> {
    if payload.get("kind").and_then(|value| value.as_str()) == Some("aw-tauri.idle-dialog.action") {
        info!(
            "Sending idle dialog action to aw-odoo-sync: timer_session_id={:?} action={:?} resume_timer={:?}",
            payload.get("timer_session_id"),
            payload.get("action"),
            payload.get("resume_timer")
        );
    }
    let mut stdin_guard = ODOO_SYNC_STDIN
        .lock()
        .map_err(|_| "Failed to lock aw-odoo-sync stdin".to_string())?;
    let Some(stdin) = stdin_guard.as_mut() else {
        return Err("aw-odoo-sync is not running or has no command pipe".to_string());
    };
    let line = serde_json::to_string(&payload)
        .map_err(|e| format!("Failed to serialize aw-odoo-sync command: {e}"))?;
    stdin
        .write_all(line.as_bytes())
        .and_then(|_| stdin.write_all(b"\n"))
        .and_then(|_| stdin.flush())
        .map_err(|e| format!("Failed to write aw-odoo-sync command: {e}"))
}

fn handle_odoo_sync_output_line(line: &str) {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return;
    }
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        debug!("Ignoring non-JSON aw-odoo-sync output: {}", trimmed);
        return;
    };
    match payload.get("kind").and_then(|value| value.as_str()) {
        Some("aw-odoo-sync.idle-dialog.show") | Some("aw-odoo-sync.idle-dialog.update") => {
            info!(
                "Received idle dialog payload from aw-odoo-sync: kind={:?} timer_session_id={:?} idle_seconds={:?} state={:?}",
                payload.get("kind"),
                payload.get("timer_session_id"),
                payload.get("idle_seconds"),
                payload.get("state")
            );
            show_idle_dialog(payload);
        }
        Some("aw-odoo-sync.idle-dialog.closed") => {
            info!("Received idle dialog close event from aw-odoo-sync");
            close_idle_dialog_window();
        }
        _ => {}
    }
}

fn show_idle_dialog(payload: serde_json::Value) {
    let Ok(app_handle_guard) = get_app_handle().lock() else {
        error!("Failed to get app handle lock for idle dialog");
        return;
    };
    let app_handle = &*app_handle_guard;

    if let Some(window) = app_handle.webview_windows().get("idle-dialog") {
        info!(
            "Updating existing idle dialog window timer_session_id={:?} idle_seconds={:?} state={:?}",
            payload.get("timer_session_id"),
            payload.get("idle_seconds"),
            payload.get("state")
        );
        update_idle_dialog_window(&window, &payload);
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }

    let payload_json = match serde_json::to_string(&payload) {
        Ok(value) => value,
        Err(e) => {
            error!("Failed to serialize idle dialog payload: {}", e);
            "{}".to_string()
        }
    };
    let init_script = format!("window.__AW_IDLE_DIALOG_PAYLOAD__ = {};", payload_json);

    let wb = WebviewWindowBuilder::new(
        app_handle,
        "idle-dialog",
        WebviewUrl::App("idle-dialog.html".into()),
    )
    .title("Idle time detected")
    .inner_size(672.0, 468.0)
    .resizable(false)
    .decorations(false)
    .shadow(false)
    .always_on_top(true)
    .center()
    .initialization_script(&init_script);

    // transparent() requires macos-private-api feature on macOS — skip to keep build portable
    #[cfg(not(target_os = "macos"))]
    let wb = wb.transparent(true);

    let window = wb.build();

    match window {
        Ok(window) => {
            info!(
                "Created idle dialog window timer_session_id={:?} idle_seconds={:?} state={:?}",
                payload.get("timer_session_id"),
                payload.get("idle_seconds"),
                payload.get("state")
            );
            let _ = window.set_focus();
        }
        Err(e) => error!("Failed to create idle dialog window: {}", e),
    }
}

fn update_idle_dialog_window(window: &tauri::WebviewWindow, payload: &serde_json::Value) {
    let payload_json = match serde_json::to_string(payload) {
        Ok(value) => value,
        Err(e) => {
            error!("Failed to serialize idle dialog update payload: {}", e);
            return;
        }
    };
    let script = format!(
        "if (window.__AW_IDLE_DIALOG_UPDATE__) window.__AW_IDLE_DIALOG_UPDATE__({});",
        payload_json
    );
    if let Err(e) = window.eval(&script) {
        debug!("Failed to update idle dialog window: {}", e);
    }
}

fn close_idle_dialog_window() {
    let Ok(app_handle_guard) = get_app_handle().lock() else {
        error!("Failed to get app handle lock for idle dialog close");
        return;
    };
    let app_handle = &*app_handle_guard;
    if let Some(window) = app_handle.webview_windows().get("idle-dialog") {
        if let Err(e) = window.close() {
            debug!("Failed to close idle dialog window: {}", e);
        } else {
            info!("Closed idle dialog window");
        }
    }
}

#[cfg(unix)]
fn discover_modules() -> BTreeMap<String, PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let excluded = [
        "aw-tauri",
        "aw-client",
        "aw-cli",
        "aw-qt",
        "aw-server",
        "aw-server-rust",
        "aw-watcher-window-macos",
    ];
    let config = crate::get_config();

    let path = env::var_os("PATH").unwrap_or_default();
    let mut paths = env::split_paths(&path).collect::<Vec<_>>();

    // check each path in discovery_paths and add it to the start of the paths list if it's not already there
    for path in config.discovery_paths.iter() {
        if !paths.contains(path) {
            paths.insert(0, path.to_owned());
        }
    }

    // Create new PATH-like string
    let new_paths = env::join_paths(paths).unwrap_or_default();

    // Build a set of paths to search
    let mut found_modules = BTreeMap::new();
    // Use (device, inode) pairs for cycle detection (works across filesystems)
    let mut visited_inodes = HashSet::new();

    // Create a stack of directories to search, starting with PATH entries
    let mut dirs_to_search: Vec<PathBuf> = env::split_paths(&new_paths).collect();

    // Process directories in depth-first order
    while let Some(dir) = dirs_to_search.pop() {
        // Use (device, inode) tuple to detect cycles (works across different filesystems)
        if let Ok(metadata) = fs::metadata(&dir) {
            let id = (metadata.dev(), metadata.ino());
            if !visited_inodes.insert(id) {
                continue; // Already visited this directory
            }
        } else {
            continue; // Can't access directory
        }

        // Look for aw-* executables in this directory
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();

                // Get metadata once and reuse (avoid duplicate fs::metadata call)
                let metadata = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                let file_name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(name) => name.to_string(),
                    None => continue,
                };

                // Process only items starting with "aw-"
                if !file_name.starts_with("aw-") {
                    continue;
                }

                // If it's a directory starting with "aw-", add to search stack
                if metadata.is_dir() {
                    dirs_to_search.push(path);
                }
                // If it's an executable file
                else if metadata.is_file() || metadata.file_type().is_symlink() {
                    // Skip if has extension or is excluded
                    if file_name.contains('.') || excluded.contains(&file_name.as_str()) {
                        continue;
                    }

                    // Check if executable
                    let is_executable = metadata.permissions().mode() & 0o111 != 0;
                    if is_executable {
                        found_modules.insert(file_name, path);
                    }
                }
            }
        }
    }

    debug!(
        "Discovered modules: {:?}",
        found_modules.keys().collect::<Vec<_>>()
    );
    found_modules
}

#[cfg(windows)]
fn discover_modules() -> BTreeMap<String, PathBuf> {
    let excluded = [
        "aw-tauri",
        "aw-client",
        "aw-cli",
        "aw-qt",
        "aw-server",
        "aw-server-rust",
    ];
    let config = crate::get_config();

    let path = env::var_os("PATH").unwrap_or_default();
    let mut paths = env::split_paths(&path).collect::<Vec<_>>();

    // Always prepend exe's own directory — NSIS installs watcher bundles alongside aw-tauri.exe
    if let Ok(exe_path) = env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let exe_dir = exe_dir.to_path_buf();
            if !paths.contains(&exe_dir) {
                paths.insert(0, exe_dir);
            }
        }
    }

    // check each path in discovery_paths and add it to the start of the paths list if it's not already there
    for path in config.discovery_paths.iter() {
        if !paths.contains(path) {
            paths.insert(0, path.to_owned());
        }
    }

    let new_paths = env::join_paths(paths).unwrap_or_default();

    // Build a set of paths to search
    let mut found_modules = BTreeMap::new();
    let mut visited_dirs = HashSet::new();

    // Create a stack of directories to search, starting with PATH entries
    let mut dirs_to_search: Vec<PathBuf> = env::split_paths(&new_paths).collect();

    // Process directories in depth-first order
    while let Some(dir) = dirs_to_search.pop() {
        // Skip if already visited
        if !visited_dirs.insert(dir.clone()) {
            continue;
        }

        // Look for aw-* executables in this directory
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();

                // Skip if not a file or directory
                if let Ok(metadata) = fs::metadata(&path) {
                    let file_name = match path.file_name().and_then(|n| n.to_str()) {
                        Some(name) => name.to_string(),
                        None => continue,
                    };

                    // Process only items starting with "aw-"
                    if !file_name.starts_with("aw-") {
                        continue;
                    }

                    // If it's a directory starting with "aw-", recurse unless it's a launcher app
                    if metadata.is_dir() {
                        if !excluded.contains(&file_name.as_str()) {
                            dirs_to_search.push(path);
                        }
                    }
                    // If it's an executable file
                    else if metadata.is_file() && file_name.ends_with(".exe") {
                        // Extract name without .exe suffix
                        let name = match file_name.strip_suffix(".exe") {
                            Some(name) => name.to_lowercase(),
                            None => continue,
                        };

                        // Skip if excluded
                        if excluded.contains(&name.as_str()) {
                            continue;
                        }

                        found_modules.insert(name, path);
                    }
                }
            }
        }
    }

    found_modules
}
