use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

pub const DEFAULT_PORT: u16 = 1337;

fn to_spice_raw(v: f32) -> f32 {
    (v * 0.5 + 0.5).clamp(0.0, 1.0)
}

#[derive(Default)]
struct Pending {
    buttons: Vec<(String, bool)>,
    analogs: Option<(f32, f32)>,
    stop: bool,
}

struct Shared {
    pending: Mutex<Pending>,
    cv: Condvar,
    connected: AtomicBool,
    status: Mutex<String>,
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
        });
        let worker = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("spiceapi".into())
                .spawn(move || run(shared, port))
                .ok()
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
        drop(p);
        self.shared.cv.notify_one();
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
                log::info!("spiceapi connected on port {port}");
                set_status(&shared, true, format!("connected :{port}"));
                let why = pump(&shared, sock);
                set_status(&shared, false, format!("disconnected ({why})"));
                log::warn!("spiceapi disconnected: {why}");
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
            .wait_timeout(p, std::time::Duration::from_millis(750))
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
    let _ = reader_sock.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let mut reader = BufReader::new(reader_sock);
    let mut sock = sock;
    let mut id: u64 = 1;

    loop {
        let (buttons, analogs, stop, more) = {
            let mut p = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
            while !p.stop && p.buttons.is_empty() && p.analogs.is_none() {
                p = shared.cv.wait(p).unwrap_or_else(|e| e.into_inner());
            }
            let buttons = take_button_batch(&mut p);
            let analogs = p.analogs.take();
            let more = !p.buttons.is_empty();
            (buttons, analogs, p.stop, more)
        };

        if stop {
            let _ = request(
                &mut sock,
                &mut reader,
                &mut id,
                "buttons",
                "write_reset",
                vec![],
            );
            let _ = request(
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
            let params: Vec<serde_json::Value> = buttons
                .iter()
                .map(|(n, d)| serde_json::json!([n, d]))
                .collect();
            if let Err(e) = request(&mut sock, &mut reader, &mut id, "buttons", "write", params) {
                return e;
            }
        }
        if let Some((l, r)) = analogs {
            let params = vec![
                serde_json::json!(["Fader-L", to_spice_raw(l)]),
                serde_json::json!(["Fader-R", to_spice_raw(r)]),
            ];
            if let Err(e) = request(&mut sock, &mut reader, &mut id, "analogs", "write", params) {
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
    sock: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    id: &mut u64,
    module: &str,
    function: &str,
    params: Vec<serde_json::Value>,
) -> Result<(), String> {
    let req = serde_json::json!({
        "id": *id,
        "module": module,
        "function": function,
        "params": params,
    });
    *id += 1;

    let mut buf = serde_json::to_vec(&req).map_err(|e| format!("encode: {e}"))?;
    buf.push(0);
    sock.write_all(&buf).map_err(|e| format!("write: {e}"))?;

    let mut resp = Vec::new();
    match reader.read_until(0, &mut resp) {
        Ok(0) => return Err("connection closed".into()),
        Ok(_) => {}
        Err(e) => return Err(format!("read: {e}")),
    }
    report_errors(&resp, module, function);
    Ok(())
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

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();

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
            drop(reader);
            drop(sock);
            msgs
        });

        {
            let client = Client::start(port);
            client.button("Button 7", true);
            client.faders(1.0, -1.0);
            std::thread::sleep(std::time::Duration::from_millis(400));
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
