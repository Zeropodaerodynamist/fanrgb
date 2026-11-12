//! Device I/O worker for the RGB controller.
//!
//! hidapi's feature-report calls block forever on a stalled control transfer, so
//! all device I/O runs on a dedicated worker thread. The lighting thread only posts the desired
//! state ([`Link::post`], latest wins) and polls a watchdog ([`Link::poll`]): if a single HID
//! call takes longer than [`Timings::io_timeout`], the worker is abandoned (its thread stays
//! blocked inside the driver until Windows fails the request), the handle is given up and a
//! fresh worker reconnects with exponential backoff. Neither the UI nor the lighting preview
//! ever waits on the device.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;

use super::fusion2::{Fusion2, Fusion2Info};
use super::session::{Desired, Session, persist_plan};
use super::transport::{Connector, HidTransport};
use crate::config::ExitLighting;
use crate::error::{ErrorCode, code_of, err};

#[derive(Clone, Copy, Debug)]
pub struct Timings {
    /// Longest a single feature report may take before the device counts as hung.
    pub io_timeout: Duration,
    /// Longest enumeration / `CreateFile` may take.
    pub open_timeout: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// Re-read `[CC 60]` this often to detect a firmware that stopped answering.
    pub health_every: Duration,
    /// Upper bound for the worker's idle wait.
    pub idle_wake: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            io_timeout: Duration::from_secs(1),
            open_timeout: Duration::from_secs(3),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            health_every: Duration::from_secs(30),
            idle_wake: Duration::from_millis(250),
        }
    }
}

/// A connection must work this long before the reconnect backoff starts again from 1 s, so a
/// controller that passes init but fails its first writes is not re-initialised every second.
const HEALTHY_AFTER: Duration = Duration::from_secs(10);

/// Wait before reconnect attempt `attempt` (1-based): 1 s, 2 s, 4 s ... capped at 30 s.
pub fn backoff(attempt: u32, t: &Timings) -> Duration {
    let exp = attempt.saturating_sub(1).min(16);
    t.backoff_min.saturating_mul(1u32 << exp).min(t.backoff_max)
}

/// Connection state shown in the UI.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum RgbConnState {
    /// First search after start-up (or after "Reconnect").
    #[default]
    Searching,
    Connected,
    /// Lost or failed; the next attempt is `attempt` at `next_try`.
    Reconnecting {
        attempt: u32,
        next_try: Instant,
    },
    /// The controller stopped answering or Windows reports it failed (USB error 43). Retries
    /// continue in the background, but usually only a full power drain helps.
    Hung,
    /// Opening was refused (access denied / sharing violation): another app holds it.
    InUseByOtherApp,
    /// No controller enumerated.
    NotPresent,
}

impl RgbConnState {
    pub fn label(&self) -> &'static str {
        match self {
            RgbConnState::Searching => "searching",
            RgbConnState::Connected => "connected",
            RgbConnState::Reconnecting { .. } => "reconnecting",
            RgbConnState::Hung => "not responding",
            RgbConnState::InUseByOtherApp => "in use by another app",
            RgbConnState::NotPresent => "not detected",
        }
    }
}

/// The most recent RGB error.
#[derive(Clone, Debug, PartialEq)]
pub struct RgbErrorInfo {
    pub code: ErrorCode,
    pub detail: String,
    /// When it last happened.
    pub at: SystemTime,
    /// How many times in a row it happened.
    pub count: u32,
}

impl RgbErrorInfo {
    /// `[FRG-E405] RGB controller disconnected: <detail>`
    pub fn message(&self) -> String {
        if self.detail.is_empty() {
            format!("[{}] {}", self.code, self.code.title())
        } else {
            format!("[{}] {}: {}", self.code, self.code.title(), self.detail)
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DeviceSnapshot {
    pub pid: u16,
    pub info: Fusion2Info,
    /// Raw `[CC 48]` reply byte.
    pub dynamic_lighting_raw: Option<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct LinkStatus {
    pub state: RgbConnState,
    pub last_error: Option<RgbErrorInfo>,
    pub next_retry: Option<Instant>,
    /// Failed attempts since the last successful connection.
    pub attempts: u32,
    pub device: Option<DeviceSnapshot>,
    /// Smoothed time per feature report (ms); 0 = not measured yet.
    pub packet_ms: f32,
    pub packets_total: u64,
    pub write_errors: u64,
    /// Successful reconnections after a loss.
    pub reconnects: u32,
    pub saved_to_flash: bool,
    pub suspended: bool,
    /// Worker threads still blocked inside a hung HID call.
    pub hung_threads: usize,
    /// A controller was connected at least once in this session.
    pub ever_connected: bool,
}

pub enum WorkerCmd {
    /// Drop the handle and connect again now.
    Reconnect,
    /// `[CC 47 01]` so the current hardware effects survive sleep / power-off.
    SaveToFlash,
    /// Suspend / exit: leave persistent lighting, `[CC 47 01]`, then stop driving the device
    /// until [`WorkerCmd::Resume`]. `ack` fires when done.
    Persist {
        policy: ExitLighting,
        cycle_brightness: u8,
        ack: Option<SyncSender<()>>,
    },
    /// After sleep: `[CC 47 00]` + full re-init + resend everything.
    Resume,
    Shutdown,
}

#[derive(Clone, Copy, Debug)]
struct HangInfo {
    op: &'static str,
    limit: Duration,
}

struct Busy {
    since: Instant,
    limit: Duration,
    op: &'static str,
}

#[derive(Default)]
struct Inbox {
    desired: Option<Desired>,
    cmds: VecDeque<WorkerCmd>,
}

struct LinkShared {
    inbox: Mutex<Inbox>,
    cv: Condvar,
    status: Mutex<LinkStatus>,
    busy: Mutex<Option<Busy>>,
    abandoned: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl LinkShared {
    fn new(status: LinkStatus) -> Arc<Self> {
        Arc::new(LinkShared {
            inbox: Mutex::new(Inbox::default()),
            cv: Condvar::new(),
            status: Mutex::new(status),
            busy: Mutex::new(None),
            abandoned: AtomicBool::new(false),
        })
    }

    fn abandoned(&self) -> bool {
        self.abandoned.load(Ordering::SeqCst)
    }

    /// Runs `f` with the watchdog armed.
    fn watched<T>(&self, op: &'static str, limit: Duration, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if self.abandoned() {
            return Err(err(ErrorCode::RgbWriteFailed, "I/O worker was abandoned"));
        }
        *lock(&self.busy) = Some(Busy { since: Instant::now(), limit, op });
        let r = f();
        *lock(&self.busy) = None;
        if self.abandoned() {
            return Err(err(ErrorCode::RgbWriteFailed, format!("{op} returned after the watchdog gave up on it")));
        }
        r
    }
}

/// Transport wrapper that arms the watchdog around every HID call.
struct Watched {
    inner: Box<dyn HidTransport>,
    sh: Arc<LinkShared>,
    limit: Duration,
}

impl HidTransport for Watched {
    fn send_feature(&self, data: &[u8]) -> Result<()> {
        self.sh.watched("feature write", self.limit, || self.inner.send_feature(data))
    }
    fn get_feature(&self, buf: &mut [u8]) -> Result<usize> {
        self.sh.watched("feature read", self.limit, || self.inner.get_feature(buf))
    }
    fn sleep(&self, d: Duration) {
        if !self.sh.abandoned() {
            self.inner.sleep(d);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Worker (runs on "fanrgb-rgb-io")

struct Worker {
    sh: Arc<LinkShared>,
    connector: Box<dyn Connector>,
    t: Timings,
    session: Option<Session>,
    desired: Option<Desired>,
    fresh: bool,
    suspended: bool,
    attempt: u32,
    next_try: Instant,
    last_apply: Instant,
    last_health: Instant,
    saved_at_writes: Option<u64>,
    /// Frame that was current when "Save to controller" sent `[CC 47 01]`. The firmware stays in
    /// keep-settings mode until `[CC 47 00]`; GCC only uses that mode at suspend / exit, so it is
    /// left again (47 00) before the next different frame is written.
    saved_desired: Option<Desired>,
    ever_connected: bool,
    /// Connected and not yet proven healthy (see [`HEALTHY_AFTER`]).
    probation_since: Option<Instant>,
}

/// Decrements the "blocked threads" counter when an abandoned worker finally returns.
struct StuckGuard {
    sh: Arc<LinkShared>,
    stuck: Arc<AtomicUsize>,
}

impl Drop for StuckGuard {
    fn drop(&mut self) {
        if self.sh.abandoned() {
            self.stuck.fetch_sub(1, Ordering::SeqCst);
            log::info!("an abandoned RGB I/O thread returned");
        }
    }
}

fn state_for(code: ErrorCode, attempt: u32, next_try: Instant) -> RgbConnState {
    match code {
        ErrorCode::RgbHung => RgbConnState::Hung,
        ErrorCode::RgbOpenFailed => RgbConnState::InUseByOtherApp,
        ErrorCode::RgbNotFound | ErrorCode::RgbUnsupported => RgbConnState::NotPresent,
        _ => RgbConnState::Reconnecting { attempt, next_try },
    }
}

impl Worker {
    fn new(sh: Arc<LinkShared>, connector: Box<dyn Connector>, t: Timings) -> Self {
        let now = Instant::now();
        let (attempt, ever) = {
            let s = lock(&sh.status);
            (s.attempts, s.ever_connected)
        };
        Worker {
            sh,
            connector,
            t,
            session: None,
            desired: None,
            fresh: false,
            suspended: false,
            attempt,
            next_try: now,
            last_apply: now,
            last_health: now,
            saved_at_writes: None,
            saved_desired: None,
            ever_connected: ever,
            probation_since: None,
        }
    }

    fn status(&self) -> MutexGuard<'_, LinkStatus> {
        lock(&self.sh.status)
    }

    fn run(mut self, hang: Option<HangInfo>, stuck: Arc<AtomicUsize>) {
        let _guard = StuckGuard { sh: self.sh.clone(), stuck };
        let suspended = self.status().suspended;
        self.suspended = suspended;
        if let Some(h) = hang {
            let e = err(
                ErrorCode::RgbWriteFailed,
                format!(
                    "the controller did not complete a {} within {} ms; the request was abandoned",
                    h.op,
                    h.limit.as_millis()
                ),
            );
            self.fail(e);
        }
        loop {
            if self.sh.abandoned() {
                return;
            }
            let (cmds, desired) = self.wait();
            if let Some(d) = desired {
                self.desired = Some(d);
                self.fresh = true;
            }
            for c in cmds {
                if matches!(c, WorkerCmd::Shutdown) {
                    self.session = None;
                    return;
                }
                self.handle(c);
                if self.sh.abandoned() {
                    return;
                }
            }
            if self.session.is_none() && !self.suspended && Instant::now() >= self.next_try {
                self.connect();
            }
            if self.session.is_some() && !self.suspended {
                self.service();
            }
        }
    }

    fn next_wakeup(&self) -> Instant {
        let now = Instant::now();
        let mut at = now + self.t.idle_wake;
        if self.suspended {
            return at;
        }
        if self.session.is_none() {
            at = at.min(self.next_try.max(now));
        } else {
            at = at.min(self.last_health + self.t.health_every);
        }
        at
    }

    fn wait(&self) -> (Vec<WorkerCmd>, Option<Desired>) {
        let deadline = self.next_wakeup();
        let mut g = lock(&self.sh.inbox);
        loop {
            if !g.cmds.is_empty() || g.desired.is_some() || self.sh.abandoned() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            g = self.sh.cv.wait_timeout(g, deadline - now).map(|r| r.0).unwrap_or_else(|p| p.into_inner().0);
        }
        (g.cmds.drain(..).collect(), g.desired.take())
    }

    fn handle(&mut self, c: WorkerCmd) {
        match c {
            WorkerCmd::Shutdown => {}
            WorkerCmd::Reconnect => {
                self.session = None;
                self.attempt = 0;
                self.next_try = Instant::now();
                let mut s = self.status();
                s.state = RgbConnState::Searching;
                s.device = None;
                s.attempts = 0;
                s.next_retry = None;
            }
            WorkerCmd::SaveToFlash => {
                let Some(sess) = self.session.as_mut() else {
                    log::warn!("save to controller ignored: no RGB controller connected");
                    return;
                };
                match sess.dev.persist(true) {
                    Ok(true) => {
                        log::info!("lighting saved to the controller ([CC 47 01])");
                        self.saved_at_writes = Some(sess.hw_writes);
                        self.saved_desired = Some(self.desired.clone().unwrap_or_default());
                        self.status().saved_to_flash = true;
                    }
                    Ok(false) => self.record(
                        ErrorCode::RgbUnsupported,
                        "this controller's firmware cannot keep effects (SuppCmdFlag bit 0 not set)".into(),
                    ),
                    Err(e) => self.fail(e),
                }
            }
            WorkerCmd::Persist { policy, cycle_brightness, ack } => {
                // Suspended *before* the I/O: if the persist hangs and this worker is abandoned, the
                // replacement worker inherits "suspended" and does not re-init (`[CC 47 00]`, clear
                // all) the controller right before sleep / shutdown.
                self.suspended = true;
                self.status().suspended = true;
                if let Some(sess) = self.session.as_mut() {
                    let plan = persist_plan(&self.desired.clone().unwrap_or_default(), policy, cycle_brightness);
                    match sess.persist(&plan, Instant::now()) {
                        Ok(kept) => log::info!(
                            "RGB lighting handed over to the controller ({policy:?}{})",
                            if kept { ", [CC 47 01] keep settings" } else { ", firmware has no keep-settings command" }
                        ),
                        Err(e) => {
                            log::warn!("could not leave persistent lighting: {e:#}");
                            self.fail(e);
                        }
                    }
                }
                if let Some(ack) = ack {
                    let _ = ack.try_send(());
                }
            }
            WorkerCmd::Resume => {
                self.suspended = false;
                self.status().suspended = false;
                if let Some(sess) = self.session.as_mut() {
                    log::info!("RGB: re-initialising the controller after resume");
                    match sess.reinit() {
                        Ok(()) => {
                            self.fresh = true;
                            self.last_health = Instant::now();
                        }
                        Err(e) => self.fail(e),
                    }
                } else {
                    self.attempt = 0;
                    self.next_try = Instant::now();
                }
            }
        }
    }

    fn connect(&mut self) {
        let sh = self.sh.clone();
        let t = self.t;
        let cands = sh.watched("HID enumeration", t.open_timeout, || self.connector.candidates());
        let cands = match cands {
            Ok(c) => c,
            Err(e) => return self.fail(e),
        };
        if cands.is_empty() {
            return self.not_present();
        }
        let mut last: Option<anyhow::Error> = None;
        for c in cands {
            let io = match sh.watched("CreateFile", t.open_timeout, || self.connector.open(&c)) {
                Ok(io) => io,
                Err(e) => {
                    last = Some(e);
                    continue;
                }
            };
            if sh.abandoned() {
                return;
            }
            let io: Box<dyn HidTransport> = Box::new(Watched { inner: io, sh: sh.clone(), limit: t.io_timeout });
            let res = Fusion2::connect(io, c.pid).and_then(|mut f| f.init().map(|_| f));
            match res {
                Ok(f) => return self.on_connected(f),
                Err(e) => {
                    log::debug!("RGB candidate 048D:{:04X} rejected: {e:#}", c.pid);
                    last = Some(e);
                }
            }
        }
        self.fail(last.unwrap_or_else(|| err(ErrorCode::RgbNotFound, "no usable controller collection")));
    }

    fn on_connected(&mut self, f: Fusion2) {
        log::info!(
            "RGB controller: {} (048D:{:04X}) fw {} chip {:08X}, flags {:02X}, cal D_LED1 {:08X} D_LED2 {:08X}, Dynamic Lighting {:?} (0x48 reply {:?})",
            f.info.name,
            f.pid,
            f.info.fw,
            f.info.chip_id,
            f.info.support_flags,
            f.info.cal_strip0,
            f.info.cal_strip1,
            f.dynamic_lighting,
            f.dynamic_lighting_raw
        );
        let now = Instant::now();
        let snap = DeviceSnapshot { pid: f.pid, info: f.info.clone(), dynamic_lighting_raw: f.dynamic_lighting_raw };
        let reconnect = self.ever_connected;
        self.ever_connected = true;
        self.session = Some(Session::new(f));
        // `attempt` (the backoff) is only reset once the connection has proven itself.
        self.probation_since = Some(now);
        self.fresh = true;
        self.last_health = now;
        self.saved_at_writes = None;
        self.saved_desired = None;
        let mut s = self.status();
        s.state = RgbConnState::Connected;
        s.device = Some(snap);
        s.attempts = 0;
        s.next_retry = None;
        s.saved_to_flash = false;
        s.ever_connected = true;
        if reconnect {
            s.reconnects += 1;
        }
    }

    fn service(&mut self) {
        let now = Instant::now();
        if let (Some(sess), Some(d)) = (self.session.as_mut(), self.desired.as_ref())
            && (self.fresh || now.duration_since(self.last_apply) >= Duration::from_millis(250))
        {
            if self.saved_desired.as_ref().is_some_and(|sd| sd != d) {
                // Leave keep-settings mode before writing anything new (review: no flash
                // writes per later apply). The saved effect stays on the controller.
                if let Err(e) = sess.dev.persist(false) {
                    return self.fail(e);
                }
                self.saved_desired = None;
                self.saved_at_writes = None;
                lock(&self.sh.status).saved_to_flash = false;
            }
            let t0 = Instant::now();
            let res = sess.apply(d, now);
            self.fresh = false;
            self.last_apply = now;
            match res {
                Ok(n) => {
                    let writes = sess.hw_writes;
                    let mut s = lock(&self.sh.status);
                    s.packets_total += n;
                    if n > 0 {
                        let per = (t0.elapsed().as_secs_f32() * 1000.0 / n as f32).clamp(0.1, 20.0);
                        s.packet_ms = if s.packet_ms == 0.0 { per } else { s.packet_ms * 0.9 + per * 0.1 };
                    }
                    if self.saved_at_writes.is_some_and(|w| w != writes) {
                        self.saved_at_writes = None;
                        s.saved_to_flash = false;
                    }
                    drop(s);
                    if self.probation_since.is_some_and(|t| now.duration_since(t) >= HEALTHY_AFTER) {
                        self.probation_since = None;
                        self.attempt = 0;
                    }
                }
                Err(e) => return self.fail(e),
            }
        }
        if let Some(sess) = self.session.as_mut()
            && now.duration_since(self.last_health) >= self.t.health_every
        {
            self.last_health = now;
            if let Err(e) = sess.dev.check_alive() {
                self.fail(e);
            }
        }
    }

    /// Records an error without changing the connection.
    fn record(&self, code: ErrorCode, detail: String) {
        let mut s = self.status();
        let now = SystemTime::now();
        match s.last_error.as_mut() {
            Some(e) if e.code == code && e.detail == detail => {
                e.count += 1;
                e.at = now;
                log::debug!("[{code}] {detail} (x{})", e.count);
            }
            _ => {
                match code.severity() {
                    crate::error::Severity::Info => log::info!("[{code}] {detail}"),
                    _ => log::warn!("[{code}] {detail}"),
                }
                s.last_error = Some(RgbErrorInfo { code, detail, at: now, count: 1 });
            }
        }
    }

    fn schedule(&mut self, code: ErrorCode) {
        self.attempt = self.attempt.saturating_add(1);
        self.next_try = Instant::now() + backoff(self.attempt, &self.t);
        let mut s = self.status();
        s.state = state_for(code, self.attempt, self.next_try);
        s.next_retry = Some(self.next_try);
        s.attempts = self.attempt;
        s.device = None;
    }

    /// The device failed: classify, drop the handle, back off.
    fn fail(&mut self, e: anyhow::Error) {
        let was_connected = self.session.take().is_some();
        self.probation_since = None;
        let mut code = code_of(&e);
        let mut detail = format!("{e:#}");
        if code == ErrorCode::Unknown {
            code = ErrorCode::RgbWriteFailed;
        }
        if matches!(
            code,
            ErrorCode::RgbWriteFailed | ErrorCode::RgbBadReply | ErrorCode::RgbDisconnected | ErrorCode::RgbFlashFailed
        ) && let Some(p) = self.connector.usb_problem()
        {
            code = ErrorCode::RgbHung;
            detail = format!("Windows reports USB problem code {p} for the controller ({detail})");
        }
        if was_connected || code != ErrorCode::RgbNotFound {
            self.status().write_errors += 1;
        }
        self.record(code, detail);
        self.schedule(code);
    }

    fn not_present(&mut self) {
        let (code, detail) = match self.connector.usb_problem() {
            Some(p) => (
                ErrorCode::RgbHung,
                format!("the controller failed to start: Windows reports USB error {p} on its port"),
            ),
            None => (
                ErrorCode::RgbNotFound,
                "no RGB Fusion 2 controller (USB 048D:5702/8297/5711/8950) is connected".into(),
            ),
        };
        self.record(code, detail);
        self.schedule(code);
    }
}

// ---------------------------------------------------------------------------------------
// Engine side

pub type ConnectorFactory = Arc<dyn Fn() -> Box<dyn Connector> + Send + Sync>;

/// At most this many worker threads may be stuck in the driver at once.
pub const MAX_STUCK: usize = 3;

pub struct Link {
    sh: Arc<LinkShared>,
    factory: ConnectorFactory,
    t: Timings,
    stuck: Arc<AtomicUsize>,
    pending: Option<HangInfo>,
}

impl Link {
    pub fn start(factory: ConnectorFactory, t: Timings) -> Link {
        let link = Link {
            sh: LinkShared::new(LinkStatus::default()),
            factory,
            t,
            stuck: Arc::new(AtomicUsize::new(0)),
            pending: None,
        };
        link.spawn(None);
        link
    }

    fn spawn(&self, hang: Option<HangInfo>) {
        let sh = self.sh.clone();
        let connector = (self.factory)();
        let t = self.t;
        let stuck = self.stuck.clone();
        let res = std::thread::Builder::new()
            .name("fanrgb-rgb-io".into())
            .spawn(move || Worker::new(sh, connector, t).run(hang, stuck));
        if let Err(e) = res {
            log::error!("could not start the RGB I/O thread: {e}");
            let mut s = lock(&self.sh.status);
            s.last_error = Some(RgbErrorInfo {
                code: ErrorCode::Internal,
                detail: format!("RGB I/O thread: {e}"),
                at: SystemTime::now(),
                count: 1,
            });
        }
    }

    /// Hands the next frame to the worker (replaces any frame it hasn't picked up yet).
    pub fn post(&self, d: Desired) {
        lock(&self.sh.inbox).desired = Some(d);
        self.sh.cv.notify_one();
    }

    pub fn send(&self, c: WorkerCmd) {
        lock(&self.sh.inbox).cmds.push_back(c);
        self.sh.cv.notify_one();
    }

    /// Watchdog: call regularly from the lighting thread.
    pub fn poll(&mut self) {
        if let Some(h) = self.pending {
            if self.stuck.load(Ordering::SeqCst) < MAX_STUCK {
                self.pending = None;
                self.spawn(Some(h));
            }
            return;
        }
        let over = lock(&self.sh.busy)
            .as_ref()
            .filter(|b| b.since.elapsed() > b.limit)
            .map(|b| HangInfo { op: b.op, limit: b.limit });
        if let Some(h) = over {
            self.abandon(h);
        }
    }

    fn abandon(&mut self, h: HangInfo) {
        log::error!(
            "RGB controller did not complete a {} within {} ms: abandoning the handle",
            h.op,
            h.limit.as_millis()
        );
        let old = self.sh.clone();
        old.abandoned.store(true, Ordering::SeqCst);
        old.cv.notify_all();
        let stuck = self.stuck.fetch_add(1, Ordering::SeqCst) + 1;
        let mut carry = lock(&old.status).clone();
        carry.device = None;
        carry.state =
            RgbConnState::Reconnecting { attempt: carry.attempts + 1, next_try: Instant::now() + self.t.backoff_min };
        let cmds: VecDeque<WorkerCmd> = lock(&old.inbox).cmds.drain(..).collect();
        let desired = lock(&old.inbox).desired.take();
        self.sh = LinkShared::new(carry);
        {
            let mut ib = lock(&self.sh.inbox);
            ib.cmds = cmds;
            ib.desired = desired;
        }
        if stuck >= MAX_STUCK {
            let mut s = lock(&self.sh.status);
            s.state = RgbConnState::Hung;
            s.last_error = Some(RgbErrorInfo {
                code: ErrorCode::RgbHung,
                detail: format!("{stuck} requests to the controller are stuck in the USB driver"),
                at: SystemTime::now(),
                count: 1,
            });
            self.pending = Some(h);
        } else {
            self.spawn(Some(h));
        }
    }

    /// "Reconnect" button: a worker stuck in the driver is abandoned right away. Returns false when
    /// nothing can be tried because too many requests are stuck in the USB driver (only a restart or
    /// a power drain helps then); the status error says so.
    pub fn reconnect(&mut self) -> bool {
        if self.pending.is_some() {
            let stuck = self.stuck.load(Ordering::SeqCst);
            if stuck < MAX_STUCK {
                self.poll();
                return true;
            }
            let mut s = lock(&self.sh.status);
            s.state = RgbConnState::Hung;
            s.last_error = Some(RgbErrorInfo {
                code: ErrorCode::RgbHung,
                detail: format!(
                    "{stuck} requests to the controller are stuck in the USB driver; restart Windows (or shut down and drain power) to recover"
                ),
                at: SystemTime::now(),
                count: s.last_error.as_ref().map_or(1, |e| e.count + 1),
            });
            return false;
        }
        let stuck_now = lock(&self.sh.busy)
            .as_ref()
            .filter(|b| b.since.elapsed() > Duration::from_millis(250))
            .map(|b| HangInfo { op: b.op, limit: b.limit });
        match stuck_now {
            Some(h) => self.abandon(h),
            None => self.send(WorkerCmd::Reconnect),
        }
        true
    }

    /// Connected?, and the smoothed time per feature report: what every frame needs, without
    /// cloning the device info as [`Link::status`] does.
    pub fn conn(&self) -> (bool, f32) {
        let s = lock(&self.sh.status);
        (s.state == RgbConnState::Connected, s.packet_ms)
    }

    pub fn status(&self) -> LinkStatus {
        let mut s = lock(&self.sh.status).clone();
        s.hung_threads = self.stuck.load(Ordering::SeqCst);
        s
    }

    /// Sends a command with an ack channel and waits for it (keeps the watchdog running).
    pub fn request(&mut self, make: impl FnOnce(SyncSender<()>) -> WorkerCmd, timeout: Duration) -> bool {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.send(make(tx));
        let end = Instant::now() + timeout;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return true,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    self.poll();
                    if Instant::now() >= end {
                        return false;
                    }
                }
            }
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.send(WorkerCmd::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rgb::fusion2::HwEffect;
    use crate::rgb::mock::{MockConnector, MockDev, Plug, pkt};
    use crate::rgb::session::HeaderOut;

    fn fast() -> Timings {
        Timings {
            io_timeout: Duration::from_millis(80),
            open_timeout: Duration::from_millis(300),
            backoff_min: Duration::from_millis(20),
            backoff_max: Duration::from_millis(160),
            health_every: Duration::from_secs(60),
            idle_wake: Duration::from_millis(10),
        }
    }

    fn start(conn: &MockConnector) -> Link {
        let c = conn.clone();
        Link::start(Arc::new(move || Box::new(c.clone()) as Box<dyn Connector>), fast())
    }

    fn wait(link: &mut Link, what: &str, pred: impl Fn(&LinkStatus) -> bool) -> LinkStatus {
        let start = Instant::now();
        loop {
            link.poll();
            let s = link.status();
            if pred(&s) {
                return s;
            }
            assert!(start.elapsed() < Duration::from_secs(4), "timed out waiting for {what}: {s:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn strip(n: usize) -> Desired {
        Desired {
            led_counts: [n as u16, 1],
            headers: [HeaderOut::Direct(vec![[1, 2, 3]; n]), HeaderOut::Unused],
            ..Default::default()
        }
    }

    fn code(s: &LinkStatus) -> Option<ErrorCode> {
        s.last_error.as_ref().map(|e| e.code)
    }

    #[test]
    fn backoff_doubles_to_30s() {
        let t = Timings::default();
        let secs: Vec<u64> = (1..=8).map(|a| backoff(a, &t).as_secs()).collect();
        assert_eq!(secs, vec![1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(backoff(u32::MAX, &t).as_secs(), 30);
    }

    #[test]
    fn connects_and_streams() {
        let dev = MockDev::new(0x03);
        let conn = MockConnector::new(vec![Plug::Present(dev.clone())]);
        let mut link = start(&conn);
        link.post(strip(10));
        let s = wait(&mut link, "connected", |s| s.state == RgbConnState::Connected);
        assert_eq!(s.device.as_ref().unwrap().pid, 0x5702);
        wait(&mut link, "stream", |_| dev.sent().iter().any(|p| p[1] == 0x58));
        assert_eq!(link.status().write_errors, 0);
    }

    #[test]
    fn hung_write_is_abandoned_and_reconnected() {
        let dev1 = MockDev::new(0x03);
        dev1.block_on(0x58, 1);
        let dev2 = MockDev::new(0x03);
        let conn = MockConnector::new(vec![Plug::Present(dev1.clone()), Plug::Present(dev2.clone())]);
        let mut link = start(&conn);
        link.post(strip(10));
        let s = wait(&mut link, "watchdog", |s| s.last_error.is_some());
        assert_eq!(code(&s), Some(ErrorCode::RgbWriteFailed));
        assert!(s.last_error.unwrap().detail.contains("feature write"));
        assert_eq!(s.hung_threads, 1);
        assert_eq!(dev1.blocked.load(Ordering::SeqCst), 1);
        // The new worker reconnects (to the re-enumerated device) without waiting for the stuck one.
        link.post(strip(10));
        let s = wait(&mut link, "reconnected", |s| s.state == RgbConnState::Connected);
        assert_eq!(s.reconnects, 1);
        wait(&mut link, "stream on new handle", |_| dev2.sent().iter().any(|p| p[1] == 0x58));
        // When the stuck call finally returns, that thread exits.
        dev1.release.store(true, Ordering::SeqCst);
        wait(&mut link, "stuck thread exit", |s| s.hung_threads == 0);
    }

    #[test]
    fn hang_with_usb_error_43_is_rgb_hung() {
        let dev = MockDev::new(0x03);
        dev.block_on(0x58, 1);
        let conn = MockConnector::new(vec![Plug::Present(dev.clone()), Plug::Absent]);
        *conn.usb_problem.lock().unwrap() = Some(43);
        let mut link = start(&conn);
        link.post(strip(5));
        let s = wait(&mut link, "hung", |s| s.state == RgbConnState::Hung && s.last_error.is_some());
        assert_eq!(code(&s), Some(ErrorCode::RgbHung));
        assert!(s.last_error.unwrap().detail.contains("43"));
        dev.release.store(true, Ordering::SeqCst);
    }

    #[test]
    fn disconnect_backs_off_and_recovers() {
        let dev = MockDev::new(0x03);
        let conn = MockConnector::new(vec![Plug::Present(dev.clone())]);
        let mut link = start(&conn);
        wait(&mut link, "connected", |s| s.state == RgbConnState::Connected);
        dev.fail_all_sends(Some(1167));
        link.post(strip(5));
        let s = wait(&mut link, "disconnect", |s| matches!(s.state, RgbConnState::Reconnecting { .. }));
        assert_eq!(code(&s), Some(ErrorCode::RgbDisconnected));
        assert!(s.last_error.as_ref().unwrap().detail.contains("1167"));
        // Re-opening keeps failing: attempts grow and the retry is scheduled with backoff.
        let s = wait(&mut link, "attempt 3", |s| s.attempts >= 3);
        assert!(matches!(s.state, RgbConnState::Reconnecting { attempt, .. } if attempt >= 3));
        assert!(s.last_error.as_ref().unwrap().count >= 2, "repeats are counted, not re-logged");
        dev.fail_all_sends(None);
        let s = wait(&mut link, "recovered", |s| s.state == RgbConnState::Connected);
        assert_eq!(s.attempts, 0);
        assert!(s.reconnects >= 1);
    }

    #[test]
    fn absent_hung_and_in_use_states() {
        let conn = MockConnector::new(vec![Plug::Absent]);
        let mut link = start(&conn);
        let s = wait(&mut link, "not present", |s| s.state == RgbConnState::NotPresent);
        assert_eq!(code(&s), Some(ErrorCode::RgbNotFound));
        assert!(s.next_retry.is_some());
        *conn.usb_problem.lock().unwrap() = Some(43);
        let s = wait(&mut link, "hung", |s| s.state == RgbConnState::Hung);
        assert_eq!(code(&s), Some(ErrorCode::RgbHung));

        let conn = MockConnector::new(vec![Plug::OpenFails(5)]);
        let mut link = start(&conn);
        let s = wait(&mut link, "in use", |s| s.state == RgbConnState::InUseByOtherApp);
        assert_eq!(code(&s), Some(ErrorCode::RgbOpenFailed));
    }

    #[test]
    fn bad_reply_is_rejected() {
        let dev = MockDev::new(0x03);
        for i in 1..64 {
            dev.set_info_byte(i, 0);
        }
        let conn = MockConnector::new(vec![Plug::Present(dev)]);
        let mut link = start(&conn);
        let s = wait(&mut link, "bad reply", |s| s.last_error.is_some());
        assert_eq!(code(&s), Some(ErrorCode::RgbBadReply));
        assert!(matches!(s.state, RgbConnState::Reconnecting { .. }));
        // A non-motherboard MCU is "not present" (unsupported), not an endless error loop.
        let dev = MockDev::new(0x03);
        dev.set_info_byte(1, 3);
        let conn = MockConnector::new(vec![Plug::Present(dev)]);
        let mut link = start(&conn);
        let s = wait(&mut link, "unsupported", |s| s.last_error.is_some());
        assert_eq!(code(&s), Some(ErrorCode::RgbUnsupported));
        assert_eq!(s.state, RgbConnState::NotPresent);
    }

    #[test]
    fn suspend_persists_and_resume_reinitialises() {
        let dev = MockDev::new(0x03);
        let conn = MockConnector::new(vec![Plug::Present(dev.clone())]);
        let mut link = start(&conn);
        link.post(strip(10));
        wait(&mut link, "stream", |_| dev.sent().iter().any(|p| p[1] == 0x58));
        let ok = link.request(
            |ack| WorkerCmd::Persist { policy: ExitLighting::HardwareCycle, cycle_brightness: 200, ack: Some(ack) },
            Duration::from_secs(2),
        );
        assert!(ok);
        assert!(link.status().suspended);
        let sent = dev.sent();
        assert_eq!(sent.last().unwrap(), &pkt(&[0x47, 0x01]));
        let cyc = HwEffect::new(crate::rgb::fusion2::HwMode::ColorCycle, [255, 255, 255], 200, 4);
        assert!(sent.contains(&crate::rgb::fusion2::effect_packet(5, &cyc, 0x20).to_vec()));
        // While suspended nothing is written.
        dev.clear_log();
        link.post(strip(12));
        std::thread::sleep(Duration::from_millis(60));
        assert!(dev.sent().is_empty());
        // Resume: [CC 47 00] + full init, then streaming again.
        link.send(WorkerCmd::Resume);
        wait(&mut link, "re-init + stream", |_| dev.sent().iter().any(|p| p[1] == 0x58));
        let sent = dev.sent();
        assert_eq!(sent[0], pkt(&[0x60, 0]));
        assert!(sent.contains(&pkt(&[0x47, 0x00])));
        assert!(!link.status().suspended);
    }

    #[test]
    fn hung_persist_leaves_the_replacement_worker_suspended() {
        let dev = MockDev::new(0x03);
        let conn = MockConnector::new(vec![Plug::Present(dev.clone())]);
        let mut link = start(&conn);
        link.post(strip(10));
        wait(&mut link, "stream", |_| dev.sent().iter().any(|p| p[1] == 0x58));
        dev.block_on(0x47, 1);
        let ok = link.request(
            |ack| WorkerCmd::Persist { policy: ExitLighting::HardwareCycle, cycle_brightness: 200, ack: Some(ack) },
            Duration::from_millis(400),
        );
        assert!(!ok, "the persist hung");
        let s = wait(&mut link, "abandoned", |s| s.hung_threads == 1);
        assert!(s.suspended, "the new worker must not re-init the controller before sleep");
        dev.clear_log();
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !dev.sent().iter().any(|p| p[1] == 0x60 || p == &pkt(&[0x47, 0x00])),
            "no reconnect / init while suspended"
        );
        dev.release.store(true, Ordering::SeqCst);
    }

    #[test]
    fn backoff_grows_when_a_fresh_connection_fails_at_once() {
        // Passes [CC 60] and init, but every strip write fails: the retry interval must grow instead
        // of re-initialising the controller every backoff_min forever.
        let dev = MockDev::new(0x03);
        dev.fail_cmd(0x58, Some(31));
        let conn = MockConnector::new(vec![Plug::Present(dev.clone())]);
        let mut link = start(&conn);
        link.post(strip(10));
        let s = wait(&mut link, "attempt 3", |s| s.attempts >= 3);
        assert!(matches!(s.state, RgbConnState::Reconnecting { attempt, .. } if attempt >= 3));
    }

    #[test]
    fn reconnect_reports_when_too_many_requests_are_stuck() {
        let dev = MockDev::new(0x03);
        dev.block_on(0x58, 10);
        let conn = MockConnector::new(vec![Plug::Present(dev.clone())]);
        let mut link = start(&conn);
        for _ in 0..MAX_STUCK {
            link.post(strip(10));
            let n = link.status().hung_threads;
            wait(&mut link, "another stuck thread", |s| s.hung_threads > n);
        }
        assert_eq!(link.status().state, RgbConnState::Hung);
        assert!(!link.reconnect(), "nothing can be tried");
        let s = link.status();
        assert!(s.last_error.unwrap().detail.contains("restart Windows"));
        dev.release.store(true, Ordering::SeqCst);
    }

    #[test]
    fn save_to_flash_and_unsupported_firmware() {
        let dev = MockDev::new(0x01);
        let conn = MockConnector::new(vec![Plug::Present(dev.clone())]);
        let mut link = start(&conn);
        wait(&mut link, "connected", |s| s.state == RgbConnState::Connected);
        link.send(WorkerCmd::SaveToFlash);
        wait(&mut link, "saved", |s| s.saved_to_flash);
        assert_eq!(dev.sent().last().unwrap(), &pkt(&[0x47, 0x01]));
        // A different frame afterwards first leaves keep-settings mode.
        link.post(strip(4));
        wait(&mut link, "47 00 then stream", |_| dev.sent().iter().any(|p| p[1] == 0x58));
        let sent = dev.sent();
        let save = sent.iter().position(|p| p == &pkt(&[0x47, 0x01])).unwrap();
        let leave = sent.iter().rposition(|p| p == &pkt(&[0x47, 0x00])).unwrap();
        let stream = sent.iter().position(|p| p[1] == 0x58).unwrap();
        assert!(save < leave && leave < stream, "47 01 … 47 00 before new writes");
        assert!(!link.status().saved_to_flash);

        let dev = MockDev::new(0x00);
        let conn = MockConnector::new(vec![Plug::Present(dev.clone())]);
        let mut link = start(&conn);
        wait(&mut link, "connected", |s| s.state == RgbConnState::Connected);
        link.send(WorkerCmd::SaveToFlash);
        let s = wait(&mut link, "unsupported", |s| s.last_error.is_some());
        assert_eq!(code(&s), Some(ErrorCode::RgbUnsupported));
        assert_eq!(s.state, RgbConnState::Connected, "an unsupported command doesn't drop the device");
    }

    /// "Reconnect" (button or a USB re-plug notification) while the PC is going to sleep: the worker
    /// drops the handle but must not open or write the controller until Resume.
    #[test]
    fn reconnect_while_suspended_stays_silent_until_resume() {
        let dev = MockDev::new(0x03);
        let conn = MockConnector::new(vec![Plug::Present(dev.clone())]);
        let mut link = start(&conn);
        link.post(strip(10));
        wait(&mut link, "stream", |_| dev.sent().iter().any(|p| p[1] == 0x58));
        let ok = link.request(
            |ack| WorkerCmd::Persist { policy: ExitLighting::KeepLast, cycle_brightness: 200, ack: Some(ack) },
            Duration::from_secs(2),
        );
        assert!(ok && link.status().suspended);
        dev.clear_log();
        let attempts = conn.attempts.load(Ordering::SeqCst);
        link.reconnect();
        link.post(strip(10));
        std::thread::sleep(Duration::from_millis(150));
        link.poll();
        assert!(dev.sent().is_empty(), "wrote while suspended: {:?}", dev.sent());
        assert_eq!(conn.attempts.load(Ordering::SeqCst), attempts, "no enumeration while suspended");
        let s = link.status();
        assert!(s.suspended && s.device.is_none(), "{s:?}");
        link.send(WorkerCmd::Resume);
        let s = wait(&mut link, "reconnected after resume", |s| s.state == RgbConnState::Connected);
        assert!(!s.suspended);
        wait(&mut link, "stream after resume", |_| dev.sent().iter().any(|p| p[1] == 0x58));
        assert_eq!(dev.sent()[0], pkt(&[0x60, 0]), "fresh init on the new handle");
    }

    /// Suspend while the controller is missing (e.g. USB error 43 before sleep), the controller comes
    /// back during sleep: nothing is touched until Resume, then it connects right away (no backoff).
    #[test]
    fn suspend_while_absent_then_resume_connects_immediately() {
        let conn = MockConnector::new(vec![Plug::Absent]);
        let mut link = start(&conn);
        wait(&mut link, "not present", |s| s.state == RgbConnState::NotPresent);
        let ok = link.request(
            |ack| WorkerCmd::Persist { policy: ExitLighting::Off, cycle_brightness: 200, ack: Some(ack) },
            Duration::from_secs(2),
        );
        assert!(ok, "the sleep handler is acknowledged even without a device");
        assert!(link.status().suspended);
        let dev = MockDev::new(0x03);
        *conn.plugs.lock().unwrap() = vec![Plug::Present(dev.clone())].into();
        let attempts = conn.attempts.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(conn.attempts.load(Ordering::SeqCst), attempts, "no reconnect attempts while suspended");
        assert!(dev.sent().is_empty());
        link.post(strip(4));
        link.send(WorkerCmd::Resume);
        let s = wait(&mut link, "connected after resume", |s| s.state == RgbConnState::Connected);
        assert!(!s.suspended && s.attempts == 0);
        wait(&mut link, "stream", |_| dev.sent().iter().any(|p| p[1] == 0x58));
    }
}
