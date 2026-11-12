#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! FanRGB: AI fan control and RGB lighting for Gigabyte (ITE Super I/O) motherboards.

// The engine's catch_unwind and the panic hook that hands fans back to the BIOS need unwinding.
#[cfg(panic = "abort")]
compile_error!("FanRGB must be built with panic = \"unwind\" (see Cargo.toml)");

mod config;
mod control;
mod diag;
mod drivers;
mod engine;
mod error;
mod hw;
mod icon_art;
mod model;
mod notify;
mod rgb;
mod system;
mod ui;
mod updater;
mod util;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use hw::HwBackend;

#[derive(Default, Debug, PartialEq, Eq)]
struct Args {
    demo: bool,
    minimized: bool,
    no_elevate: bool,
    diag: bool,
    verbose: bool,
    exit: bool,
    install: bool,
    uninstall: bool,
    /// `--revert`: go back to `fanrgb.exe.old` in the install folder (elevated).
    revert: bool,
    /// `--reset-update-state`: clear the updater's downgrade / replay record (elevated).
    reset_update_state: bool,
    /// `--yes`: no confirmation for `--revert` / `--reset-update-state`.
    yes: bool,
    help: bool,
    /// `--restart-after <pid>`: wait for that (previous) instance to exit first (restart).
    restart_after: Option<u32>,
    /// `--replace`: stop the running copy of FanRGB first, then start (the user agreed in the launcher).
    replace: bool,
    /// `--update-fetch`: the unelevated update helper (network + token), see `updater::fetch`.
    update_fetch: bool,
    /// `--catalog-fetch`: the unelevated helper that downloads only the signed vendor driver catalog.
    catalog_fetch: bool,
    /// `--update-finish <old pid>`: the elevated update supervisor (runs from `fanrgb.exe.old`).
    update_finish: Option<u32>,
    watchdog: Option<system::WatchdogArgs>,
    /// `--driver-check [--out <file> --nonce <n>] [--auto] [--offline]`: the unelevated Windows Update
    /// helper (no hardware access, no engine).
    driver_check: Option<drivers::wu::HelperArgs>,
}

const USAGE: &str = "usage: fanrgb [--minimized] [--demo] [--no-elevate] [--verbose] \
                     | --diag | --exit [--demo] | --install | --uninstall | --revert [--yes] \
                     | --reset-update-state [--yes] | --driver-check [--offline] | --help";

/// Parses the command line. Unknown arguments are ignored (older autostart tasks may pass some);
/// malformed values of known options are errors.
fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Args, String> {
    let mut a = Args::default();
    let mut parent_handle = None;
    let mut state_dir = None;
    let mut helper = drivers::wu::HelperArgs::default();
    let mut helper_opts = false;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--demo" => a.demo = true,
            "--minimized" => a.minimized = true,
            "--no-elevate" => a.no_elevate = true,
            "--replace" => a.replace = true,
            "--diag" => a.diag = true,
            "--verbose" => a.verbose = true,
            "--exit" => a.exit = true,
            "--install" => a.install = true,
            "--driver-check" => a.driver_check = Some(drivers::wu::HelperArgs::default()),
            "--out" => {
                helper.out = Some(PathBuf::from(it.next().ok_or("--out needs a file")?));
                helper_opts = true;
            }
            "--nonce" => {
                let ok = |n: &String| !n.is_empty() && n.len() <= 64 && n.bytes().all(|b| b.is_ascii_alphanumeric());
                helper.nonce = it.next().filter(ok).ok_or("--nonce needs a short alphanumeric value")?;
                helper_opts = true;
            }
            "--auto" => {
                helper.auto = true;
                helper_opts = true;
            }
            "--offline" => {
                helper.offline = true;
                helper_opts = true;
            }
            "--uninstall" => a.uninstall = true,
            "--revert" => a.revert = true,
            "--reset-update-state" => a.reset_update_state = true,
            "--yes" => a.yes = true,
            "--watchdog" => {
                let pid = it.next().and_then(|p| p.parse::<u32>().ok()).filter(|&p| p != 0);
                let pid = pid.ok_or("--watchdog needs a process id")?;
                a.watchdog = Some(system::WatchdogArgs { pid, ..Default::default() });
            }
            "--parent-handle" => {
                parent_handle =
                    Some(it.next().and_then(|h| h.parse::<usize>().ok()).ok_or("--parent-handle needs a number")?);
            }
            "--state-dir" => state_dir = Some(PathBuf::from(it.next().ok_or("--state-dir needs a folder")?)),
            system::RESTART_AFTER_ARG => {
                let pid = it.next().and_then(|p| p.parse::<u32>().ok()).filter(|&p| p != 0);
                a.restart_after = Some(pid.ok_or("--restart-after needs a process id")?);
            }
            updater::FETCH_ARG => a.update_fetch = true,
            updater::catalog_fetch::FETCH_ARG => a.catalog_fetch = true,
            updater::install::FINISH_ARG => {
                let pid = it.next().and_then(|p| p.parse::<u32>().ok()).filter(|&p| p != 0);
                a.update_finish = Some(pid.ok_or("--update-finish needs a process id")?);
            }
            "--help" | "-h" | "/?" => a.help = true,
            _ => {}
        }
    }
    match &mut a.watchdog {
        Some(w) => {
            w.parent_handle = parent_handle;
            w.state_dir = state_dir;
        }
        None if parent_handle.is_some() || state_dir.is_some() => {
            return Err("--parent-handle/--state-dir are only valid with --watchdog".into());
        }
        None => {}
    }
    match &mut a.driver_check {
        Some(d) => *d = helper,
        None if helper_opts => return Err("--out/--nonce/--auto/--offline are only valid with --driver-check".into()),
        None => {}
    }
    let modes = [
        a.diag,
        a.exit,
        a.install,
        a.uninstall,
        a.revert,
        a.reset_update_state,
        a.watchdog.is_some(),
        a.update_fetch,
        a.catalog_fetch,
        a.update_finish.is_some(),
        a.driver_check.is_some(),
    ]
    .iter()
    .filter(|&&m| m)
    .count();
    if modes > 1 {
        // The internal modes (started by FanRGB itself) are named too: the error must explain any mix.
        return Err(format!(
            "choose one of --diag, --exit, --install, --uninstall, --revert, --reset-update-state, --driver-check \
             (internal: --watchdog, --update-fetch, --catalog-fetch, --update-finish)\n{USAGE}"
        ));
    }
    if modes > 0 && a.restart_after.is_some() {
        return Err(format!("--restart-after only starts the app\n{USAGE}"));
    }
    if a.yes && !(a.revert || a.reset_update_state) {
        return Err(format!("--yes only confirms --revert or --reset-update-state\n{USAGE}"));
    }
    Ok(a)
}

fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // SAFETY: plain call; failing (no parent console) is harmless.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// `--diag`: read-only hardware report (no fan writes).
fn run_diag() {
    attach_console();
    util::logger::init(true, true);
    let cfg = config::Config::load();
    let mut hw = hw::RealHw::new();
    let _ = hw.poll();
    std::thread::sleep(Duration::from_millis(1100));
    let values = hw.poll();
    let info = hw.info().clone();
    let mut s = diag::report_header("--diag (separate read-only process; the running app keeps its own state)", &info);
    s.push_str(&diag::hardware_section(&info));
    s.push_str(&diag::config_section(&cfg));
    let sensors: Vec<_> = hw.sensors().iter().cloned().zip(values.iter().copied()).collect();
    s.push_str(&diag::sensors_section(&sensors));
    let value = |id: &str| sensors.iter().find(|(d, _)| d.id == id).and_then(|(_, v)| *v);
    let profile = cfg.profile();
    let rows: Vec<diag::ChannelRow> = hw
        .fans()
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let assigned = cfg.header_role(&f.id);
            let role = config::effective_role(assigned, f, f.tach_detected);
            let fc = profile.fan(&f.id, role);
            let duty = match (value(&f.id.replace("pwm", "duty")), hw.estimated_duty(i)) {
                (Some(d), _) => format!("{d:.0} %"),
                (None, Some(e)) => format!("est. {e:.0} % (BIOS curve)"),
                (None, None) => "-".into(),
            };
            diag::ChannelRow {
                id: f.id.clone(),
                header: f.default_name.clone(),
                role: diag::role_label(role, assigned),
                connected: diag::connected_label(f, false),
                mode: format!("profile: {}", fc.mode.label()),
                source: if fc.mode == config::FanMode::Curve { fc.curve.source.key() } else { "-".into() },
                duty,
                failsafe: "-".into(),
            }
        })
        .collect();
    s.push_str(&diag::channels_section(&rows));
    let cooling: Vec<diag::CoolingHeader> = hw
        .fans()
        .iter()
        .map(|f| {
            let assigned = cfg.header_role(&f.id);
            diag::CoolingHeader {
                header: f.default_name.clone(),
                role: config::effective_role(assigned, f, f.tach_detected),
                assigned,
                present: f.present,
                speed_signal: f.tach_detected,
            }
        })
        .collect();
    s.push_str(&diag::cooling_section(&cooling, None));
    s.push_str(&format!("\nRGB USB health: {:?}\n", diag::rgb_usb_health()));
    hidapi::HidApi::disable_device_discovery();
    let hid = hidapi::HidApi::new();
    match hid {
        Ok(mut api) => {
            for pid in rgb::fusion2::PIDS {
                let _ = api.add_devices(rgb::fusion2::VID_ITE, pid);
            }
            match rgb::fusion2::Fusion2::open(&api) {
                Ok(Some(d)) => {
                    let order =
                        |c: u32| rgb::fusion2::order_from_calibration(c).map(rgb::order_name).unwrap_or("unset".into());
                    s.push_str(&format!(
                        "RGB controller: {:04X} {} fw {}; cal D_LED1 {:#010x} ({}), D_LED2 {:#010x} ({}), rgb_cali {:#010x}\n",
                        d.pid,
                        d.info.name,
                        d.info.fw,
                        d.info.cal_strip0,
                        order(d.info.cal_strip0),
                        d.info.cal_strip1,
                        order(d.info.cal_strip1),
                        d.info.rgb_cali
                    ));
                }
                Ok(None) => s.push_str("RGB controller: not found\n"),
                Err(e) => s.push_str(&format!("RGB controller error: {e:#}\n")),
            }
        }
        Err(e) => s.push_str(&format!("HID unavailable: {e}\n")),
    }
    s.push_str(
        "RGB 0x48 Dynamic Lighting byte: not queried by --diag (the query writes [CC 48 00]); the running app's report has it\n",
    );
    // Settings only: `--diag` does not drive the controller. Dynamic Lighting is read from the
    // registry (read-only).
    let mut dl = rgb::dynlight::DlCore::new(Box::new(rgb::dynlight::RegistryBackend::default()));
    dl.refresh(std::time::Instant::now());
    let st = rgb::RgbStatus { dynamic_lighting: dl.status().clone(), ..Default::default() };
    s.push_str(&diag::rgb_section(&cfg, &st));
    s.push_str(&diag::dynamic_lighting_section());
    s.push_str(&diag::drivers_section(&drivers::current_state()));
    s.push_str(&diag::conflicts_section());
    s.push_str("\nSuper I/O:\n");
    s.push_str(&hw.diagnostics_dump());
    s.push_str(&diag::log_section(50));
    // The report is meant to be attached to issues: no account name, no machine SID.
    let s = diag::scrub_personal(&s);
    let path = util::paths::diag_file();
    // The closing line is part of what gets attached (TESTING.md redirects this output to a file);
    // without elevation the path is in the user's profile.
    let tail = match util::fsx::write_atomic(&path, s.as_bytes()) {
        Ok(()) => format!("Report written to {}", path.display()),
        Err(e) => format!("[{}] could not write {}: {e}", error::ErrorCode::Io, path.display()),
    };
    println!("{s}\n{}", diag::scrub_personal(&tail));
}

/// Runs `fanrgb <arg>` elevated (UAC prompt), waits, and returns its exit code.
fn relay_elevated(arg: &str) -> i32 {
    match util::win::run_elevated_and_wait(arg) {
        Ok(code) => code as i32,
        Err(util::win::ElevateResult::Cancelled) => {
            eprintln!("Cancelled: administrator rights are required.");
            1
        }
        Err(e) => {
            eprintln!("Could not start elevated: {e:?}");
            1
        }
    }
}

/// `--install`: copy to %ProgramFiles%\FanRGB and start with Windows from there.
fn run_install() -> i32 {
    attach_console();
    if !util::win::is_elevated() {
        let code = relay_elevated("--install");
        println!("{}", if code == 0 { "FanRGB installed and set to start with Windows." } else { "Install failed." });
        return code;
    }
    util::logger::init(false, true);
    match system::install_and_enable_autostart() {
        Ok(p) => {
            println!("FanRGB installed to {} and set to start with Windows.", p.display());
            0
        }
        Err(e) => {
            eprintln!("Install failed: {e:#}");
            1
        }
    }
}

/// `--uninstall`: stop FanRGB, remove the autostart task and the Program Files copy.
fn run_uninstall() -> i32 {
    attach_console();
    if !util::win::is_elevated() {
        let code = relay_elevated("--uninstall");
        println!("{}", if code == 0 { "FanRGB uninstalled." } else { "Uninstall failed." });
        return code;
    }
    util::logger::init(false, true);
    if system::request_instance_exit(Duration::from_secs(15)) == system::ExitOutcome::TimedOut {
        eprintln!("FanRGB did not stop in time; not uninstalling.");
        return 1;
    }
    match system::uninstall() {
        Ok(r) => {
            println!(
                "Autostart task {}; program files {}. Settings and logs were kept.",
                if r.task_removed { "removed" } else { "not present" },
                if r.removal_pending_reboot {
                    "will be removed at the next restart"
                } else if r.exe_removed {
                    "removed"
                } else {
                    "not present"
                }
            );
            0
        }
        Err(e) => {
            eprintln!("Uninstall failed: {e:#}");
            1
        }
    }
}

/// Asks before a change made by hand (`--revert`, `--reset-update-state`); `--yes` skips this. The
/// question is printed to the console and asked in a Yes/No dialog (default No): a release build is a
/// windowed program, so the console it attaches to has already returned to its prompt, and an answer
/// typed there would go to the shell, not to FanRGB.
fn confirm(question: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_DEFBUTTON2, MB_ICONWARNING};
    println!("{question}");
    let yes = ask_yes_no(question, MB_ICONWARNING | MB_DEFBUTTON2);
    if !yes {
        println!("Cancelled; nothing was changed.");
    }
    yes
}

/// A topmost Yes/No dialog without an owner window; `style` adds the icon and default button.
fn ask_yes_no(question: &str, style: windows_sys::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{IDYES, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW};
    let (text, title) = (util::win::wide(question), util::win::wide("FanRGB"));
    // SAFETY: both strings are NUL-terminated UTF-16 buffers alive for the call; no owner window.
    let answer = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_SETFOREGROUND | MB_TOPMOST | style,
        )
    };
    answer == IDYES
}

/// A second launch found a running FanRGB. If that is a different copy (an older build, another
/// folder), asks whether this copy should replace it; showing the old window instead would look as if
/// this copy had started. False for the same copy (then its window comes to the front, as before).
fn ask_replace_other_copy() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONQUESTION;
    let Some(other) = system::other_copy_path() else { return false };
    let this = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "?".into());
    ask_yes_no(
        &format!(
            "Another copy of FanRGB is already running:
{}

Close it and start this copy instead?
{this}

             Closing it hands the fans back to the BIOS until this copy takes over.              No brings the running copy to the front.",
            other.display()
        ),
        MB_ICONQUESTION,
    )
}

/// `--replace`: stops the running instance (its shutdown hands the fans back to the BIOS) so this copy
/// can take over. False if it is still running.
fn stop_running_instance() -> bool {
    match system::request_instance_exit(Duration::from_secs(20)) {
        system::ExitOutcome::Stopped | system::ExitOutcome::NotRunning => {
            log::info!("stopped the running FanRGB; this copy takes over");
            true
        }
        outcome => {
            log::warn!("the running FanRGB did not stop ({outcome:?}); showing it instead");
            false
        }
    }
}

/// Exit code of `--revert` / `--reset-update-state` when the user said no (nothing changed).
const DECLINED: i32 = 3;

/// `--revert`: go back to the previous version the updater kept (`fanrgb.exe.old`), elevated. The
/// unelevated part shows what will happen (the install folder is readable) and asks first.
fn run_revert(yes: bool) -> i32 {
    attach_console();
    let elevated = util::win::is_elevated();
    if elevated {
        util::logger::init(false, true);
    }
    let plan = match updater::install::revert_plan(&util::paths::install_dir()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Nothing to go back to: {e:#}");
            return 1;
        }
    };
    let current = plan.current.map_or_else(|| "the installed version".to_string(), |v| format!("FanRGB {v}"));
    if !yes
        && !confirm(&format!(
            "Go back from {current} to FanRGB {}?\n\nFanRGB stops first (every fan returns to the BIOS), the previous \
             version is put back in Program Files and started again. {current} will not be offered as an update again.",
            plan.previous
        ))
    {
        return DECLINED;
    }
    if !elevated {
        let code = relay_elevated("--revert --yes");
        println!(
            "{}",
            if code == 0 {
                format!("FanRGB {} is back in place.", plan.previous)
            } else {
                format!(
                    "Going back failed; details in {}.",
                    util::paths::program_data().join("FanRGB").join("fanrgb.log").display()
                )
            }
        );
        return code;
    }
    match updater::install::run_revert() {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            log::error!("--revert failed: {e:#}");
            eprintln!("Going back failed: {e:#}");
            1
        }
    }
}

/// `--reset-update-state`: clear the updater's downgrade and replay record (`update_state.json`),
/// elevated, after a reinstall from a known-good build (docs/RELEASING.md).
fn run_reset_update_state(yes: bool) -> i32 {
    attach_console();
    if !yes
        && !confirm(
            "Reset FanRGB's update record?\n\nFanRGB forgets the newest version it installed and the newest release \
             date it saw, so an older release could be offered again. Only do this after reinstalling from a \
             known-good build (for example after a bad or wrongly signed release). The old record is kept as a \
             .bak file.",
        )
    {
        return DECLINED;
    }
    if !util::win::is_elevated() {
        let code = relay_elevated("--reset-update-state --yes");
        println!("{}", if code == 0 { "Update record reset." } else { "Resetting the update record failed." });
        return code;
    }
    util::logger::init(false, true);
    match updater::install::reset_update_state() {
        Ok(Some(backup)) => {
            println!("Update record reset (the old one is {}).", backup.display());
            0
        }
        Ok(None) => {
            println!("There is no update record; nothing to reset.");
            0
        }
        Err(e) => {
            log::error!("--reset-update-state failed: {e:#}");
            eprintln!("Resetting the update record failed: {e:#}");
            1
        }
    }
}

/// `--exit`: stop a running instance cleanly (fans go back to the BIOS). `--exit --demo` stops only a
/// demo instance; a plain `--exit` also stops a demo when no real instance is running.
fn run_exit(demo: bool) -> i32 {
    attach_console();
    let timeout = Duration::from_secs(15);
    let real = if demo { system::ExitOutcome::NotRunning } else { system::request_instance_exit(timeout) };
    match real {
        system::ExitOutcome::NotRunning => match system::request_demo_exit(timeout) {
            system::ExitOutcome::Stopped => {
                println!("FanRGB demo stopped.");
                0
            }
            system::ExitOutcome::TimedOut => {
                println!("FanRGB demo did not stop in time.");
                1
            }
            _ => {
                println!("FanRGB is not running.");
                0
            }
        },
        system::ExitOutcome::Stopped => {
            println!("FanRGB stopped (fans returned to BIOS).");
            0
        }
        system::ExitOutcome::TimedOut => {
            println!("FanRGB did not stop in time.");
            1
        }
        system::ExitOutcome::NeedsElevation => {
            let code = relay_elevated("--exit");
            println!(
                "{}",
                if code == 0 { "FanRGB stopped (fans returned to BIOS)." } else { "FanRGB was not stopped." }
            );
            code
        }
    }
}

fn main() {
    // Before anything loads a DLL by name or touches a file: System32-only DLL search, Redirection
    // Guard, image-load policy (no file access). Then, before the arguments are even parsed, lock our
    // own executable so it cannot be swapped while (or after) we decide what to run.
    util::sec::harden_process();
    system::pin_own_exe();

    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            attach_console();
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if args.help {
        attach_console();
        println!("{USAGE}");
        std::process::exit(0);
    }
    // The Windows Update helper: started unelevated by the app (or by hand from a console). It returns
    // before anything below loads hardware backends, the engine, the logger or the settings.
    if let Some(d) = &args.driver_check {
        if d.out.is_none() {
            attach_console();
        }
        std::process::exit(drivers::wu::helper_main(d));
    }
    if let Some(w) = &args.watchdog {
        if let Some(dir) = &w.state_dir
            && let Err(e) = util::paths::use_state_dir(dir, w.pid)
        {
            // Never act on a snapshot from a folder that isn't admin-only.
            eprintln!("watchdog: {e}");
            return;
        }
        util::logger::init(false, false);
        system::install_panic_hook();
        system::run_watchdog(w);
        return;
    }
    if args.update_fetch {
        // Unelevated helper: no logger (nothing token-related is ever written to a log); the result
        // goes to %LOCALAPPDATA%\FanRGB\update\status.json.
        std::process::exit(updater::fetch::run_helper());
    }
    if args.catalog_fetch {
        // Same rules as --update-fetch: unelevated, no logger; the result goes to
        // %LOCALAPPDATA%\FanRGB\update\catalog\status.json.
        std::process::exit(updater::catalog_fetch::run_helper());
    }
    if let Some(old_pid) = args.update_finish {
        if !util::win::is_elevated() {
            eprintln!("--update-finish needs administrator rights");
            std::process::exit(2);
        }
        util::logger::init(false, false);
        system::install_panic_hook();
        std::process::exit(updater::install::run_finisher(old_pid));
    }
    if args.install {
        std::process::exit(run_install());
    }
    if args.uninstall {
        std::process::exit(run_uninstall());
    }
    if args.revert {
        std::process::exit(run_revert(args.yes));
    }
    if args.reset_update_state {
        std::process::exit(run_reset_update_state(args.yes));
    }
    if args.diag {
        run_diag();
        return;
    }
    if args.exit {
        std::process::exit(run_exit(args.demo));
    }
    // A restart: let the previous instance finish its shutdown (fans back to BIOS) and release the
    // single-instance objects before we look for a running instance.
    if let Some(pid) = args.restart_after {
        system::wait_for_previous_instance(pid, Duration::from_secs(30));
    }

    let elevated = util::win::is_elevated();
    // Let an already running instance come to the front instead of starting twice, unless it is a
    // different copy the user wants to replace. (A demo runs next to the real app with its own instance
    // objects.)
    let mut replace = args.replace;
    if !elevated && !args.demo && !replace {
        let probe = system::probe_instance();
        if matches!(probe, system::InstanceProbe::Elevated | system::InstanceProbe::User) {
            if !ask_replace_other_copy() {
                system::signal_show(probe);
                return;
            }
            replace = true;
        }
    }

    if !elevated && !args.demo && !args.no_elevate {
        let mut relaunch = Vec::new();
        if replace {
            relaunch.push("--replace");
        }
        if args.minimized {
            relaunch.push("--minimized");
        }
        if args.verbose {
            relaunch.push("--verbose");
        }
        match util::win::relaunch_elevated(&relaunch.join(" ")) {
            util::win::ElevateResult::Launched => return,
            util::win::ElevateResult::Cancelled => {}
            util::win::ElevateResult::Failed(e) => eprintln!("elevation failed: {e}"),
        }
    }

    util::logger::init(args.verbose, cfg!(debug_assertions));
    system::install_panic_hook();
    log::info!(
        "FanRGB {} starting (elevated: {elevated}, demo: {}, args: {:?}, state: {:?} {})",
        env!("CARGO_PKG_VERSION"),
        args.demo,
        args,
        util::paths::state_location(),
        util::paths::data_dir().display()
    );
    for p in util::sec::hardening_problems() {
        log::warn!("[{}] {p}", error::ErrorCode::Hardening);
    }
    // What a restart keeps.
    let mut keep = Vec::new();
    let kept = [(args.demo, "--demo"), (args.no_elevate && !elevated, "--no-elevate"), (args.verbose, "--verbose")];
    for (on, flag) in kept {
        if on {
            keep.push(flag.to_string());
        }
    }
    system::set_restart_args(keep);
    updater::init(args.demo);

    // Started elevated by hand (an admin console) next to a different running copy: ask here.
    if elevated && !args.demo && !replace {
        let probe = system::probe_instance();
        replace =
            matches!(probe, system::InstanceProbe::Elevated | system::InstanceProbe::User) && ask_replace_other_copy();
    }
    if replace && !args.demo && !stop_running_instance() {
        system::signal_show(system::probe_instance());
        return;
    }

    let instance = if args.demo { system::acquire_demo_instance() } else { system::acquire_instance(elevated) };
    let instance = match instance {
        Ok(i) => i,
        Err(e) => {
            log::info!("not starting: {e:#}");
            if elevated && !args.demo && error::code_of(&e) == error::ErrorCode::AlreadyRunning {
                // Another FanRGB already runs: not a failed update if this is a new version on probation.
                updater::install::mark_clean_exit();
            }
            return;
        }
    };

    if args.demo {
        // Demo mode never touches the real settings, learned models or crash snapshot (and never loads
        // PawnIO or starts the watchdog: the backend is simulated).
        config::redirect_storage(util::paths::data_dir().join("demo"));
    } else if elevated {
        system::recover_at_startup();
        std::thread::Builder::new().name("fanrgb-autostart-check".into()).spawn(system::harden_autostart).ok();
    }
    // (The engine also shows these in the UI.)
    for issue in system::startup_issues() {
        log::warn!("start-up problem: {issue}");
    }

    let config = config::Config::load();
    // Driver inventory (offline, read-only) and the opt-in daily Windows Update check (unelevated helper).
    drivers::start(config.drivers.auto_check);
    let start_minimized = args.minimized || config.general.start_minimized;
    let shared = engine::Shared::new(config);
    system::set_panic_shared(shared.clone());

    let backend: Box<dyn HwBackend> =
        // Demo: CPU_FAN reports no tach, like the reference PC.
        if args.demo { Box::new(hw::sim::SimHw::new(1.0).without_tach(0)) } else { Box::new(hw::RealHw::new()) };
    let controls_fans = !args.demo && backend.info().fan_control && !backend.info().simulated;
    // The crash watchdog is mandatory whenever fans can be under our control (the old
    // `general.watchdog` setting is ignored).
    if controls_fans && let Err(e) = system::spawn_watchdog() {
        log::error!("{e:#}");
    }
    let engine_thread = engine::spawn(shared.clone(), backend);
    if elevated && !args.demo {
        // After a self-update: tell the supervisor once the engine runs steadily.
        updater::install::start_probation(shared.clone());
    }

    let (rgb_tx, rgb_rx) = std::sync::mpsc::channel();
    shared.set_rgb_sender(rgb_tx);
    // Demo: a simulated RGB controller, never the user's hardware.
    let rgb_thread = rgb::spawn_with(shared.clone(), rgb_rx, args.demo);

    system::spawn_session_watcher(shared.clone());
    system::spawn_show_listener(instance.show_events, shared.clone());
    if let Some(ev) = instance.exit_event {
        system::spawn_exit_listener(ev, shared.clone());
    }

    // A UI panic unwinds to here (dropping every lock the UI held), so the engine can finish its
    // shutdown and hand the fans back to the BIOS before the process ends (review safety #13).
    let ui_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ui::run(shared.clone(), start_minimized)));
    let panicked = match ui_result {
        Ok(Ok(())) => {
            if elevated && !args.demo {
                // Closed by the user: a new version on probation has not failed.
                updater::install::mark_clean_exit();
            }
            false
        }
        Ok(Err(e)) => {
            updater::install::EXIT_FAILED.store(true, Ordering::SeqCst);
            log::error!("UI error: {e}");
            false
        }
        Err(_) => {
            updater::install::EXIT_FAILED.store(true, Ordering::SeqCst);
            log::error!("[{}] the UI panicked; stopping the engine", error::ErrorCode::Internal);
            shared.send(engine::Command::EmergencyRelease);
            true
        }
    };

    log::info!("shutting down");
    shared.quit.store(true, Ordering::SeqCst);
    shared.send(engine::Command::Wake);
    let _ = engine_thread.join();
    let _ = rgb_thread.join();
    log::info!("bye");
    log::logger().flush();
    if panicked {
        std::process::exit(101);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_normal_flags() {
        let a = p(&["--minimized", "--verbose", "--something-unknown"]).unwrap();
        assert!(a.minimized && a.verbose && !a.demo && a.watchdog.is_none());
        assert_eq!(p(&[]).unwrap(), Args::default());
        assert!(p(&["--install"]).unwrap().install);
        assert!(p(&["--uninstall"]).unwrap().uninstall);
        assert!(p(&["--exit"]).unwrap().exit);
        let r = p(&["--replace", "--minimized"]).unwrap();
        assert!(r.replace && r.minimized);
    }

    #[test]
    fn parses_watchdog() {
        let a = p(&["--watchdog", "1234", "--parent-handle", "716"]).unwrap();
        assert_eq!(a.watchdog, Some(system::WatchdogArgs { pid: 1234, parent_handle: Some(716), state_dir: None }));
        let a = p(&["--watchdog", "9", "--state-dir", r"C:\ProgramData\FanRGB.9-0"]).unwrap();
        assert_eq!(a.watchdog.unwrap().state_dir, Some(PathBuf::from(r"C:\ProgramData\FanRGB.9-0")));
    }

    #[test]
    fn rejects_malformed_or_conflicting() {
        assert!(p(&["--watchdog"]).is_err());
        assert!(p(&["--watchdog", "abc"]).is_err());
        assert!(p(&["--watchdog", "0"]).is_err());
        assert!(p(&["--watchdog", "1", "--parent-handle", "x"]).is_err());
        assert!(p(&["--parent-handle", "5"]).is_err());
        assert!(p(&["--state-dir", r"C:\x"]).is_err());
        assert!(p(&["--install", "--uninstall"]).is_err());
        assert!(p(&["--diag", "--exit"]).is_err());
        // The error names the internal modes too.
        let e = p(&["--update-fetch", "--update-finish", "5"]).unwrap_err();
        assert!(e.contains("--update-fetch") && e.contains("--update-finish"), "{e}");
        assert!(p(&["--watchdog", "5", "--install"]).is_err());
        assert!(p(&["--restart-after"]).is_err());
        assert!(p(&["--restart-after", "0"]).is_err());
        assert!(p(&["--restart-after", "x"]).is_err());
        assert!(p(&["--restart-after", "5", "--install"]).is_err());
        assert!(p(&["--restart-after", "5", "--watchdog", "6"]).is_err());
    }

    #[test]
    fn parses_update_modes() {
        assert!(p(&["--update-fetch"]).unwrap().update_fetch);
        assert_eq!(p(&["--update-finish", "77"]).unwrap().update_finish, Some(77));
        assert!(p(&["--update-finish"]).is_err());
        assert!(p(&["--update-finish", "0"]).is_err());
        assert!(p(&["--update-fetch", "--install"]).is_err());
        assert!(p(&["--update-finish", "5", "--update-fetch"]).is_err());
        assert!(p(&["--update-fetch", "--restart-after", "5"]).is_err());
        assert!(!p(&[]).unwrap().update_fetch);
        // The catalog-only helper is its own exclusive internal mode.
        assert!(p(&["--catalog-fetch"]).unwrap().catalog_fetch);
        let e = p(&["--catalog-fetch", "--update-fetch"]).unwrap_err();
        assert!(e.contains("--catalog-fetch"), "{e}");
        assert!(p(&["--catalog-fetch", "--driver-check"]).is_err());
        assert!(p(&["--catalog-fetch", "--restart-after", "5"]).is_err());
        assert!(!p(&[]).unwrap().catalog_fetch);
    }

    #[test]
    fn parses_revert_and_reset() {
        assert!(p(&["--revert"]).unwrap().revert);
        let a = p(&["--revert", "--yes"]).unwrap();
        assert!(a.revert && a.yes && !a.reset_update_state);
        let a = p(&["--reset-update-state", "--yes"]).unwrap();
        assert!(a.reset_update_state && a.yes);
        assert!(!p(&["--reset-update-state"]).unwrap().yes, "asks unless --yes");
        // Exclusive modes; --yes confirms only these two.
        let e = p(&["--revert", "--reset-update-state"]).unwrap_err();
        assert!(e.contains("--revert") && e.contains("--reset-update-state"), "{e}");
        assert!(p(&["--revert", "--install"]).is_err());
        assert!(p(&["--revert", "--restart-after", "5"]).is_err());
        assert!(p(&["--yes"]).is_err());
        assert!(p(&["--uninstall", "--yes"]).is_err());
        assert_eq!(p(&[]).unwrap(), Args::default());
    }

    #[test]
    fn help_is_not_an_error() {
        // `--help` prints the usage and exits 0 (main), it is not a parse error (exit 2).
        for h in ["--help", "-h", "/?"] {
            assert!(p(&[h]).unwrap().help, "{h}");
        }
        assert!(!p(&[]).unwrap().help);
    }

    #[test]
    fn parses_restart() {
        let a = p(&["--restart-after", "4321", "--demo", "--verbose"]).unwrap();
        assert_eq!(a.restart_after, Some(4321));
        assert!(a.demo && a.verbose);
        assert_eq!(p(&[]).unwrap().restart_after, None);
    }

    #[test]
    fn duplicate_unknown_and_odd_arguments() {
        // Duplicates of a flag are harmless; a mode given twice is still one mode.
        let a = p(&["--demo", "--demo", "--minimized", "--minimized"]).unwrap();
        assert!(a.demo && a.minimized);
        assert!(p(&["--exit", "--exit"]).unwrap().exit);
        assert!(p(&["--diag", "--diag", "--verbose"]).unwrap().diag);
        // Unknown arguments (older autostart tasks) are ignored, including case variants and values.
        assert_eq!(p(&["--DEMO", "-demo", "/demo", "demo", "--poll=500", ""]).unwrap(), Args::default());
        // A repeated --watchdog keeps the last pid.
        assert_eq!(p(&["--watchdog", "5", "--watchdog", "6"]).unwrap().watchdog.unwrap().pid, 6);
        assert!(p(&["--watchdog", "-5"]).is_err());
        assert!(p(&["--watchdog", "4294967296"]).is_err(), "pid overflow");
        assert!(p(&["--watchdog", "7", "--state-dir"]).is_err(), "missing value");
        // A flag value that looks like another flag is consumed as the value.
        assert!(p(&["--watchdog", "--demo"]).is_err());
        // Normal-run flags combine with a mode (they are simply unused there).
        assert!(p(&["--diag", "--demo", "--no-elevate"]).unwrap().diag);
        // Help is a successful parse (main prints the usage and exits 0), also next to other flags.
        assert!(p(&["-h", "--demo"]).unwrap().help && p(&["/?"]).unwrap().help);
    }

    #[test]
    fn parses_driver_check() {
        let a = p(&["--driver-check"]).unwrap();
        assert_eq!(a.driver_check, Some(drivers::wu::HelperArgs::default()));
        let out = r"C:\Users\u\AppData\Local\FanRGB\d.json";
        let a = p(&["--driver-check", "--out", out, "--nonce", "ab12", "--auto"]).unwrap();
        let d = a.driver_check.unwrap();
        assert_eq!(d.out, Some(PathBuf::from(out)));
        assert_eq!(d.nonce, "ab12");
        assert!(d.auto && !d.offline);
        assert!(p(&["--driver-check", "--offline"]).unwrap().driver_check.unwrap().offline);
        // Helper options need the mode; the mode excludes the others; values are checked.
        assert!(p(&["--out", out]).is_err());
        assert!(p(&["--auto"]).is_err());
        assert!(p(&["--driver-check", "--diag"]).is_err());
        assert!(p(&["--driver-check", "--restart-after", "5"]).is_err());
        assert!(p(&["--driver-check", "--nonce", "a b"]).is_err());
        assert!(p(&["--driver-check", "--nonce"]).is_err());
        assert!(p(&["--driver-check", "--out"]).is_err());
    }
}
