use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

pub const DEFAULT_PORT: u16 = 1337;

const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const RECONNECT_WAIT: std::time::Duration = std::time::Duration::from_millis(750);
const SLOW_RTT_MS: f32 = 50.0;
const RECENT_CAP: usize = 48;

fn to_spice_raw(v: f32) -> f32 {
    (v * 0.5 + 0.5).clamp(0.0, 1.0)
}

#[derive(Default)]
struct Pending {
    buttons: Vec<(String, bool)>,
    analogs: Option<(f32, f32)>,
    stop: bool,
}

#[derive(Default)]
struct IoStats {
    req_ok: u64,
    req_fail: u64,
    slow_req: u64,
    last_rtt_ms: f32,
    max_rtt_ms: f32,
    last_module: String,
    in_flight_since: Option<Instant>,
    in_flight_label: String,
    recent: VecDeque<String>,
}

impl IoStats {
    fn push_recent(&mut self, line: String) {
        if self.recent.len() == RECENT_CAP {
            self.recent.pop_front();
        }
        self.recent.push_back(line);
    }
}

struct Shared {
    pending: Mutex<Pending>,
    cv: Condvar,
    connected: AtomicBool,
    status: Mutex<String>,
    stats: Mutex<IoStats>,
}

pub struct Client {
    shared: Arc<Shared>,
    worker: Option<std::thread::JoinHandle<()>>,
    port: u16,
}

impl Client {
    pub fn start(port: u16) -> Client {
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending::default()),
            cv: Condvar::new(),
            connected: AtomicBool::new(false),
            status: Mutex::new(format!("connecting to 127.0.0.1:{port}")),
            stats: Mutex::new(IoStats::default()),
        });
        let worker = {
            let shared_for_thread = shared.clone();
            match std::thread::Builder::new()
                .name("spiceapi".into())
                .spawn(move || run(shared_for_thread, port))
            {
                Ok(handle) => Some(handle),
                Err(e) => {
                    set_status(&shared, false, format!("worker spawn failed: {e}"));
                    log::error!("spiceapi worker spawn failed: {e}");
                    None
                }
            }
        };
        Client {
            shared,
            worker,
            port,
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn button(&self, name: &str, down: bool) {
        let mut p = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        p.buttons.push((name.to_string(), down));
        let q = p.buttons.len();
        let connected = self.shared.connected.load(Ordering::Relaxed);
        drop(p);
        self.shared.cv.notify_one();
        if !connected && (q == 1 || q == 10 || q == 25 || q == 50) {
            log::warn!("spiceapi enqueue while disconnected: {name} down={down} q={q}");
        } else if q == 20 || q == 50 || q == 100 {
            log::warn!("spiceapi button backlog q={q}");
        }
    }

    pub fn faders(&self, left: f32, right: f32) {
        let mut p = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        p.analogs = Some((left, right));
        drop(p);
        self.shared.cv.notify_one();
    }

    pub fn status(&self) -> String {
        self.shared
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed)
    }

    pub fn debug_snapshot(&self) -> String {
        let connected = self.connected();
        let status = self.status();
        let (q_btn, q_analog, stop) = {
            let p = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (p.buttons.len(), p.analogs.is_some(), p.stop)
        };
        let st = self.shared.stats.lock().unwrap_or_else(|e| e.into_inner());
        let in_flight = match st.in_flight_since {
            Some(t) => format!(
                "{} for {:.0}ms",
                st.in_flight_label,
                t.elapsed().as_secs_f32() * 1000.0
            ),
            None => "idle".into(),
        };
        let recent: Vec<&str> = st.recent.iter().map(|s| s.as_str()).collect();
        format!(
            "spiceapi snapshot port={} connected={} status={:?} q_buttons={} q_analogs={} stop={} \
             req_ok={} req_fail={} slow>={SLOW_RTT_MS}ms={} last_rtt={:.1}ms max_rtt={:.1}ms \
             last_module={} in_flight=[{}] recent=[\n  {}\n]",
            self.port,
            connected,
            status,
            q_btn,
            q_analog,
            stop,
            st.req_ok,
            st.req_fail,
            st.slow_req,
            st.last_rtt_ms,
            st.max_rtt_ms,
            st.last_module,
            in_flight,
            recent.join("\n  ")
        )
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        {
            let mut p = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            p.stop = true;
        }
        self.shared.cv.notify_all();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn set_status(shared: &Shared, connected: bool, msg: impl Into<String>) {
    shared.connected.store(connected, Ordering::Relaxed);
    *shared.status.lock().unwrap_or_else(|e| e.into_inner()) = msg.into();
}

fn note_recent(shared: &Shared, line: impl Into<String>) {
    let line = line.into();
    log::info!("{line}");
    shared
        .stats
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_recent(line);
}

fn run(shared: Arc<Shared>, port: u16) {
    loop {
        if shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stop
        {
            return;
        }
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(sock) => {
                let _ = sock.set_nodelay(true);
                note_recent(&shared, format!("spiceapi connected :{port}"));
                set_status(&shared, true, format!("connected :{port}"));
                let why = pump(&shared, sock);
                set_status(&shared, false, format!("disconnected ({why})"));
                note_recent(&shared, format!("spiceapi disconnected: {why}"));
            }
            Err(e) => {
                set_status(&shared, false, format!("no game on :{port}"));
                let _ = e;
            }
        }
        let p = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
        if p.stop {
            return;
        }
        let (p, _) = shared
            .cv
            .wait_timeout(p, RECONNECT_WAIT)
            .unwrap_or_else(|e| e.into_inner());
        if p.stop {
            return;
        }
    }
}

fn pump(shared: &Shared, sock: TcpStream) -> String {
    let reader_sock = match sock.try_clone() {
        Ok(r) => r,
        Err(e) => return format!("try_clone: {e}"),
    };
    let _ = reader_sock.set_read_timeout(Some(READ_TIMEOUT));
    let mut reader = BufReader::new(reader_sock);
    let mut sock = sock;
    let mut id: u64 = 1;

    loop {
        let (buttons, analogs, stop, more, q_left) = {
            let mut p = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
            while !p.stop && p.buttons.is_empty() && p.analogs.is_none() {
                p = shared.cv.wait(p).unwrap_or_else(|e| e.into_inner());
            }
            let buttons = take_button_batch(&mut p);
            let analogs = p.analogs.take();
            let more = !p.buttons.is_empty();
            let q_left = p.buttons.len() + usize::from(p.analogs.is_some());
            (buttons, analogs, p.stop, more, q_left)
        };

        if stop {
            let _ = request(
                shared,
                &mut sock,
                &mut reader,
                &mut id,
                "buttons",
                "write_reset",
                vec![],
            );
            let _ = request(
                shared,
                &mut sock,
                &mut reader,
                &mut id,
                "analogs",
                "write_reset",
                vec![],
            );
            return "shutting down".into();
        }

        if !buttons.is_empty() {
            let n = buttons.len();
            let params: Vec<serde_json::Value> = buttons
                .iter()
                .map(|(name, d)| serde_json::json!([name, d]))
                .collect();
            if let Err(e) = request(
                shared,
                &mut sock,
                &mut reader,
                &mut id,
                "buttons",
                "write",
                params,
            ) {
                return e;
            }
            if q_left > 0 {
                log::info!("spiceapi buttons.write n={n} remaining_q={q_left}");
            }
        }
        if let Some((l, r)) = analogs {
            let params = vec![
                serde_json::json!(["Fader-L", to_spice_raw(l)]),
                serde_json::json!(["Fader-R", to_spice_raw(r)]),
            ];
            if let Err(e) = request(
                shared,
                &mut sock,
                &mut reader,
                &mut id,
                "analogs",
                "write",
                params,
            ) {
                return e;
            }
        }
        if more {
            continue;
        }
    }
}

fn take_button_batch(p: &mut Pending) -> Vec<(String, bool)> {
    let mut seen = HashSet::new();
    let mut n = 0;
    for (name, _) in &p.buttons {
        if !seen.insert(name.as_str()) {
            break;
        }
        n += 1;
    }
    p.buttons.drain(..n).collect()
}

fn request(
    shared: &Shared,
    sock: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    id: &mut u64,
    module: &str,
    function: &str,
    params: Vec<serde_json::Value>,
) -> Result<(), String> {
    let req_id = *id;
    let label = format!("{module}.{function} id={req_id}");
    {
        let mut st = shared.stats.lock().unwrap_or_else(|e| e.into_inner());
        st.in_flight_since = Some(Instant::now());
        st.in_flight_label = label.clone();
        st.last_module = format!("{module}.{function}");
    }

    let req = serde_json::json!({
        "id": req_id,
        "module": module,
        "function": function,
        "params": params,
    });
    *id += 1;

    let t0 = Instant::now();
    let mut buf = serde_json::to_vec(&req).map_err(|e| format!("encode: {e}"))?;
    buf.push(0);
    if let Err(e) = sock.write_all(&buf) {
        clear_in_flight(shared);
        bump_fail(shared);
        return Err(format!("write: {e}"));
    }

    let mut resp = Vec::new();
    let read_result = reader.read_until(0, &mut resp);
    let rtt_ms = t0.elapsed().as_secs_f32() * 1000.0;
    clear_in_flight(shared);

    match read_result {
        Ok(0) => {
            bump_fail(shared);
            note_recent(
                shared,
                format!("spiceapi {label} connection closed after {rtt_ms:.0}ms"),
            );
            Err("connection closed".into())
        }
        Ok(_) => {
            record_rtt(shared, &label, rtt_ms, true);
            report_errors(&resp, module, function);
            Ok(())
        }
        Err(e) => {
            bump_fail(shared);
            let kind = if e.kind() == std::io::ErrorKind::TimedOut
                || e.kind() == std::io::ErrorKind::WouldBlock
            {
                "READ TIMEOUT"
            } else {
                "read error"
            };
            note_recent(
                shared,
                format!("spiceapi {label} {kind} after {rtt_ms:.0}ms: {e}"),
            );
            Err(format!("read: {e}"))
        }
    }
}

fn clear_in_flight(shared: &Shared) {
    let mut st = shared.stats.lock().unwrap_or_else(|e| e.into_inner());
    st.in_flight_since = None;
    st.in_flight_label.clear();
}

fn bump_fail(shared: &Shared) {
    shared
        .stats
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .req_fail += 1;
}

fn record_rtt(shared: &Shared, label: &str, rtt_ms: f32, ok: bool) {
    let mut st = shared.stats.lock().unwrap_or_else(|e| e.into_inner());
    if ok {
        st.req_ok += 1;
    } else {
        st.req_fail += 1;
    }
    st.last_rtt_ms = rtt_ms;
    if rtt_ms > st.max_rtt_ms {
        st.max_rtt_ms = rtt_ms;
    }
    let slow = rtt_ms >= SLOW_RTT_MS;
    if slow {
        st.slow_req += 1;
    }
    let line = format!("spiceapi {label} rtt={rtt_ms:.1}ms");
    st.push_recent(line.clone());
    drop(st);
    if slow {
        log::warn!("{line}");
    }
}

fn report_errors(resp: &[u8], module: &str, function: &str) {
    let text = String::from_utf8_lossy(resp.strip_suffix(&[0]).unwrap_or(resp));
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let Some(errs) = v.get("errors").and_then(|e| e.as_array()) else {
        return;
    };
    if errs.is_empty() {
        return;
    }
    let msg = format!("{module}.{function}: {errs:?}");
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let mut g = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = g.get_or_insert_with(HashSet::new);
    if seen.insert(msg.clone()) {
        log::error!("spiceapi {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(edges: &[(&str, bool)]) -> Pending {
        Pending {
            buttons: edges.iter().map(|(n, d)| (n.to_string(), *d)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn spice_raw_maps_the_endpoints_and_center() {
        assert_eq!(to_spice_raw(0.0), 0.5);
        assert_eq!(to_spice_raw(-1.0), 0.0);
        assert_eq!(to_spice_raw(1.0), 1.0);
        assert!((to_spice_raw(0.5) - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn spice_raw_clamps_out_of_range_input() {
        assert_eq!(to_spice_raw(-2.0), 0.0);
        assert_eq!(to_spice_raw(2.0), 1.0);
    }

    #[test]
    fn distinct_buttons_go_out_together() {
        let mut p = pending(&[("Button 1", true), ("Button 2", true), ("Button 3", true)]);
        assert_eq!(take_button_batch(&mut p).len(), 3);
        assert!(p.buttons.is_empty());
    }

    #[test]
    fn a_press_and_release_of_one_button_are_split() {
        let mut p = pending(&[("Button 5", true), ("Button 5", false)]);
        let first = take_button_batch(&mut p);
        assert_eq!(first, vec![("Button 5".to_string(), true)]);
        let second = take_button_batch(&mut p);
        assert_eq!(second, vec![("Button 5".to_string(), false)]);
        assert!(p.buttons.is_empty());
    }

    #[test]
    fn the_split_keeps_everything_before_the_repeat() {
        let mut p = pending(&[("Button 1", true), ("Button 2", true), ("Button 1", false)]);
        assert_eq!(take_button_batch(&mut p).len(), 2);
        assert_eq!(p.buttons.len(), 1);
        assert_eq!(
            take_button_batch(&mut p),
            vec![("Button 1".to_string(), false)]
        );
    }

    #[test]
    fn speaks_the_wire_protocol() {
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let (done_tx, done_rx) = mpsc::channel();

        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut sock = sock;
            let mut msgs = Vec::new();
            for _ in 0..2 {
                let mut raw = Vec::new();
                if reader.read_until(0, &mut raw).unwrap_or(0) == 0 {
                    break;
                }
                assert_eq!(raw.last(), Some(&0), "messages must be NUL-terminated");
                raw.pop();
                let v: serde_json::Value = serde_json::from_slice(&raw).expect("valid JSON");
                let id = v["id"].as_u64().expect("id must be an unsigned int");
                let resp = serde_json::json!({"id": id, "errors": [], "data": []});
                let mut out = serde_json::to_vec(&resp).unwrap();
                out.push(0);
                let _ = sock.write_all(&out);
                msgs.push(v);
            }
            let _ = done_tx.send(());
            drop(reader);
            drop(sock);
            msgs
        });

        {
            let client = Client::start(port);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !client.connected() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for spiceapi connect"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            client.button("Button 7", true);
            client.faders(1.0, -1.0);
            done_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("server should finish both requests");
            let snap = client.debug_snapshot();
            assert!(snap.contains("connected=true"), "{snap}");
            assert!(snap.contains("req_ok="), "{snap}");
        }

        let msgs = server.join().expect("server thread");
        assert_eq!(
            msgs.len(),
            2,
            "expected a buttons write and an analogs write"
        );

        assert_eq!(msgs[0]["module"], "buttons");
        assert_eq!(msgs[0]["function"], "write");
        assert_eq!(msgs[0]["params"], serde_json::json!([["Button 7", true]]));

        assert_eq!(msgs[1]["module"], "analogs");
        assert_eq!(msgs[1]["function"], "write");
        assert_eq!(
            msgs[1]["params"],
            serde_json::json!([["Fader-L", 1.0], ["Fader-R", 0.0]])
        );

        assert!(msgs[1]["id"].as_u64().unwrap() > msgs[0]["id"].as_u64().unwrap());
    }
}
