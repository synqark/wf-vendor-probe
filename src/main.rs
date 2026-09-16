//! wf-vendor-probe — read-only search for JSON API responses in the Warframe client.
//!
//! The client receives vendor stock as JSON from the DE servers and keeps the
//! text in the heap after parsing it. This tool walks the address space with
//! VirtualQueryEx, reads it with ReadProcessMemory, searches for literal key
//! names, and reconstructs the enclosing JSON object.
//!
//! Read-only throughout: no writes, no injection, no hooks, no patching.

mod needles;
mod proc;
mod scan;

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use proc::{Proc, Region};

const USAGE: &str = "\
wf-vendor-probe — read-only JSON probe for the Warframe client

USAGE:
    wf-vendor-probe <command> [options]

COMMANDS:
    watch      Repeat the extract pass on an interval, saving only objects whose
               content has not been seen before. Start this, then walk up to the
               vendor in-game.
    extract    One extract pass: reconstruct and save the JSON around every hit.
    probe      Report needle hits with a text snippet, without reconstructing
               JSON. Fast reconnaissance.
    strings    Dump every printable ASCII run. Use when no needle is known yet.
    regions    Summarise the address space. Sanity check and troubleshooting.
    presets    List the built-in needle sets.

OPTIONS:
    --preset <name>     Built-in needle set, repeatable. Default: vendor
    --needle <text>     Additional literal needle, repeatable
    --pid <n>           Target process id. Default: auto-detect
    --process <prefix>  Executable name prefix. Default: warframe
    --out <dir>         Output directory. Default: ./out
    --interval <secs>   watch: seconds between passes. Default: 3
    --context <n>       probe: snippet bytes on each side. Default: 120
    --max-hits <n>      Cap on reported results per pass. Default: 200
    --min-len <n>       strings: minimum run length. Default: 10
    --filter <text>     strings: keep only runs containing this (case-insensitive)
    --include-exec      Also scan read-only code pages
    --include-wc        Also scan write-combined (GPU staging) regions. These are
                        large, uncached, slow to read, and never hold JSON
    --fast              Skip regions outside 64 KB..64 MB. Rarely needed: a full
                        pass already runs in about a second, and this can miss
                        large blobs such as the account inventory
    --compact           Save JSON exactly as found instead of pretty-printed
    -h, --help          This text

EXAMPLE:
    wf-vendor-probe watch
    (then open the vendor in-game and watch the captures land in ./out)
";

struct Opts {
    cmd: String,
    pid: Option<u32>,
    process: String,
    presets: Vec<String>,
    extra_needles: Vec<String>,
    out: PathBuf,
    interval: u64,
    context: usize,
    max_hits: usize,
    min_len: usize,
    filter: Option<String>,
    include_exec: bool,
    include_wc: bool,
    fast: bool,
    compact: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            cmd: String::new(),
            pid: None,
            process: "warframe".to_string(),
            presets: Vec::new(),
            extra_needles: Vec::new(),
            out: PathBuf::from("out"),
            interval: 3,
            context: 120,
            max_hits: 200,
            min_len: 10,
            filter: None,
            include_exec: false,
            include_wc: false,
            fast: false,
            compact: false,
        }
    }
}

fn main() {
    let opts = match parse_args() {
        Ok(Some(o)) => o,
        Ok(None) => {
            print!("{USAGE}");
            return;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    if let Err(e) = run(&opts) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn parse_args() -> Result<Option<Opts>, String> {
    let mut o = Opts::default();
    let mut args = std::env::args().skip(1);

    let next = |args: &mut dyn Iterator<Item = String>, flag: &str| -> Result<String, String> {
        args.next().ok_or_else(|| format!("{flag} needs a value"))
    };

    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(None),
            "--preset" => o.presets.push(next(&mut args, "--preset")?),
            "--needle" => o.extra_needles.push(next(&mut args, "--needle")?),
            "--process" => o.process = next(&mut args, "--process")?.to_lowercase(),
            "--out" => o.out = PathBuf::from(next(&mut args, "--out")?),
            "--filter" => o.filter = Some(next(&mut args, "--filter")?.to_lowercase()),
            "--include-exec" => o.include_exec = true,
            "--include-wc" => o.include_wc = true,
            "--fast" => o.fast = true,
            "--compact" => o.compact = true,
            "--pid" => {
                let v = next(&mut args, "--pid")?;
                o.pid = Some(v.parse().map_err(|_| format!("--pid: not a number: {v}"))?);
            }
            "--interval" => {
                let v = next(&mut args, "--interval")?;
                o.interval = v.parse().map_err(|_| format!("--interval: not a number: {v}"))?;
            }
            "--context" => {
                let v = next(&mut args, "--context")?;
                o.context = v.parse().map_err(|_| format!("--context: not a number: {v}"))?;
            }
            "--max-hits" => {
                let v = next(&mut args, "--max-hits")?;
                o.max_hits = v.parse().map_err(|_| format!("--max-hits: not a number: {v}"))?;
            }
            "--min-len" => {
                let v = next(&mut args, "--min-len")?;
                o.min_len = v.parse().map_err(|_| format!("--min-len: not a number: {v}"))?;
            }
            other if other.starts_with('-') => return Err(format!("unknown option: {other}")),
            other if o.cmd.is_empty() => o.cmd = other.to_string(),
            other => return Err(format!("unexpected argument: {other}")),
        }
    }

    if o.cmd.is_empty() {
        return Ok(None);
    }
    if o.presets.is_empty() {
        o.presets.push("vendor".to_string());
    }
    Ok(Some(o))
}

fn run(o: &Opts) -> Result<(), String> {
    if o.cmd == "presets" {
        for p in needles::PRESETS {
            println!("{:<12} {}", p.name, p.about);
            for n in p.needles {
                println!("             · {n}");
            }
        }
        return Ok(());
    }

    let pid = match o.pid {
        Some(p) => p,
        None => proc::find_pid(&o.process).ok_or_else(|| {
            format!("no running process starting with \"{}\" — is the game open?", o.process)
        })?,
    };
    let p = Proc::open(pid)?;
    eprintln!("[probe] attached to pid {pid} (read-only)");

    match o.cmd.as_str() {
        "regions" => cmd_regions(&p, o),
        "strings" => cmd_strings(&p, o),
        "probe" => cmd_probe(&p, o),
        "extract" => {
            let mut seen = HashSet::new();
            let n = cmd_extract(&p, o, &mut seen)?;
            eprintln!("[probe] {n} object(s) captured");
            Ok(())
        }
        "watch" => cmd_watch(&p, o),
        other => Err(format!("unknown command: {other}")),
    }
}

/// Needle set for this invocation, resolved from presets plus any extras.
fn resolve_needles(o: &Opts) -> Result<Vec<Vec<u8>>, String> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for name in &o.presets {
        let p = needles::lookup(name)
            .ok_or_else(|| format!("unknown preset \"{name}\" — try: wf-vendor-probe presets"))?;
        for n in p.needles {
            out.push(n.as_bytes().to_vec());
        }
    }
    for n in &o.extra_needles {
        out.push(n.as_bytes().to_vec());
    }
    out.sort();
    out.dedup();
    if out.is_empty() {
        return Err("no needles selected".into());
    }
    Ok(out)
}

fn selected_regions(p: &Proc, o: &Opts) -> Vec<Region> {
    let all = p.regions();
    all.into_iter()
        .filter(|r| {
            let ok = if o.include_exec {
                r.is_data_or_rodata()
            } else {
                r.is_data()
            };
            if !ok {
                return false;
            }
            if !o.include_wc && r.is_write_combine() {
                return false;
            }
            if o.fast {
                r.size >= 64 * 1024 && r.size <= 64 * 1024 * 1024
            } else {
                r.size <= 512 * 1024 * 1024
            }
        })
        .collect()
}

/// Read every selected region in bounded chunks, handing each chunk to `f`.
///
/// Chunks overlap by `overlap` bytes so a needle straddling a chunk boundary is
/// still found. Returning `false` from `f` stops the walk.
fn walk<F>(p: &Proc, regions: &[Region], overlap: usize, mut f: F)
where
    F: FnMut(usize, &[u8]) -> bool,
{
    const CHUNK: usize = 32 * 1024 * 1024;
    let step = CHUNK.saturating_sub(overlap).max(1);

    for r in regions {
        let mut off = 0usize;
        while off < r.size {
            let want = CHUNK.min(r.size - off);
            let base = r.base + off;
            if let Some(data) = p.read(base, want) {
                if !f(base, &data) {
                    return;
                }
            }
            off = off.saturating_add(step);
        }
    }
}

fn cmd_regions(p: &Proc, o: &Opts) -> Result<(), String> {
    let all = p.regions();
    let sel = selected_regions(p, o);
    let total: usize = all.iter().filter(|r| r.is_data()).map(|r| r.size).sum();
    let scanned: usize = sel.iter().map(|r| r.size).sum();
    println!("regions total      : {}", all.len());
    println!("readable data      : {} MB", total / (1024 * 1024));
    println!("selected for scan  : {} regions, {} MB", sel.len(), scanned / (1024 * 1024));
    println!();
    println!("largest selected regions:");
    let mut by_size = sel.clone();
    by_size.sort_by_key(|r| std::cmp::Reverse(r.size));
    for r in by_size.iter().take(15) {
        println!(
            "  0x{:012x}  {:>8} KB  protect=0x{:02x} type=0x{:08x}",
            r.base,
            r.size / 1024,
            r.protect,
            r.kind
        );
    }
    Ok(())
}

fn cmd_probe(p: &Proc, o: &Opts) -> Result<(), String> {
    let needle_set = resolve_needles(o)?;
    let overlap = needle_set.iter().map(|n| n.len()).max().unwrap_or(1);
    let regions = selected_regions(p, o);
    eprintln!(
        "[probe] scanning {} regions with {} needles",
        regions.len(),
        needle_set.len()
    );

    let mut shown = 0usize;
    let mut seen_snippets: HashSet<String> = HashSet::new();

    walk(p, &regions, overlap, |base, data| {
        for (off, ni) in scan::find_all(data, &needle_set) {
            let start = off.saturating_sub(o.context);
            let end = (off + o.context).min(data.len());
            let snippet = scan::render(&data[start..end]);
            // Many hits are the same string reachable from several needles;
            // collapse them so the output stays readable.
            let key: String = snippet.chars().take(60).collect();
            if !seen_snippets.insert(key) {
                continue;
            }
            println!(
                "0x{:012x}  [{}]\n    {}\n",
                base + off,
                String::from_utf8_lossy(&needle_set[ni]),
                snippet
            );
            shown += 1;
            if shown >= o.max_hits {
                return false;
            }
        }
        true
    });

    eprintln!("[probe] {shown} distinct hit(s)");
    Ok(())
}

fn cmd_extract(p: &Proc, o: &Opts, seen: &mut HashSet<u64>) -> Result<usize, String> {
    let needle_set = resolve_needles(o)?;
    let overlap = needle_set.iter().map(|n| n.len()).max().unwrap_or(1);
    let regions = selected_regions(p, o);
    let cfg = scan::ExtractCfg::default();

    std::fs::create_dir_all(&o.out)
        .map_err(|e| format!("cannot create {}: {e}", o.out.display()))?;

    // Absolute ranges already captured this pass. One response produces dozens of
    // needle hits inside a single object; without this every one of them would be
    // re-extracted.
    let mut done: Vec<(usize, usize)> = Vec::new();
    // Hits arrive in address order and neighbouring hits almost always live in
    // the same stitched window, so holding on to the last one avoids re-reading
    // tens of megabytes per hit.
    let mut cached: Option<(Vec<u8>, usize)> = None;
    let mut written = 0usize;

    walk(p, &regions, overlap, |base, data| {
        for (off, _) in scan::find_all(data, &needle_set) {
            let abs = base + off;
            if done.iter().any(|(s, e)| abs >= *s && abs < *e) {
                continue;
            }
            let reusable = matches!(&cached, Some((w, wb)) if abs >= *wb && abs - *wb < w.len());
            if !reusable {
                cached = proc::read_window(p, &regions, abs, 32 << 20, 64 << 20);
            }
            let (wbase, win) = match &cached {
                Some((w, wb)) if abs >= *wb && abs - *wb < w.len() => (*wb, w),
                _ => continue,
            };
            let got = match scan::extract_enclosing(win, abs - wbase, &cfg) {
                Some(g) => g,
                None => continue,
            };

            let obj_addr = wbase + got.start;
            done.push((obj_addr, obj_addr + got.bytes.len()));

            if !seen.insert(got.content_hash()) {
                continue; // identical content already saved
            }
            match save(o, obj_addr, &got) {
                Ok(path) => {
                    println!(
                        "0x{:012x}  {:>9} B  {}\n    -> {}",
                        obj_addr,
                        got.bytes.len(),
                        scan::summarize(&got.value),
                        path.display()
                    );
                    let _ = std::io::stdout().flush();
                    written += 1;
                }
                Err(e) => eprintln!("[probe] save failed at 0x{obj_addr:012x}: {e}"),
            }
            if written >= o.max_hits {
                return false;
            }
        }
        true
    });

    Ok(written)
}

fn cmd_watch(p: &Proc, o: &Opts) -> Result<(), String> {
    let mut seen: HashSet<u64> = HashSet::new();
    eprintln!(
        "[probe] watching every {}s — open the vendor in-game. Ctrl+C to stop.",
        o.interval
    );
    let mut pass = 0u64;
    loop {
        pass += 1;
        let started = SystemTime::now();
        let n = cmd_extract(p, o, &mut seen)?;
        let secs = started.elapsed().map(|d| d.as_secs_f32()).unwrap_or(0.0);
        eprintln!(
            "[probe] pass {pass}: {n} new, {} total, {secs:.1}s",
            seen.len()
        );
        std::thread::sleep(Duration::from_secs(o.interval));
    }
}

fn cmd_strings(p: &Proc, o: &Opts) -> Result<(), String> {
    let regions = selected_regions(p, o);
    eprintln!("[probe] dumping strings from {} regions", regions.len());
    let stdout = std::io::stdout();
    let mut w = std::io::BufWriter::new(stdout.lock());
    let mut shown = 0usize;

    walk(p, &regions, o.min_len, |base, data| {
        let mut run: Option<usize> = None;
        for (i, &b) in data.iter().enumerate() {
            if (0x20..0x7f).contains(&b) {
                if run.is_none() {
                    run = Some(i);
                }
                continue;
            }
            if let Some(s) = run.take() {
                if i - s >= o.min_len {
                    let text = String::from_utf8_lossy(&data[s..i]);
                    if o.filter.as_ref().map_or(true, |f| text.to_lowercase().contains(f)) {
                        let _ = writeln!(w, "0x{:012x}  {}", base + s, text);
                        shown += 1;
                        if shown >= o.max_hits {
                            return false;
                        }
                    }
                }
            }
        }
        true
    });

    let _ = w.flush();
    eprintln!("[probe] {shown} string(s)");
    Ok(())
}

fn save(o: &Opts, addr: usize, got: &scan::Extracted) -> std::io::Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = format!("{ts}_{}_{:012x}.json", scan::label(&got.value), addr);
    let path = o.out.join(name);
    if o.compact {
        std::fs::write(&path, &got.bytes)?;
    } else {
        let pretty = serde_json::to_vec_pretty(&got.value)
            .unwrap_or_else(|_| got.bytes.clone());
        std::fs::write(&path, pretty)?;
    }
    Ok(path)
}
