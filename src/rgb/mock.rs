//! Recording mock of the IT5702 for unit tests (no hardware is ever touched by tests), also used
//! as the simulated controller in `--demo` (then without recording).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::fusion2::PKT;
use super::transport::{Candidate, Connector, HidTransport, io_error};

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// A feature report written (always 64 bytes).
    Send(Vec<u8>),
    /// A feature report read; the value is the command byte the reply answers.
    Get(u8),
    /// A settle delay in ms.
    Sleep(u64),
}

/// 64-byte report `[CC, bytes.., 0..]`.
#[cfg(test)]
pub fn pkt(bytes: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; PKT];
    v[0] = 0xCC;
    v[1..1 + bytes.len()].copy_from_slice(bytes);
    v
}

/// A plausible IT5702 `[CC 60]` reply.
pub fn info_reply(flags: u8) -> [u8; PKT] {
    let mut b = [0u8; PKT];
    b[0] = 0xCC;
    b[1] = 1; // motherboard
    b[2] = 0;
    b[3] = 0;
    b[4..8].copy_from_slice(&[2, 0, 10, 0]);
    b[11] = flags;
    let name = b"IT5702-GIGABYTE V2.0.10.0";
    b[12..12 + name.len()].copy_from_slice(name);
    b[44..48].copy_from_slice(&0x0001_0002u32.to_le_bytes()); // GRB
    b[48..52].copy_from_slice(&0x0001_0002u32.to_le_bytes());
    b[56..60].copy_from_slice(&0x5702_0400u32.to_le_bytes());
    b
}

struct State {
    log: Vec<Op>,
    info: [u8; PKT],
    dl_reply: [u8; PKT],
    last_cmd: u8,
    /// Fail the next send with this Win32 error.
    fail_next: Option<u32>,
    /// Fail every send with this Win32 error.
    fail_all: Option<u32>,
    /// Block a send whose command byte matches, the first `block_count` times.
    block_cmd: Option<u8>,
    block_count: usize,
    /// Fail every send whose command byte matches, with this Win32 error.
    fail_cmd: Option<(u8, u32)>,
    /// Keep a log of all I/O (tests). The demo controller does not, so it runs forever in O(1).
    record: bool,
}

/// Shared handle to one simulated controller.
#[derive(Clone)]
pub struct MockDev {
    st: Arc<Mutex<State>>,
    /// Set to release a blocked send.
    pub release: Arc<AtomicBool>,
    /// How many sends are currently blocked.
    pub blocked: Arc<AtomicUsize>,
}

impl MockDev {
    pub fn new(flags: u8) -> Self {
        let mut dl = [0u8; PKT];
        dl[0] = 0xCC;
        dl[1] = 1; // Dynamic Lighting disabled
        let st = State {
            log: Vec::new(),
            info: info_reply(flags),
            dl_reply: dl,
            last_cmd: 0,
            fail_next: None,
            fail_all: None,
            block_cmd: None,
            block_count: 0,
            fail_cmd: None,
            record: true,
        };
        MockDev {
            st: Arc::new(Mutex::new(st)),
            release: Arc::new(AtomicBool::new(false)),
            blocked: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The `--demo` controller: an IT5702 with keep-settings support and Dynamic Lighting off.
    pub fn simulated() -> Self {
        let d = MockDev::new(0x03);
        d.st.lock().unwrap_or_else(|p| p.into_inner()).record = false;
        d
    }

    pub fn transport(&self) -> Box<dyn HidTransport> {
        Box::new(MockTransport(self.clone()))
    }
}

#[cfg(test)]
impl MockDev {
    pub fn log(&self) -> Vec<Op> {
        self.st.lock().unwrap().log.clone()
    }

    pub fn sent(&self) -> Vec<Vec<u8>> {
        self.log()
            .into_iter()
            .filter_map(|o| match o {
                Op::Send(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    pub fn clear_log(&self) {
        self.st.lock().unwrap().log.clear();
    }

    pub fn fail_next_send(&self, win32: u32) {
        self.st.lock().unwrap().fail_next = Some(win32);
    }

    pub fn fail_all_sends(&self, win32: Option<u32>) {
        self.st.lock().unwrap().fail_all = win32;
    }

    pub fn block_on(&self, cmd: u8, times: usize) {
        let mut s = self.st.lock().unwrap();
        s.block_cmd = Some(cmd);
        s.block_count = times;
    }

    pub fn set_info_byte(&self, i: usize, v: u8) {
        self.st.lock().unwrap().info[i] = v;
    }

    pub fn fail_cmd(&self, cmd: u8, win32: Option<u32>) {
        self.st.lock().unwrap().fail_cmd = win32.map(|w| (cmd, w));
    }

    pub fn set_dynamic_lighting_reply(&self, v: u8) {
        self.st.lock().unwrap().dl_reply[1] = v;
    }
}

pub struct MockTransport(pub MockDev);

impl HidTransport for MockTransport {
    fn send_feature(&self, data: &[u8]) -> Result<()> {
        assert_eq!(data.len(), PKT, "feature reports are 64 bytes");
        assert_eq!(data[0], 0xCC);
        let block = {
            let mut s = self.0.st.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(code) = s.fail_next.take().or(s.fail_all) {
                return Err(io_error("HID feature write", Some(code), "mock failure", false));
            }
            if let Some((_, code)) = s.fail_cmd.filter(|(c, _)| *c == data[1]) {
                return Err(io_error("HID feature write", Some(code), "mock failure", false));
            }
            s.last_cmd = data[1];
            if s.record {
                s.log.push(Op::Send(data.to_vec()));
            }
            if s.block_cmd == Some(data[1]) && s.block_count > 0 {
                s.block_count -= 1;
                true
            } else {
                false
            }
        };
        if block {
            // A stalled control transfer: hidapi would wait forever. Released by the test, or
            // after 5 s so no test thread outlives the suite for long.
            self.0.blocked.fetch_add(1, Ordering::SeqCst);
            let start = Instant::now();
            while !self.0.release.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.0.blocked.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(())
    }

    fn get_feature(&self, buf: &mut [u8]) -> Result<usize> {
        let mut s = self.0.st.lock().unwrap_or_else(|p| p.into_inner());
        let cmd = s.last_cmd;
        if s.record {
            s.log.push(Op::Get(cmd));
        }
        let reply = match cmd {
            0x60 => s.info,
            0x48 => s.dl_reply,
            _ => {
                let mut z = [0u8; PKT];
                z[0] = 0xCC;
                z
            }
        };
        let n = buf.len().min(PKT);
        buf[..n].copy_from_slice(&reply[..n]);
        Ok(n)
    }

    fn sleep(&self, d: Duration) {
        let mut s = self.0.st.lock().unwrap_or_else(|p| p.into_inner());
        if s.record {
            s.log.push(Op::Sleep(d.as_millis() as u64));
        }
    }
}

/// What the mock connector reports on the next `candidates()` / `open()` calls.
#[derive(Clone)]
pub enum Plug {
    /// The device is enumerated and opens.
    Present(MockDev),
    /// Nothing enumerated.
    #[cfg_attr(not(test), allow(dead_code))]
    Absent,
    /// Enumerated, but `open` fails with this Win32 error.
    #[cfg_attr(not(test), allow(dead_code))]
    OpenFails(u32),
}

/// Scripted connector: pops one `Plug` per connection attempt (the last one repeats).
#[derive(Clone)]
pub struct MockConnector {
    pub plugs: Arc<Mutex<VecDeque<Plug>>>,
    pub usb_problem: Arc<Mutex<Option<u32>>>,
    pub attempts: Arc<AtomicUsize>,
    current: Option<Plug>,
}

impl MockConnector {
    pub fn new(plugs: Vec<Plug>) -> Self {
        MockConnector {
            plugs: Arc::new(Mutex::new(plugs.into())),
            usb_problem: Arc::new(Mutex::new(None)),
            attempts: Arc::new(AtomicUsize::new(0)),
            current: None,
        }
    }
}

impl Connector for MockConnector {
    fn candidates(&mut self) -> Result<Vec<Candidate>> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let plug = {
            let mut q = self.plugs.lock().unwrap();
            if q.len() > 1 { q.pop_front() } else { q.front().cloned() }
        };
        self.current = plug.clone();
        Ok(match plug {
            None | Some(Plug::Absent) => Vec::new(),
            Some(_) => vec![Candidate { path: std::ffi::CString::new("mock").unwrap(), pid: 0x5702 }],
        })
    }

    fn open(&mut self, _c: &Candidate) -> Result<Box<dyn HidTransport>> {
        match self.current.clone() {
            Some(Plug::Present(d)) => Ok(d.transport()),
            Some(Plug::OpenFails(code)) => Err(io_error("opening the RGB controller", Some(code), "mock", true)),
            _ => Err(io_error("opening the RGB controller", Some(2), "mock", true)),
        }
    }

    fn usb_problem(&mut self) -> Option<u32> {
        *self.usb_problem.lock().unwrap()
    }
}
