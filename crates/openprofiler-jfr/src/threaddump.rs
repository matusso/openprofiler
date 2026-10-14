//! Parser for the jstack-style text in `jdk.ThreadDump` events.

use openprofiler_model::{DumpedThread, Nanos, ThreadDump};

pub(crate) fn parse(text: &str, time: Nanos) -> ThreadDump {
    let mut dump = ThreadDump { time, label: "JFR thread dump".into(), threads: vec![] };
    let mut cur: Option<DumpedThread> = None;
    for line in text.lines() {
        if line.starts_with('"') {
            if let Some(t) = cur.take() {
                dump.threads.push(t);
            }
            cur = Some(parse_header(line));
            continue;
        }
        let Some(t) = cur.as_mut() else { continue };
        let l = line.trim();
        if let Some(state) = l.strip_prefix("java.lang.Thread.State:") {
            t.state = state.trim().to_owned();
        } else if let Some(frame) = l.strip_prefix("at ") {
            t.frames.push(frame.to_owned());
        } else if let Some(lock) = l.strip_prefix("- locked ") {
            t.locked.push(lock.to_owned());
        } else if let Some(w) = ["- waiting on ", "- parking to wait for ", "- waiting to lock "]
            .iter()
            .find_map(|p| l.strip_prefix(p))
        {
            t.waiting_on = Some(w.trim().to_owned());
        } else if l.is_empty() && t.frames.is_empty() && !t.state.is_empty() {
            // Blank line after a frameless thread ends it.
            dump.threads.push(cur.take().unwrap());
        }
    }
    if let Some(t) = cur {
        dump.threads.push(t);
    }
    dump
}

fn parse_header(line: &str) -> DumpedThread {
    // "name with \"quotes\"" #12 [4567] daemon prio=5 ... tid=0x... nid=... state  [0x...]
    let rest = &line[1..];
    let (name, tail) = match rest.rfind("\" ") {
        Some(i) => (&rest[..i], &rest[i + 2..]),
        None => (rest.trim_end_matches('"'), ""),
    };
    let java_id = tail
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix('#'))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    DumpedThread {
        name: name.to_owned(),
        java_id,
        state: String::new(),
        is_virtual: tail.contains("virtual"),
        frames: vec![],
        locked: vec![],
        waiting_on: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jstack_text() {
        let text = r#"2026-10-13 21:00:00
Full thread dump Java HotSpot(TM) 64-Bit Server VM (27+35 mixed mode):

"main" #1 [8963] prio=5 os_prio=31 cpu=120.00ms elapsed=1.20s tid=0x000 nid=8963 waiting on condition  [0x0]
   java.lang.Thread.State: TIMED_WAITING (sleeping)
	at java.lang.Thread.sleep0(java.base@27/Native Method)
	at app.Main.main(Main.java:10)
	- locked <0x1> (a java.lang.Object)

"VM Thread" os_prio=31 cpu=1.00ms elapsed=1.20s tid=0x0 nid=1 runnable

"Worker "1"" #22 daemon prio=5 tid=0x0 nid=2 waiting on condition
   java.lang.Thread.State: WAITING (parking)
	at jdk.internal.misc.Unsafe.park(java.base@27/Native Method)
	- parking to wait for  <0x2> (a java.util.concurrent.locks.ReentrantLock$NonfairSync)
"#;
        let d = parse(text, 5);
        assert_eq!(d.threads.len(), 3);
        assert_eq!(d.threads[0].name, "main");
        assert_eq!(d.threads[0].java_id, 1);
        assert_eq!(d.threads[0].frames.len(), 2);
        assert_eq!(d.threads[0].locked.len(), 1);
        assert!(d.threads[0].state.starts_with("TIMED_WAITING"));
        assert_eq!(d.threads[1].name, "VM Thread");
        assert_eq!(d.threads[2].name, "Worker \"1\"");
        assert_eq!(d.threads[2].java_id, 22);
        assert!(d.threads[2].waiting_on.as_deref().unwrap().contains("ReentrantLock"));
    }
}
