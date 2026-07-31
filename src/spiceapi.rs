use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

pub const DEFAULT_PORT: u16 = 1337;

const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const RECONNECT_WAIT: std::time::Duration = std::time::Duration::from_millis(750);
const SLOW_RTT_MS: f32 = 50.0;
const RESYNC_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
const RECENT_CAP: usize = 48;

fn to_spice_raw(v: f32) -> f32 {
    (v * 0.5 + 0.5).clamp(0.0, 1.0)
}

#[derive(Default)]
struct Pending {
    buttons: Vec<(&'static str, bool)>,
    analogs: Option<(f32, f32)>,
    stop: bool,
    known: std::collections::BTreeMap<&'static str, bool>,
    known_analogs: (f32, f32),
    dropped_offline: u64,
}

impl Pending {
    fn discard_stale(&mut self) -> (usize, u64) {
        let stale = self.buttons.len();
        self.buttons.clear();
        (stale, std::mem::take(&mut self.dropped_offline))
    }
}

#[derive(Default)]
struct IoStats {
    req_ok: u64,
    req_fail: u64,
    slow_req: u64,
    last_rtt_ms: f32,
    max_rtt_ms: f32,
    last_module: String,
    resyncs: u64,
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
    status_epoch: AtomicU64,
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
            status_epoch: AtomicU64::new(1),
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

    pub fn button(&self, name: &'static str, down: bool) {
        let mut p = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        p.known.insert(name, down);
        if !self.shared.connected.load(Ordering::Relaxed) {
            p.dropped_offline += 1;
            let n = p.dropped_offline;
            drop(p);
            if n == 1 || n == 25 || n.is_multiple_of(250) {
                log::warn!(
                    "spiceapi offline: {name} down={down} not queued (#{n}) — \
                     the link resyncs from the held picture when it returns"
                );
            }
            return;
        }
        p.buttons.push((name, down));
        let q = p.buttons.len();
        drop(p);
        self.shared.cv.notify_one();
        if q == 20 || q == 50 || q == 100 {
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
        p.known_analogs = (left, right);
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

    pub fn status_epoch(&self) -> u64 {
        self.shared.status_epoch.load(Ordering::Acquire)
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
             req_ok={} req_fail={} slow>={SLOW_RTT_MS}ms={} resyncs={} last_rtt={:.1}ms \
             max_rtt={:.1}ms last_module={} in_flight=[{}] recent=[\n  {}\n]",
            self.port,
            connected,
            status,
            q_btn,
            q_analog,
            stop,
            st.req_ok,
            st.req_fail,
            st.slow_req,
            st.resyncs,
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
    shared.status_epoch.fetch_add(1, Ordering::Release);
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
                let (stale, offline) = shared
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .discard_stale();
                if stale > 0 || offline > 0 {
                    log::info!(
                        "spiceapi fresh link: discarded {stale} stale edge(s), \
                         {offline} pressed while offline — resyncing the held picture instead"
                    );
                }
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

    let mut resync_due = true;

    loop {
        let (batches, analogs, stop) = {
            let mut p = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
            let mut idle = false;
            while !p.stop && p.buttons.is_empty() && p.analogs.is_none() && !resync_due {
                let (next, timeout) = shared
                    .cv
                    .wait_timeout(p, RESYNC_INTERVAL)
                    .unwrap_or_else(|e| e.into_inner());
                p = next;
                if timeout.timed_out() {
                    idle = true;
                    break;
                }
            }
            if idle {
                resync_due = true;
            }
            let batches = drain_button_batches(&mut p);
            let analogs = p.analogs.take();
            (batches, analogs, p.stop)
        };

        if resync_due && batches.is_empty() && !stop {
            resync_due = false;
            if let Err(e) = resync(shared, &mut sock, &mut reader, &mut id) {
                return e;
            }
        }

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

        let mut reqs: Vec<(&'static str, &'static str, Vec<serde_json::Value>)> =
            Vec::with_capacity(batches.len() + 1);
        for batch in &batches {
            let params = batch
                .iter()
                .map(|(name, d)| serde_json::json!([name, d]))
                .collect();
            reqs.push(("buttons", "write", params));
        }
        if let Some((l, r)) = analogs {
            reqs.push((
                "analogs",
                "write",
                vec![
                    serde_json::json!(["Fader-L", to_spice_raw(l)]),
                    serde_json::json!(["Fader-R", to_spice_raw(r)]),
                ],
            ));
        }
        if let Err(e) = send_pipeline(shared, &mut sock, &mut reader, &mut id, &reqs) {
            return e;
        }
    }
}

fn drain_button_batches(p: &mut Pending) -> Vec<Vec<(&'static str, bool)>> {
    let mut out = Vec::new();
    while !p.buttons.is_empty() {
        out.push(take_button_batch(p));
    }
    out
}

fn send_pipeline(
    shared: &Shared,
    sock: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    id: &mut u64,
    reqs: &[(&'static str, &'static str, Vec<serde_json::Value>)],
) -> Result<(), String> {
    if reqs.is_empty() {
        return Ok(());
    }
    let t0 = Instant::now();
    let mut labels: Vec<(String, &'static str, &'static str)> = Vec::with_capacity(reqs.len());

    for (module, function, params) in reqs {
        let req_id = *id;
        *id += 1;
        let req = serde_json::json!({
            "id": req_id,
            "module": module,
            "function": function,
            "params": params,
        });
        let mut buf = serde_json::to_vec(&req).map_err(|e| format!("encode: {e}"))?;
        buf.push(0);
        if let Err(e) = sock.write_all(&buf) {
            clear_in_flight(shared);
            bump_fail(shared);
            return Err(format!("write: {e}"));
        }
        labels.push((format!("{module}.{function} id={req_id}"), module, function));
    }

    {
        let mut st = shared.stats.lock().unwrap_or_else(|e| e.into_inner());
        st.in_flight_since = Some(t0);
        if let Some((_, module, function)) = labels.last() {
            st.last_module = format!("{module}.{function}");
        }
    }

    for (label, module, function) in &labels {
        {
            let mut st = shared.stats.lock().unwrap_or_else(|e| e.into_inner());
            st.in_flight_label = label.clone();
        }
        let mut resp = Vec::new();
        let read_result = reader.read_until(0, &mut resp);
        let rtt_ms = t0.elapsed().as_secs_f32() * 1000.0;
        match read_result {
            Ok(0) => {
                clear_in_flight(shared);
                bump_fail(shared);
                note_recent(
                    shared,
                    format!("spiceapi {label} connection closed after {rtt_ms:.0}ms"),
                );
                return Err("connection closed".into());
            }
            Ok(_) => {
                record_rtt(shared, label, rtt_ms, true);
                report_errors(&resp, module, function);
            }
            Err(e) => {
                clear_in_flight(shared);
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
                return Err(format!("read: {e}"));
            }
        }
    }
    clear_in_flight(shared);
    Ok(())
}

fn resync(
    shared: &Shared,
    sock: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    id: &mut u64,
) -> Result<(), String> {
    let (snapshot, (l, r)) = {
        let p = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
        let buttons: Vec<(&'static str, bool)> = p.known.iter().map(|(n, d)| (*n, *d)).collect();
        (buttons, p.known_analogs)
    };
    let held: Vec<&str> = snapshot
        .iter()
        .filter(|(_, d)| *d)
        .map(|(n, _)| *n)
        .collect();
    let n = {
        let mut st = shared.stats.lock().unwrap_or_else(|e| e.into_inner());
        st.resyncs += 1;
        st.resyncs
    };
    if n == 1 || !held.is_empty() {
        log::info!(
            "spiceapi resync #{n}: {} buttons re-asserted, held={held:?}, faders=({l:.3}, {r:.3})",
            snapshot.len()
        );
    }

    let mut reqs: Vec<(&'static str, &'static str, Vec<serde_json::Value>)> = Vec::with_capacity(2);
    if !snapshot.is_empty() {
        reqs.push((
            "buttons",
            "write",
            snapshot
                .iter()
                .map(|(name, d)| serde_json::json!([name, d]))
                .collect(),
        ));
    }
    reqs.push((
        "analogs",
        "write",
        vec![
            serde_json::json!(["Fader-L", to_spice_raw(l)]),
            serde_json::json!(["Fader-R", to_spice_raw(r)]),
        ],
    ));
    send_pipeline(shared, sock, reader, id, &reqs)
}

fn take_button_batch(p: &mut Pending) -> Vec<(&'static str, bool)> {
    let mut seen = HashSet::new();
    let mut n = 0;
    for (name, _) in &p.buttons {
        if !seen.insert(*name) {
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

    fn pending(edges: &[(&'static str, bool)]) -> Pending {
        Pending {
            buttons: edges.to_vec(),
            ..Default::default()
        }
    }

    fn serve(sock: TcpStream, tx: std::sync::mpsc::Sender<serde_json::Value>) {
        let mut reader = BufReader::new(sock.try_clone().expect("clone"));
        let mut sock = sock;
        loop {
            let mut raw = Vec::new();
            if reader.read_until(0, &mut raw).unwrap_or(0) == 0 {
                return;
            }
            assert_eq!(raw.last(), Some(&0), "messages must be NUL-terminated");
            raw.pop();
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else {
                return;
            };
            let resp = serde_json::json!({"id": v["id"], "errors": [], "data": []});
            let mut out = serde_json::to_vec(&resp).expect("encode");
            out.push(0);
            if sock.write_all(&out).is_err() || tx.send(v).is_err() {
                return;
            }
        }
    }

    fn dead_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn discard_stale_drops_queued_edges_but_keeps_the_known_picture() {
        let mut p = pending(&[("Button 1", true), ("Button 1", false), ("Button 2", true)]);
        p.known.insert("Button 1", false);
        p.known.insert("Button 2", true);
        p.dropped_offline = 7;

        assert_eq!(p.discard_stale(), (3, 7));
        assert!(p.buttons.is_empty(), "stale edges must not be replayed");
        assert_eq!(p.dropped_offline, 0, "the offline count is consumed");
        assert_eq!(p.known.get("Button 1"), Some(&false));
        assert_eq!(p.known.get("Button 2"), Some(&true));
        assert_eq!(p.discard_stale(), (0, 0), "and it is idempotent");
    }

    #[test]
    fn edges_pressed_while_offline_are_dropped_but_still_remembered() {
        let client = Client::start(dead_port());
        assert!(!client.connected(), "nothing is listening");

        client.button("Button 4", true);
        client.button("Button 4", false);
        client.button("Button 9", true);

        {
            let p = client
                .shared
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            assert!(
                p.buttons.is_empty(),
                "offline edges must not queue up to be replayed at the next game"
            );
            assert_eq!(p.dropped_offline, 3);
            assert_eq!(p.known.get("Button 4"), Some(&false));
            assert_eq!(p.known.get("Button 9"), Some(&true));
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
        assert_eq!(first, vec![("Button 5", true)]);
        let second = take_button_batch(&mut p);
        assert_eq!(second, vec![("Button 5", false)]);
        assert!(p.buttons.is_empty());
    }

    #[test]
    fn the_split_keeps_everything_before_the_repeat() {
        let mut p = pending(&[("Button 1", true), ("Button 2", true), ("Button 1", false)]);
        assert_eq!(take_button_batch(&mut p).len(), 2);
        assert_eq!(p.buttons.len(), 1);
        assert_eq!(take_button_batch(&mut p), vec![("Button 1", false)]);
    }

    #[test]
    fn draining_splits_repeats_into_ordered_batches() {
        let mut p = pending(&[
            ("Button 1", true),
            ("Button 2", true),
            ("Button 1", false),
            ("Button 3", true),
        ]);
        let batches = drain_button_batches(&mut p);
        assert_eq!(
            batches,
            vec![
                vec![("Button 1", true), ("Button 2", true)],
                vec![("Button 1", false), ("Button 3", true)],
            ]
        );
        assert!(p.buttons.is_empty());
    }

    #[test]
    fn a_pipeline_writes_every_request_before_reading_any_reply() {
        use std::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut sock = sock;
            let mut ids = Vec::new();
            for _ in 0..3 {
                let mut raw = Vec::new();
                assert!(
                    reader.read_until(0, &mut raw).unwrap() > 0,
                    "request {}",
                    ids.len()
                );
                raw.pop();
                let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
                ids.push(v["id"].as_u64().unwrap());
            }
            for id in ids {
                let resp = serde_json::json!({"id": id, "errors": [], "data": []});
                let mut out = serde_json::to_vec(&resp).unwrap();
                out.push(0);
                sock.write_all(&out).unwrap();
            }
        });

        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending::default()),
            cv: Condvar::new(),
            connected: AtomicBool::new(true),
            status: Mutex::new(String::new()),
            status_epoch: AtomicU64::new(1),
            stats: Mutex::new(IoStats::default()),
        });

        let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let reader_sock = sock.try_clone().unwrap();
        reader_sock
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(reader_sock);
        let mut id = 1u64;

        let reqs = vec![
            (
                "buttons",
                "write",
                vec![serde_json::json!(["Button 1", true])],
            ),
            (
                "buttons",
                "write",
                vec![serde_json::json!(["Button 1", false])],
            ),
            (
                "analogs",
                "write",
                vec![serde_json::json!(["Fader-L", 0.5])],
            ),
        ];
        send_pipeline(&shared, &mut sock, &mut reader, &mut id, &reqs).expect("pipeline ok");
        assert_eq!(id, 4, "three request ids consumed");
        server.join().expect("server thread");
    }

    #[test]
    fn an_idle_link_re_asserts_the_full_button_picture() {
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();

        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().expect("accept");
            serve(sock, tx);
        });

        let client = Client::start(port);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !client.connected() {
            assert!(std::time::Instant::now() < deadline, "connect timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        client.button("Button 3", true);
        client.button("Button 7", true);
        client.button("Button 7", false);

        let found = std::iter::from_fn(|| {
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .ok()
                .map(|v| v["params"].clone())
        })
        .take(12)
        .any(|params| {
            params.as_array().is_some_and(|a| {
                a.contains(&serde_json::json!(["Button 3", true]))
                    && a.contains(&serde_json::json!(["Button 7", false]))
            })
        });
        assert!(
            found,
            "expected an unprompted resync carrying Button 3 down"
        );

        drop(client);
        let _ = server.join();
    }

    #[test]
    fn speaks_the_wire_protocol() {
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();

        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().expect("accept");
            serve(sock, tx);
        });

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

        let msgs: Vec<serde_json::Value> =
            std::iter::from_fn(|| rx.recv_timeout(std::time::Duration::from_secs(5)).ok())
                .take(8)
                .collect();

        let button = msgs
            .iter()
            .find(|v| {
                v["module"] == "buttons" && v["params"] == serde_json::json!([["Button 7", true]])
            })
            .unwrap_or_else(|| panic!("expected a buttons.write for Button 7: {msgs:#?}"));
        assert_eq!(button["function"], "write");

        let analog = msgs
            .iter()
            .find(|v| {
                v["module"] == "analogs"
                    && v["params"] == serde_json::json!([["Fader-L", 1.0], ["Fader-R", 0.0]])
            })
            .unwrap_or_else(|| panic!("expected the ±1 pair mapped into 0..1: {msgs:#?}"));
        assert_eq!(analog["function"], "write");

        let ids: Vec<u64> = msgs
            .iter()
            .map(|v| v["id"].as_u64().expect("id must be an unsigned int"))
            .collect();
        assert!(
            ids.windows(2).all(|w| w[1] > w[0]),
            "ids must rise: {ids:?}"
        );

        let snap = client.debug_snapshot();
        assert!(snap.contains("connected=true"), "{snap}");
        assert!(snap.contains("req_ok="), "{snap}");

        drop(client);
        let _ = server.join();
    }

    #[test]
    fn a_fresh_link_centres_the_faders_before_any_input() {
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();

        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().expect("accept");
            serve(sock, tx);
        });

        let client = Client::start(port);
        let found = std::iter::from_fn(|| rx.recv_timeout(std::time::Duration::from_secs(5)).ok())
            .take(4)
            .any(|v| {
                v["module"] == "analogs"
                    && v["function"] == "write"
                    && v["params"] == serde_json::json!([["Fader-L", 0.5], ["Fader-R", 0.5]])
            });
        assert!(found, "a fresh link must state the faders unprompted");

        drop(client);
        let _ = server.join();
    }
}
