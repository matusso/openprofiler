//! Checks an `.oplink` recording against expectations, one per line (`|`-separated, `#` comments):
//!
//! ```text
//! call    | <probe> | <payload>                         a call whose payload contains <payload>
//! nocall  | <probe> | <payload>                         no such call
//! count   | <probe> | <payload> | <n>                   exactly n calls (n+: at least n)
//! event   | <probe> | <payload> | <key>~<text> ...      an event with these fields containing <text>
//!                                                       (keys: detail, context, error;
//!                                                       ms~<n>: lasted at least n ms)
//! every   | <probe> | <payload> | <key>~<text> ...      like event, for every matching event (at least one)
//! stack   | <probe> | <payload> | <frame>               a call whose stack has a frame containing <frame>
//! control | <probe> | <description> | open|close|gc     a control event (gc: collected without close)
//! sample  | <kind>  | <frame>                           a sample (cpu, wall, alloc, calls, ...) with that frame
//! ```
//!
//! Payloads match case-insensitively (ORMs differ in the case of their SQL); other fields exactly.
//! Prints every failed expectation and the probe data it was checked against, and exits with 1.
//! Usage: cargo run -p openprofiler-link --example oplink_check -- <file.oplink> <expect.txt>

use openprofiler_model::{ControlEventKind, Profile, SampleKind, StackId};

fn frames(p: &Profile, stack: StackId) -> Vec<String> {
    let mut f = vec![];
    p.stacks.frames_root_first(stack, &mut f);
    f.iter().map(|&f| p.symbols.method_label(p.symbols.frame(f).method)).collect()
}

fn has(payload: &str, needle: &str) -> bool {
    payload.to_lowercase().contains(&needle.to_lowercase())
}

fn probe(p: &Profile, id: &str) -> Option<u32> {
    p.probes.probes.iter().position(|x| x.id == id).map(|i| i as u32)
}

fn kind(name: &str) -> Option<SampleKind> {
    Some(match name {
        "cpu" => SampleKind::Cpu,
        "wall" => SampleKind::Wall,
        "alloc" => SampleKind::Alloc,
        "monitor" => SampleKind::Monitor,
        "wait" => SampleKind::Wait,
        "io" => SampleKind::Io,
        "exception" => SampleKind::Exception,
        "calls" => SampleKind::Calls,
        _ => return None,
    })
}

fn check(p: &Profile, f: &[&str]) -> Result<(), String> {
    let field = |i: usize| f.get(i).copied().ok_or_else(|| format!("missing field {i}"));
    match f[0] {
        "count" => {
            let id = probe(p, field(1)?);
            let needle = field(2)?;
            let want = field(3)?;
            let n: u64 = p
                .probes
                .calls
                .iter()
                .filter(|c| Some(c.probe) == id && has(p.symbols.str(c.payload), needle))
                .map(|c| c.count as u64)
                .sum();
            let ok = match want.strip_suffix('+') {
                Some(min) => n >= min.parse::<u64>().map_err(|e| e.to_string())?,
                None => n == want.parse::<u64>().map_err(|e| e.to_string())?,
            };
            if ok { Ok(()) } else { Err(format!("{n} calls")) }
        }
        "call" | "nocall" | "stack" => {
            let id = probe(p, field(1)?);
            let needle = field(2)?;
            let mut calls = p
                .probes
                .calls
                .iter()
                .filter(|c| Some(c.probe) == id && has(p.symbols.str(c.payload), needle));
            match f[0] {
                "call" => calls.next().map(|_| ()).ok_or_else(|| "no such call".into()),
                "nocall" => {
                    calls.next().map_or(Ok(()), |c| Err(format!("found {}", p.symbols.str(c.payload))))
                }
                _ => {
                    let frame = field(3)?;
                    let stacks: Vec<_> = calls.map(|c| frames(p, c.stack)).collect();
                    if stacks.iter().any(|s| s.iter().any(|l| l.contains(frame))) {
                        Ok(())
                    } else {
                        Err(format!("stacks: {stacks:#?}"))
                    }
                }
            }
        }
        "event" | "every" => {
            let id = probe(p, field(1)?);
            let needle = field(2)?;
            let wanted: Vec<(&str, &str)> = f[3..]
                .iter()
                .map(|kv| kv.split_once('~').ok_or_else(|| format!("bad {kv}")))
                .collect::<Result<_, _>>()?;
            let events: Vec<_> = p
                .probes
                .events
                .iter()
                .filter(|e| Some(e.probe) == id && has(p.symbols.str(e.payload), needle))
                .collect();
            let matches = |e: &&openprofiler_model::ProbeEvent| {
                wanted.iter().all(|(k, v)| {
                    if *k == "ms" {
                        return v.parse::<i64>().is_ok_and(|ms| e.duration >= ms * 1_000_000);
                    }
                    let have = match *k {
                        "detail" => &e.detail,
                        "context" => &e.context,
                        "error" => &e.error,
                        _ => return false,
                    };
                    have.as_deref().is_some_and(|h| h.contains(v))
                })
            };
            let ok = if f[0] == "every" {
                !events.is_empty() && events.iter().all(&matches)
            } else {
                events.iter().any(&matches)
            };
            if ok {
                Ok(())
            } else {
                let seen: Vec<_> = events
                    .iter()
                    .filter(|e| f[0] == "event" || !matches(e))
                    .take(10)
                    .map(|e| {
                        format!(
                            "{:.1} ms detail={:?} context={:?} error={:?}",
                            e.duration as f64 / 1e6,
                            e.detail,
                            e.context,
                            e.error
                        )
                    })
                    .collect();
                Err(format!("{} matching events: {seen:#?}", events.len()))
            }
        }
        "control" => {
            let id = probe(p, field(1)?);
            let needle = field(2)?;
            let state = field(3)?;
            let opened: Vec<u64> = p
                .probes
                .controls
                .iter()
                .filter(|c| {
                    Some(c.probe) == id && c.kind == ControlEventKind::Open && c.description.contains(needle)
                })
                .map(|c| c.id)
                .collect();
            let closes = |gc: bool| {
                p.probes.controls.iter().any(|c| {
                    Some(c.probe) == id
                        && c.kind == ControlEventKind::Close
                        && opened.contains(&c.id)
                        && (c.description == "garbage collected without close") == gc
                })
            };
            let ok = match state {
                "open" => !opened.is_empty(),
                "close" => closes(false),
                "gc" => closes(true),
                _ => return Err(format!("bad state {state}")),
            };
            if ok { Ok(()) } else { Err(format!("{} opened", opened.len())) }
        }
        "sample" => {
            let k = kind(field(1)?).ok_or("bad kind")?;
            let needle = field(2)?;
            let Some(set) = p.samples.get(&k) else { return Err("no samples".into()) };
            if (0..set.len()).any(|i| frames(p, set.stack[i]).iter().any(|l| l.contains(needle))) {
                Ok(())
            } else {
                Err(format!("{} rows", set.len()))
            }
        }
        other => Err(format!("unknown expectation {other}")),
    }
}

fn dump(p: &Profile) {
    for (i, info) in p.probes.probes.iter().enumerate() {
        let mut payloads: Vec<&str> = p
            .probes
            .calls
            .iter()
            .filter(|c| c.probe as usize == i)
            .map(|c| p.symbols.str(c.payload))
            .collect();
        payloads.sort_unstable();
        payloads.dedup();
        if payloads.is_empty() {
            continue;
        }
        eprintln!("  probe {}:", info.id);
        for pl in payloads.iter().take(40) {
            eprintln!("    {pl}");
        }
    }
    for c in p.probes.controls.iter().take(40) {
        let id = &p.probes.probes[c.probe as usize].id;
        eprintln!("  control {id} #{} {:?} {}", c.id, c.kind, c.description);
    }
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(rec), Some(expect)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: oplink_check <file.oplink> <expect.txt>");
    };
    let p = openprofiler_link::replay_recording(std::path::Path::new(&rec))?;
    let mut failed = 0;
    let mut total = 0;
    for line in std::fs::read_to_string(&expect)?.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('|').map(str::trim).collect();
        total += 1;
        if let Err(e) = check(&p, &f) {
            failed += 1;
            eprintln!("FAIL {line}\n     {}", e.replace('\n', "\n     "));
        }
    }
    if failed > 0 {
        eprintln!("recorded probe data:");
        dump(&p);
        eprintln!("{failed} of {total} expectations failed");
        std::process::exit(1);
    }
    println!("ok: {total} expectations");
    Ok(())
}
