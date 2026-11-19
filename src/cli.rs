//! Headless CLI + scheduled scans. GUI starts when no arguments are given.
use crate::scan::*;
use crate::undo::{Journal, data_dir};
use std::path::PathBuf;

const USAGE: &str = "\
usage:
  tidytwin                                    start the GUI
  tidytwin scan [OPTIONS] PATH...             find duplicates, print report
  tidytwin schedule hourly|daily|weekly [OPTIONS] PATH...
                                             run the scan periodically; reports go to the
                                             data dir and can be opened in the GUI
  tidytwin unschedule
  tidytwin undo-last                          undo the most recent action batch

options:
  --match LIST     comma list of name,size,mtime,content   (default size,content)
  --min-size N     bytes (default 1)
  --exclude S      skip paths containing S (repeatable)
  --hidden         include dot-files
  --cross          only groups spanning 2+ roots (directory compare)
  --largest N      also list N largest files (default 50)
  --format F       json | csv                              (default json)
  --out FILE       write report to FILE instead of stdout
  --report         write report into <data dir>/reports/
";

pub fn reports_dir() -> PathBuf {
    data_dir().join("reports")
}

pub fn run(args: &[String]) -> i32 {
    let r = match args.first().map(String::as_str) {
        Some("scan") => scan(&args[1..]),
        Some("schedule") => args.get(1).ok_or("missing frequency".to_string()).and_then(|f| schedule(f, &args[2..])).map(|m| println!("{m}")),
        Some("unschedule") => unschedule().map(|m| println!("{m}")),
        Some("undo-last") => undo_last(),
        _ => Err(USAGE.into()),
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            2
        }
    }
}

struct Args {
    opts: ScanOpts,
    crit: Criteria,
    cross: bool,
    largest: usize,
    csv: bool,
    out: Option<PathBuf>,
    report: bool,
}

fn parse(a: &[String]) -> Result<Args, String> {
    let mut r = Args {
        opts: ScanOpts { roots: vec![], min_size: 1, exclude: vec![], follow_hidden: false },
        crit: Criteria::default(),
        cross: false,
        largest: 50,
        csv: false,
        out: None,
        report: false,
    };
    let mut it = a.iter();
    while let Some(x) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{x} needs a value"));
        match x.as_str() {
            "--match" => {
                let v = val()?;
                let has = |s| v.split(',').any(|p| p == s);
                r.crit = Criteria { name: has("name"), size: has("size"), mtime: has("mtime"), content: has("content") };
            }
            "--min-size" => r.opts.min_size = val()?.parse().map_err(|e| format!("--min-size: {e}"))?,
            "--exclude" => r.opts.exclude.push(val()?),
            "--hidden" => r.opts.follow_hidden = true,
            "--cross" => r.cross = true,
            "--largest" => r.largest = val()?.parse().map_err(|e| format!("--largest: {e}"))?,
            "--format" => r.csv = val()? == "csv",
            "--out" => r.out = Some(val()?.into()),
            "--report" => r.report = true,
            s if s.starts_with("--") => return Err(format!("unknown option {s}\n{USAGE}")),
            p => r.opts.roots.push(std::fs::canonicalize(p).map_err(|e| format!("{p}: {e}"))?),
        }
    }
    if r.opts.roots.is_empty() {
        return Err(format!("no paths given\n{USAGE}"));
    }
    Ok(r)
}

fn scan(a: &[String]) -> Result<(), String> {
    let a = parse(a)?;
    let prog = Progress::default();
    let cache = HashCache::load(data_dir().join("hashcache.jsonl"));
    let t = std::time::Instant::now();
    let files = collect(&a.opts, &prog);
    cache.prune(&a.opts.roots, &files);
    let report = Report {
        created: std::time::SystemTime::now(),
        roots: a.opts.roots.clone(),
        scanned: files.len(),
        largest: largest(&files, a.largest),
        groups: find_dups(files, a.crit, a.cross, &prog, &cache),
        secs: t.elapsed().as_secs_f32(),
    };
    cache.save().map_err(|e| format!("hash cache: {e}"))?;
    let body = if a.csv { to_csv(&report.groups) } else { serde_json::to_string_pretty(&report).unwrap() };
    let out = a.out.or_else(|| {
        a.report.then(|| {
            let ts = report.created.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            reports_dir().join(format!("{ts}.{}", if a.csv { "csv" } else { "json" }))
        })
    });
    match out {
        Some(p) => {
            std::fs::create_dir_all(p.parent().unwrap_or(".".as_ref())).map_err(|e| e.to_string())?;
            std::fs::write(&p, body).map_err(|e| format!("{}: {e}", p.display()))?;
            eprintln!("{} groups, report: {}", report.groups.len(), p.display());
        }
        None => println!("{body}"),
    }
    Ok(())
}

fn undo_last() -> Result<(), String> {
    let mut j = Journal::open(data_dir());
    let b = j.entries.iter().filter(|e| !e.undone).map(|e| e.batch).max().ok_or("nothing to undo")?;
    let errs = j.undo_batch(b);
    if errs.is_empty() { Ok(()) } else { Err(errs.join("\n")) }
}

/// Install a periodic `tidytwin scan --report ARGS`.
pub fn schedule(freq: &str, args: &[String]) -> Result<String, String> {
    if !["hourly", "daily", "weekly"].contains(&freq) {
        return Err(format!("frequency must be hourly, daily or weekly\n{USAGE}"));
    }
    parse(args)?; // validate now, not at 3am
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = vec![exe.to_string_lossy().into_owned(), "scan".into(), "--report".into()];
    cmd.extend(args.iter().cloned());
    // A newline would end the ExecStart= line and start a new unit directive (or crontab entry).
    if cmd.iter().any(|s| s.contains(['\n', '\r'])) {
        return Err("paths/arguments containing newlines can't be scheduled".into());
    }
    if cfg!(target_os = "linux") {
        let dir = systemd_user_dir()?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let exec = cmd.iter().map(|s| systemd_quote(s)).collect::<Vec<_>>().join(" ");
        std::fs::write(dir.join("tidytwin-scan.service"), format!("[Unit]\nDescription=tidytwin scheduled scan\n\n[Service]\nType=oneshot\nNice=10\nIOSchedulingClass=idle\nExecStart={exec}\n"))
            .map_err(|e| e.to_string())?;
        std::fs::write(dir.join("tidytwin-scan.timer"), format!("[Unit]\nDescription=tidytwin {freq} scan\n\n[Timer]\nOnCalendar={freq}\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n"))
            .map_err(|e| e.to_string())?;
        sh("systemctl", &["--user", "daemon-reload"])?;
        sh("systemctl", &["--user", "enable", "--now", "tidytwin-scan.timer"])?;
        Ok(format!("scheduled {freq} via systemd user timer; reports in {}", reports_dir().display()))
    } else if cfg!(windows) {
        let sc = match freq {
            "hourly" => "HOURLY",
            "daily" => "DAILY",
            _ => "WEEKLY",
        };
        let tr = cmd.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(" ");
        sh("schtasks", &["/Create", "/F", "/SC", sc, "/TN", "tidytwin-scan", "/TR", &tr])?;
        Ok(format!("scheduled {freq} via Task Scheduler; reports in {}", reports_dir().display()))
    } else {
        // ponytail: macOS gets a crontab line to paste; launchd plist if mac users ask.
        let when = match freq {
            "hourly" => "0 * * * *",
            "daily" => "0 3 * * *",
            _ => "0 3 * * 0",
        };
        let line = cmd.iter().map(|s| format!("'{}'", s.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ");
        Ok(format!("add to `crontab -e`:\n{when} {line}"))
    }
}

pub fn unschedule() -> Result<String, String> {
    if cfg!(target_os = "linux") {
        let _ = sh("systemctl", &["--user", "disable", "--now", "tidytwin-scan.timer"]);
        // Same dir schedule() wrote to (honours XDG_CONFIG_HOME).
        let dir = systemd_user_dir()?;
        for f in ["tidytwin-scan.service", "tidytwin-scan.timer"] {
            let _ = std::fs::remove_file(dir.join(f));
        }
        let _ = sh("systemctl", &["--user", "daemon-reload"]);
        Ok("schedule removed".into())
    } else if cfg!(windows) {
        sh("schtasks", &["/Delete", "/F", "/TN", "tidytwin-scan"]).map(|_| "schedule removed".into())
    } else {
        Ok("remove the tidytwin line from `crontab -e`".into())
    }
}

fn systemd_user_dir() -> Result<PathBuf, String> {
    Ok(std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or("no HOME")?
        .join("systemd/user"))
}

/// Quote one ExecStart= word: C-style escapes, `%%` for specifiers, `$$` against env expansion.
fn systemd_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$"))
}

fn sh(cmd: &str, args: &[&str]) -> Result<(), String> {
    let o = std::process::Command::new(cmd).args(args).output().map_err(|e| format!("{cmd}: {e}"))?;
    if o.status.success() { Ok(()) } else { Err(format!("{cmd} failed: {}", String::from_utf8_lossy(&o.stderr))) }
}

/// GUI → CLI args for scheduling the scan currently configured.
pub fn to_args(o: &ScanOpts, c: Criteria, cross: bool) -> Vec<String> {
    let m: Vec<&str> = [(c.name, "name"), (c.size, "size"), (c.mtime, "mtime"), (c.content, "content")].iter().filter(|x| x.0).map(|x| x.1).collect();
    let mut a = vec!["--match".into(), m.join(","), "--min-size".into(), o.min_size.to_string()];
    for e in o.exclude.iter().filter(|e| !e.is_empty()) {
        a.push("--exclude".into());
        a.push(e.clone());
    }
    if o.follow_hidden {
        a.push("--hidden".into());
    }
    if cross {
        a.push("--cross".into());
    }
    a.extend(o.roots.iter().map(|r| r.to_string_lossy().into_owned()));
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_roundtrip() {
        let o = ScanOpts { roots: vec![std::env::temp_dir()], min_size: 5, exclude: vec!["node_modules".into(), "".into()], follow_hidden: true };
        let c = Criteria { name: true, size: false, mtime: false, content: true };
        let a = parse(&to_args(&o, c, true)).unwrap();
        assert_eq!(a.crit, c);
        assert_eq!((a.opts.min_size, a.opts.exclude.len(), a.opts.follow_hidden, a.cross), (5, 1, true, true));
        assert!(parse(&["--bogus".into()]).is_err());
    }

    #[test]
    fn systemd_quoting() {
        assert_eq!(systemd_quote(r#"/a $HOME/50%"q"\"#), r#""/a $$HOME/50%%\"q\"\\""#);
        assert!(schedule("daily", &["/x\nExecStartPre=/bin/evil".into()]).is_err());
    }
}
